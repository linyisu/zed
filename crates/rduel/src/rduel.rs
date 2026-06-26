use std::{
    any::TypeId,
    collections::HashMap,
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Context as _;
use editor::{
    Editor, EditorMode, MultiBuffer, SizingBehavior,
    actions::{ConfirmRename, Rename},
};
use gpui::{
    Action, AnyElement, App, ClipboardItem, Context, DismissEvent, DragMoveEvent, Empty, Entity,
    EntityId, EventEmitter, FocusHandle, Focusable, ImageSource, MouseButton, MouseDownEvent,
    MouseUpEvent, Render, Resource, ScrollHandle, SharedString, SharedUri, Subscription,
    WeakEntity, Window, div, px,
};
use language::{Buffer, LanguageRegistry};
use markdown::{
    CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownFont,
    MarkdownOptions, MarkdownStyle, WrapButtonVisibility,
};
use menu::{Cancel, Confirm};
use project::{Project, ProjectItem, ProjectPath};
use schemars::JsonSchema;
use search::BufferSearchBar;
use serde::{Deserialize, Serialize};
use settings::{RegisterSetting, Settings};
use text::{LineEnding, Rope};
use ui::{Button, ButtonSize, ButtonStyle, prelude::*};
use util::{ResultExt, rel_path::RelPath};
use workspace::{
    Item, ModalView, ToolbarItemEvent, ToolbarItemView, Workspace,
    item::{ItemBufferKind, ItemEvent, SaveOptions},
};
use zed_actions::rduel::{OpenRduel, OpenRduelHistory};

const DEFAULT_PROBLEM_WIDTH_FRACTION: f32 = 0.42;
const MIN_PROBLEM_WIDTH_FRACTION: f32 = 0.25;
const MAX_PROBLEM_WIDTH_FRACTION: f32 = 0.75;
const DEFAULT_COMMAND_OUTPUT_HEIGHT: f32 = 280.0;
/// Floor for the output panel: enough to keep its toolbar (run/submit) usable.
const MIN_COMMAND_OUTPUT_HEIGHT: f32 = 28.0;
/// Space kept above the output at full height so the code tab bar stays visible.
const CODE_AREA_TOP_RESERVE: f32 = 50.0;
const PROBLEM_MARKDOWN_FONT_SCALE: f32 = 1.12;
const DEFAULT_RDUEL_SERVER_URL: &str = "http://127.0.0.1:8787";
const SAMPLE_TEST_TIMEOUT: Duration = Duration::from_secs(3);
/// How long the "opponent submitted" banner stays visible.
const OPPONENT_FLASH_DURATION: Duration = Duration::from_secs(5);
/// Interval for automatic code snapshot uploads during a match.
const CODE_SNAPSHOT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, RegisterSetting)]
pub struct RduelSettings {
    pub atcoder_user: String,
    pub server_url: String,
}

impl Settings for RduelSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let rduel = content.rduel.as_ref();
        let local_config = LocalRduelConfig::load();
        Self {
            atcoder_user: rduel
                .and_then(|settings| settings.atcoder_user.clone())
                .filter(|atcoder_user| !atcoder_user.trim().is_empty())
                .or_else(|| {
                    local_config
                        .as_ref()
                        .and_then(|config| config.atcoder_user.clone())
                })
                .unwrap_or_default(),
            server_url: rduel
                .and_then(|settings| settings.server_url.clone())
                .filter(|server_url| !server_url.trim().is_empty())
                .or_else(|| local_config.and_then(|config| config.server_url))
                .unwrap_or_else(|| DEFAULT_RDUEL_SERVER_URL.to_string()),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct LocalRduelConfig {
    atcoder_user: Option<String>,
    server_url: Option<String>,
}

impl LocalRduelConfig {
    fn path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
        Some(PathBuf::from(home).join(".rduel").join("config.json"))
    }

    fn load() -> Option<Self> {
        let path = Self::path()?;
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn save_atcoder_user_and_server_url(
        atcoder_user: &str,
        server_url: &str,
    ) -> anyhow::Result<()> {
        let path = Self::path().ok_or_else(|| anyhow::anyhow!("could not resolve ~/.rduel"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut config = Self::load().unwrap_or_default();
        config.atcoder_user = Some(atcoder_user.to_string());
        config.server_url = Some(server_url.to_string());
        std::fs::write(path, serde_json::to_string_pretty(&config)?)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = rduel)]
pub struct RunSamples;

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = rduel)]
pub struct SubmitSolution;

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = rduel)]
pub struct ToggleLayout;

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = rduel)]
pub struct SelectMainRs;

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = rduel)]
pub struct SelectCargoToml;

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = rduel)]
pub struct SelectOpponentMainRs;

pub fn init(cx: &mut App) {
    RduelSettings::register(cx);
    cx.observe_new(|workspace: &mut Workspace, _window, cx| {
        cleanup_legacy_rduel_worktree(workspace, cx);
        workspace.register_action(|workspace, _: &OpenRduel, window, cx| {
            open_rduel(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &OpenRduelHistory, window, cx| {
            open_rduel_history(workspace, window, cx);
        });
    })
    .detach();
}

fn open_rduel(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    cleanup_legacy_rduel_worktree(workspace, cx);
    let workspace_handle = cx.entity().downgrade();
    workspace.toggle_modal(window, cx, |window, cx| {
        RduelMatchModal::new(workspace_handle, window, cx)
    });
}

fn open_rduel_history(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let workspace_handle = cx.entity().downgrade();
    workspace.toggle_modal(window, cx, |_, cx| {
        RduelHistoryModal::new(workspace_handle, cx)
    });
}

fn cleanup_legacy_rduel_worktree(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) else {
        return;
    };
    let rduel_path = PathBuf::from(home).join(".rduel");
    let legacy_problem_path = rduel_path.join("abc001").join("a");
    let legacy_contest_path = rduel_path.join("abc001");
    workspace.project().update(cx, |project, cx| {
        project.remove_worktree_for_main_worktree_path(&legacy_problem_path, cx);
        project.remove_worktree_for_main_worktree_path(&legacy_contest_path, cx);
    });
}

/// Helper to hide modal and execute a callback. Reduces boilerplate.
fn hide_modal_and_then<F>(
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
    callback: F,
) where
    F: FnOnce(WeakEntity<Workspace>, &mut Window, &mut App) + 'static,
{
    let window_handle = window.window_handle();
    cx.defer(move |cx| {
        window_handle
            .update(cx, |_, window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.hide_modal(window, cx);
                    })
                    .log_err();
                callback(workspace, window, cx);
            })
            .log_err();
    });
}

/// Helper to just hide modal without additional action.
fn hide_modal(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut App) {
    let window_handle = window.window_handle();
    cx.defer(move |cx| {
        window_handle
            .update(cx, |_, window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.hide_modal(window, cx);
                    })
                    .log_err();
            })
            .log_err();
    });
}

fn open_rduel_session(
    workspace: WeakEntity<Workspace>,
    session: RduelSession,
    window: &mut Window,
    cx: &mut App,
) {
    let fs = match workspace
        .read_with(cx, |workspace, _| workspace.app_state().fs.clone())
        .log_err()
    {
        Some(fs) => fs,
        None => return,
    };
    let project = match workspace
        .read_with(cx, |workspace, _cx| workspace.project().clone())
        .log_err()
    {
        Some(project) => project,
        None => return,
    };
    let language_registry = project.read(cx).languages().clone();
    window
        .spawn(cx, async move |cx| {
            let rduel_project = match RduelProjectFiles::for_current_user() {
                Ok(rduel_project) => match prepare_rduel_project(fs, rduel_project).await {
                    Ok(rduel_project) => Some(rduel_project),
                    Err(error) => {
                        log::error!("failed to prepare Rduel Rust project: {error:#}");
                        None
                    }
                },
                Err(error) => {
                    log::error!("failed to resolve Rduel root directory: {error:#}");
                    None
                }
            };

            if let (Some(rduel_project), Some(room)) =
                (rduel_project.as_ref(), session.room.as_ref())
                && let Err(error) = write_server_problem_to_project(rduel_project, &room.problem)
            {
                log::error!("failed to write Rduel server problem: {error:#}");
            }
            if let Some(rduel_project) = rduel_project.as_ref() {
                if let Some(main_rs) = session.initial_main_rs.as_deref()
                    && let Err(error) = std::fs::write(&rduel_project.problem_rs_path, main_rs)
                {
                    log::error!("failed to restore Rduel history main.rs: {error:#}");
                }
                if let Some(cargo_toml) = session.initial_cargo_toml.as_deref() {
                    let cargo_toml = session
                        .room
                        .as_ref()
                        .map(|room| replace_problem_url(cargo_toml, &room.problem.url))
                        .unwrap_or_else(|| cargo_toml.to_string());
                    if let Err(error) = std::fs::write(&rduel_project.cargo_toml_path, cargo_toml) {
                        log::error!("failed to restore Rduel history Cargo.toml: {error:#}");
                    }
                }
            }

            let main_rs_buffer = if let Some(rduel_project) = rduel_project.as_ref() {
                open_rduel_project_buffer(workspace.clone(), rduel_project, "src/main.rs", cx).await
            } else {
                None
            };
            let cargo_toml_buffer = if let Some(rduel_project) = rduel_project.as_ref() {
                open_rduel_project_buffer(workspace.clone(), rduel_project, "Cargo.toml", cx).await
            } else {
                None
            };
            let fallback_rust = language_registry.language_for_name("Rust").await.log_err();
            let fallback_toml = language_registry.language_for_name("TOML").await.log_err();

            let session_for_view = session.clone();
            let workspace_for_view = workspace.clone();
            let Some(rduel) = workspace
                .update_in(cx, |workspace, window, cx| {
                    let project = workspace.project().clone();
                    let main_rs_buffer = main_rs_buffer.unwrap_or_else(|| {
                        let buffer = cx.new(|cx| Buffer::local("", cx));
                        buffer.update(cx, |buffer, cx| {
                            if let Some(language) = fallback_rust {
                                buffer.set_language(Some(language), cx);
                            }
                            buffer.edit([(0..0, STARTER_CODE.to_string())], None, cx);
                        });
                        buffer
                    });
                    let cargo_toml_buffer = cargo_toml_buffer.unwrap_or_else(|| {
                        let buffer = cx.new(|cx| Buffer::local("", cx));
                        buffer.update(cx, |buffer, cx| {
                            if let Some(language) = fallback_toml {
                                buffer.set_language(Some(language), cx);
                            }
                            buffer.edit([(0..0, STARTER_ACR_PROBLEM_TOML.to_string())], None, cx);
                        });
                        buffer
                    });
                    Some(cx.new(|cx| {
                        let view = RduelView::new(
                            workspace_for_view,
                            project,
                            rduel_project.clone(),
                            language_registry,
                            session_for_view,
                            main_rs_buffer,
                            cargo_toml_buffer,
                            window,
                            cx,
                        );
                        view
                    }))
                })
                .log_err()
                .flatten()
            else {
                return;
            };

            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.add_item_to_active_pane(Box::new(rduel), None, true, window, cx);
                })
                .log_err();
        })
        .detach();
}

#[derive(Clone)]
struct RduelProjectFiles {
    root_path: PathBuf,
    problem_rs_path: PathBuf,
    problem_markdown_path: PathBuf,
    cargo_toml_path: PathBuf,
    test_path: PathBuf,
    cargo_config_path: PathBuf,
    target_path: PathBuf,
}

impl RduelProjectFiles {
    fn for_current_user() -> anyhow::Result<Self> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("could not determine current user home directory"))?;
        let root_path = home.join(".rduel");
        let target_path = home.join(".cache").join("rduel").join("target");
        Ok(Self {
            problem_rs_path: root_path.join("src").join("main.rs"),
            problem_markdown_path: root_path.join("problem.md"),
            cargo_toml_path: root_path.join("Cargo.toml"),
            test_path: root_path.join("tests"),
            cargo_config_path: root_path.join(".cargo").join("config.toml"),
            target_path,
            root_path,
        })
    }
}

async fn prepare_rduel_project(
    fs: Arc<dyn fs::Fs>,
    rduel_project: RduelProjectFiles,
) -> anyhow::Result<RduelProjectFiles> {
    let src_path = rduel_project.root_path.join("src");
    let cargo_config_dir = rduel_project.root_path.join(".cargo");
    let Some(target_parent) = rduel_project.target_path.parent() else {
        anyhow::bail!(
            "Rduel target path has no parent: {}",
            rduel_project.target_path.display()
        );
    };
    fs.create_dir(&rduel_project.root_path).await?;
    fs.create_dir(&src_path).await?;
    fs.create_dir(&cargo_config_dir).await?;
    fs.create_dir(&rduel_project.test_path).await?;
    fs.create_dir(target_parent).await?;
    save_if_missing(&fs, &rduel_project.root_path.join(".acr"), "").await?;
    save_if_missing(
        &fs,
        &rduel_project.cargo_toml_path,
        STARTER_ACR_PROBLEM_TOML,
    )
    .await?;
    fs.save(
        &rduel_project.cargo_config_path,
        &Rope::from(cargo_config_toml(&rduel_project.target_path)),
        LineEnding::Unix,
    )
    .await?;
    fs.save(
        &rduel_project.problem_rs_path,
        &Rope::from(STARTER_CODE),
        LineEnding::Unix,
    )
    .await?;
    Ok(rduel_project)
}

fn cargo_config_toml(target_path: &Path) -> String {
    let target_path = target_path.to_string_lossy().replace('\\', "\\\\");
    format!(
        r#"[build]
target-dir = "{target_path}"
"#
    )
}

async fn save_if_missing(fs: &Arc<dyn fs::Fs>, path: &Path, text: &str) -> anyhow::Result<()> {
    if !fs.is_file(path).await {
        fs.save(path, &Rope::from(text), LineEnding::Unix).await?;
    }
    Ok(())
}

async fn open_rduel_project_buffer(
    workspace: WeakEntity<Workspace>,
    rduel_project: &RduelProjectFiles,
    relative_path: &'static str,
    cx: &mut gpui::AsyncWindowContext,
) -> Option<Entity<Buffer>> {
    let relative_path = RelPath::unix(relative_path).map(Arc::from).log_err()?;
    let worktree = match workspace
        .update(cx, |workspace, cx| {
            workspace.project().update(cx, |project, cx| {
                project.find_or_create_worktree(&rduel_project.root_path, false, cx)
            })
        })
        .log_err()
    {
        Some(worktree) => worktree.await.log_err(),
        None => None,
    };

    let (worktree, _) = worktree?;
    let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
    match workspace
        .update(cx, |workspace, cx| {
            workspace.project().update(cx, |project, cx| {
                project.open_buffer((worktree_id, relative_path), cx)
            })
        })
        .log_err()
    {
        Some(buffer) => buffer.await.log_err(),
        None => None,
    }
}

struct RduelView {
    focus_handle: FocusHandle,
    project: Entity<Project>,
    language_registry: Arc<LanguageRegistry>,
    room: RoomState,
    problem: RduelProblem,
    rduel_project: Option<RduelProjectFiles>,
    problem_markdown: Entity<Markdown>,
    main_rs_buffer: Entity<Buffer>,
    cargo_toml_buffer: Entity<Buffer>,
    main_rs_editor: Entity<Editor>,
    cargo_toml_editor: Entity<Editor>,
    opponent_main_rs_editor: Option<Entity<Editor>>,
    workspace: WeakEntity<Workspace>,
    search_target_editor: Option<Entity<Editor>>,
    search_bar_subscriptions: Option<(EntityId, Vec<Subscription>)>,
    active_code_tab: ActiveCodeTab,
    layout_order: LayoutOrder,
    problem_width_fraction: f32,
    command_output_height: f32,
    problem_scroll_handle: ScrollHandle,
    command_output: CommandOutputState,
    command_status: CommandStatus,
    test_cases: Vec<RduelTestCase>,
    output_selection: OutputSelection,
    presence: Option<RoomPresence>,
    opponent_flash: Option<OpponentFlash>,
    last_snapshot_upload: Option<Instant>,
    submission_watch_started_at: Option<i64>,
}

/// A live snapshot of both players used to render the versus header.
#[derive(Clone)]
struct RoomPresence {
    started_at_second: i64,
    local: Option<PlayerPresence>,
    opponent: Option<PlayerPresence>,
}

#[derive(Clone)]
struct PlayerPresence {
    name: String,
    activity: ServerPlayerActivity,
}

/// A transient banner shown when the opponent makes a new submission.
#[derive(Clone)]
struct OpponentFlash {
    message: SharedString,
    shown_at: Instant,
}

impl RoomPresence {
    fn from_room(
        room: &ServerRoom,
        local_player_id: Option<&str>,
        local_atcoder_user: Option<&str>,
    ) -> Self {
        let local_index = room
            .players
            .iter()
            .position(|player| Some(player.id.as_str()) == local_player_id)
            .or_else(|| unique_player_index_by_name(room, local_atcoder_user));
        let mut local = None;
        let mut opponent = None;
        for (index, player) in room.players.iter().enumerate() {
            let presence = PlayerPresence {
                name: player.name.clone(),
                activity: room
                    .player_activity
                    .get(&player.id)
                    .cloned()
                    .unwrap_or_default(),
            };
            if Some(index) == local_index {
                local = Some(presence);
            } else {
                opponent = Some(presence);
            }
        }
        Self {
            started_at_second: room.started_at_second,
            local,
            opponent,
        }
    }
}

fn unique_player_index_by_name(room: &ServerRoom, atcoder_user: Option<&str>) -> Option<usize> {
    let atcoder_user = atcoder_user?.trim();
    if atcoder_user.is_empty() {
        return None;
    }

    let mut matching_indices = room
        .players
        .iter()
        .enumerate()
        .filter_map(|(index, player)| (player.name == atcoder_user).then_some(index));
    let index = matching_indices.next()?;
    matching_indices.next().is_none().then_some(index)
}

fn resolve_local_player_id(
    room: &ServerRoom,
    player_id: String,
    local_atcoder_user: Option<&str>,
) -> String {
    if room.players.iter().any(|player| player.id == player_id) {
        return player_id;
    }

    if let Some(index) = unique_player_index_by_name(room, local_atcoder_user) {
        let resolved_player_id = room.players[index].id.clone();
        log::warn!(
            "Rduel room {} returned local player id {} which is not in the room; recovered as {} by AtCoder user {}",
            room.id,
            player_id,
            resolved_player_id,
            room.players[index].name
        );
        return resolved_player_id;
    }

    log::warn!(
        "Rduel room {} returned local player id {} which is not in the room",
        room.id,
        player_id
    );
    player_id
}

#[derive(Clone)]
struct CommandOutputState {
    items: Vec<CommandOutputItem>,
}

#[derive(Clone)]
struct CommandOutputItem {
    label: SharedString,
    status: CommandOutputItemStatus,
    detail: Option<CommandOutputDetail>,
}

#[derive(Clone)]
struct CommandOutputDetail {
    heading: SharedString,
    diff: Option<OutputDiff>,
    sections: Vec<CommandOutputDetailSection>,
}

/// Expected vs. actual program output, rendered as a git-style line diff.
#[derive(Clone)]
struct OutputDiff {
    expected: SharedString,
    actual: SharedString,
}

#[derive(Clone)]
struct CommandOutputDetailSection {
    title: SharedString,
    body: SharedString,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CommandOutputItemStatus {
    Pending,
    Passed,
    Warning,
    Failed,
}

/// An editable test case: input/expected are live editors so the user can
/// select, copy, and edit them; `default` keeps the provided values for restore.
struct RduelTestCase {
    input: Entity<Editor>,
    expected: Entity<Editor>,
    default: Option<RduelSample>,
    result: Option<CaseResult>,
}

/// The most recent run outcome for a single test case.
#[derive(Clone)]
struct CaseResult {
    status: CommandOutputItemStatus,
    heading: SharedString,
    actual: Option<String>,
    stderr: Option<String>,
}

/// Which chip in the output toolbar is selected: a run step or a test case.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputSelection {
    Step(usize),
    Case(usize),
}

impl CommandOutputState {
    fn empty() -> Self {
        Self { items: Vec::new() }
    }

    fn running(label: &'static str) -> Self {
        Self {
            items: vec![CommandOutputItem {
                label: label.into(),
                status: CommandOutputItemStatus::Pending,
                detail: Some(CommandOutputDetail::new("Waiting for command output...")),
            }],
        }
    }
}

fn single_command_output_state(
    label: impl Into<SharedString>,
    status: CommandOutputItemStatus,
    detail: impl Into<SharedString>,
) -> CommandOutputState {
    CommandOutputState {
        items: vec![CommandOutputItem {
            label: label.into(),
            status,
            detail: Some(CommandOutputDetail::new(detail)),
        }],
    }
}

fn submission_output_item(submission: &ServerPlayerSubmissionRecord) -> CommandOutputItem {
    let verdict = if submission.verdict.trim().is_empty() {
        "Unknown"
    } else {
        submission.verdict.trim()
    };
    let status = if verdict == "AC" {
        CommandOutputItemStatus::Passed
    } else {
        CommandOutputItemStatus::Warning
    };
    CommandOutputItem {
        label: "Submission".into(),
        status,
        detail: Some(CommandOutputDetail::with_sections(
            format!("Detected AtCoder submission: {verdict}"),
            vec![CommandOutputDetailSection {
                title: "Submission".into(),
                body: format!(
                    "{verdict:<8} {}  #{}",
                    format_relative(submission.epoch_second),
                    submission.id
                )
                .into(),
            }],
        )),
    }
}

impl CommandOutputDetail {
    fn new(heading: impl Into<SharedString>) -> Self {
        Self {
            heading: heading.into(),
            diff: None,
            sections: Vec::new(),
        }
    }

    fn with_sections(
        heading: impl Into<SharedString>,
        sections: Vec<CommandOutputDetailSection>,
    ) -> Self {
        Self {
            heading: heading.into(),
            diff: None,
            sections,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActiveCodeTab {
    MainRs,
    CargoToml,
    OpponentMainRs,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LayoutOrder {
    ProblemLeft,
    CodeLeft,
}

#[derive(Clone)]
struct DraggedRduelDivider;

impl Render for DraggedRduelDivider {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

#[derive(Clone)]
struct DraggedRduelOutputDivider;

impl Render for DraggedRduelOutputDivider {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

struct RoomState {
    match_state: MatchState,
}

struct RduelProblem {
    id: SharedString,
    title: SharedString,
    markdown: SharedString,
    samples: Vec<RduelSample>,
}

#[derive(Clone)]
struct RduelSample {
    input: String,
    output: String,
}

impl RduelProblem {
    fn from_server(problem: &ServerProblem) -> Self {
        Self {
            id: problem.id.clone().into(),
            title: problem.title.clone().into(),
            markdown: problem.statement_markdown.clone().into(),
            samples: problem
                .samples
                .iter()
                .map(|sample| RduelSample {
                    input: sample.input.clone(),
                    output: sample.output.clone(),
                })
                .collect(),
        }
    }
}

#[derive(Clone)]
struct RduelSession {
    player_id: String,
    token: String,
    room: Option<ServerRoom>,
    server_url: String,
    initial_main_rs: Option<String>,
    initial_cargo_toml: Option<String>,
    opponent_main_rs: Option<(String, String)>,
    initial_command_output: Option<CommandOutputState>,
    is_history: bool,
}

#[derive(Clone)]
struct MatchState {
    player_id: Option<String>,
    token: Option<String>,
    room_id: Option<String>,
    server_url: String,
    local_atcoder_user: Option<String>,
    room_status: ServerRoomStatus,
}

#[derive(Serialize)]
struct JoinRequest {
    name: String,
    player_id: Option<String>,
}

#[derive(Serialize)]
struct LeaveRequest {
    token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    main_rs: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cargo_toml: Option<String>,
}

#[derive(Serialize)]
struct CodeSnapshotRequest {
    player_id: String,
    token: String,
    main_rs: String,
    cargo_toml: String,
}

#[derive(Serialize)]
struct SubmissionCheckRequest {
    player_id: String,
    token: String,
    from_second: i64,
}

#[derive(Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum JoinResponse {
    Waiting {
        player_id: String,
        token: String,
    },
    Matched {
        player_id: String,
        token: String,
        room: ServerRoom,
    },
}

#[derive(Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum PlayerStateResponse {
    Waiting { player_id: String },
    Matched { player_id: String, room: ServerRoom },
}

#[derive(Clone, Deserialize)]
struct ServerRoom {
    id: String,
    players: [ServerPlayer; 2],
    problem: ServerProblem,
    status: ServerRoomStatus,
    winner_player_id: Option<String>,
    finish_reason: Option<ServerRoomFinishReason>,
    #[serde(default)]
    winning_submission: Option<ServerWinningSubmission>,
    #[serde(default)]
    started_at_second: i64,
    #[serde(default)]
    player_activity: std::collections::HashMap<String, ServerPlayerActivity>,
}

#[derive(Clone, Deserialize)]
struct ServerWinningSubmission {
    player_id: String,
    atcoder_user: String,
    #[serde(default)]
    source_code: Option<String>,
}

#[derive(Clone, Default, Deserialize)]
struct ServerPlayerActivity {
    #[serde(default)]
    attempt_count: u32,
    #[serde(default)]
    last_verdict: Option<String>,
    #[serde(default)]
    last_submission_epoch: Option<i64>,
}

#[derive(Clone, Deserialize)]
struct ServerPlayerSubmissionRecord {
    id: i64,
    epoch_second: i64,
    verdict: String,
}

#[derive(Deserialize)]
struct ServerSubmissionCheckResponse {
    room: ServerRoom,
    detected_submission: Option<ServerPlayerSubmissionRecord>,
}

#[derive(Clone, Deserialize)]
struct ServerPlayer {
    id: String,
    name: String,
}

#[derive(Clone, Deserialize)]
struct MatchHistoryResponse {
    matches: Vec<MatchHistoryEntry>,
}

#[derive(Clone, Deserialize)]
struct MatchHistoryEntry {
    room_id: String,
    problem_id: String,
    problem_title: String,
    problem: ServerProblem,
    started_at_second: i64,
    finished_at_second: Option<i64>,
    finish_reason: Option<String>,
    winner_player_id: Option<String>,
    players: [MatchHistoryPlayer; 2],
}

#[derive(Clone, Deserialize)]
struct MatchHistoryPlayer {
    player_id: String,
    atcoder_user: String,
    attempt_count: u32,
    last_verdict: Option<String>,
    last_submission_epoch: Option<i64>,
    #[serde(default)]
    submissions: Vec<MatchHistorySubmission>,
    main_rs: Option<String>,
    cargo_toml: Option<String>,
}

#[derive(Clone, Deserialize)]
struct MatchHistorySubmission {
    id: i64,
    epoch_second: i64,
    verdict: String,
}

#[derive(Clone, Deserialize)]
struct ServerProblem {
    id: String,
    title: String,
    url: String,
    statement_markdown: String,
    samples: Vec<ServerSample>,
}

#[derive(Clone, Deserialize)]
struct ServerSample {
    input: String,
    output: String,
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ServerRoomStatus {
    Playing,
    Finished,
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ServerRoomFinishReason {
    Accepted,
    ManualComplete,
    PlayerLeft,
}

enum RduelMatchCommand {
    Join {
        server_url: String,
        player_id: Option<String>,
        name: String,
    },
    Poll {
        server_url: String,
        player_id: String,
    },
    PollRoom {
        server_url: String,
        room_id: String,
    },
    WatchSubmissions {
        server_url: String,
        room_id: String,
        player_id: String,
        token: String,
        from_second: i64,
    },
    UploadCodeSnapshot {
        server_url: String,
        room_id: String,
        player_id: String,
        token: String,
        main_rs: String,
        cargo_toml: String,
    },
    History {
        server_url: String,
        atcoder_user: Option<String>,
    },
    Leave {
        server_url: String,
        player_id: String,
        token: String,
        main_rs: Option<String>,
        cargo_toml: Option<String>,
    },
}

enum RduelMatchOutput {
    Waiting {
        player_id: String,
        token: Option<String>,
    },
    Matched {
        player_id: String,
        token: Option<String>,
        room: ServerRoom,
    },
    RoomStatus {
        room: ServerRoom,
    },
    SubmissionCheck {
        room: ServerRoom,
        detected_submission: Option<ServerPlayerSubmissionRecord>,
    },
    History {
        matches: Vec<MatchHistoryEntry>,
    },
}

#[derive(Clone, Copy)]
enum CommandStatus {
    Idle,
    Running,
    Succeeded,
    Failed,
}

impl CommandStatus {
    fn is_running(self) -> bool {
        matches!(self, Self::Running)
    }
}

struct RduelMatchModal {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    atcoder_user_editor: Entity<Editor>,
    server_url: String,
    player_id: Option<String>,
    token: Option<String>,
    status: SharedString,
    is_waiting: bool,
}

impl RduelMatchModal {
    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = RduelSettings::get_global(cx);
        let editor_atcoder_user = settings.atcoder_user.trim().to_string();
        let configured_server_url = settings.server_url.trim().to_string();
        let atcoder_user_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("AtCoder username", window, cx);
            if !editor_atcoder_user.is_empty() {
                editor.set_text(editor_atcoder_user, window, cx);
                editor.select_all(&editor::actions::SelectAll, window, cx);
            }
            editor
        });
        window.focus(&atcoder_user_editor.read(cx).focus_handle(cx), cx);

        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            atcoder_user_editor,
            server_url: configured_server_url,
            player_id: None,
            token: None,
            status: "输入 AtCoder 用户名后开始匹配。".into(),
            is_waiting: false,
        }
    }

    fn join(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let atcoder_user = self
            .atcoder_user_editor
            .read(cx)
            .text(cx)
            .trim()
            .to_string();
        if atcoder_user.is_empty() {
            self.status = "需要先输入 AtCoder 用户名。".into();
            cx.notify();
            return;
        }

        let server_url = self.server_url.clone();
        let atcoder_user_for_config = atcoder_user.clone();
        cx.background_spawn(async move {
            LocalRduelConfig::save_atcoder_user_and_server_url(
                &atcoder_user_for_config,
                &server_url,
            )
        })
        .detach_and_log_err(cx);

        self.status = "正在匹配对手...".into();
        self.is_waiting = true;
        let server_url = self.server_url.clone();
        let player_id = self.player_id.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    RduelMatchCommand::Join {
                        server_url,
                        player_id,
                        name: atcoder_user,
                    }
                    .run()
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.handle_match_result(result, window, cx);
            })
        })
        .detach_and_log_err(cx);
    }

    fn poll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(player_id) = self.player_id.clone() else {
            return;
        };
        let server_url = self.server_url.clone();
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let result = cx
                .background_spawn(async move {
                    RduelMatchCommand::Poll {
                        server_url,
                        player_id,
                    }
                    .run()
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.handle_match_result(result, window, cx);
            })
        })
        .detach_and_log_err(cx);
    }

    fn leave_matchmaking(&mut self, cx: &mut Context<Self>) {
        if !self.is_waiting {
            self.player_id = None;
            self.token = None;
            return;
        }
        let Some(player_id) = self.player_id.take() else {
            return;
        };
        let token = self.token.take().unwrap_or_default();
        let server_url = self.server_url.clone();
        self.is_waiting = false;
        cx.background_spawn(async move {
            RduelMatchCommand::Leave {
                server_url,
                player_id,
                token,
                main_rs: None,
                cargo_toml: None,
            }
            .run()
        })
        .detach_and_log_err(cx);
    }

    fn handle_match_result(
        &mut self,
        result: anyhow::Result<RduelMatchOutput>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(RduelMatchOutput::Waiting { player_id, token }) => {
                self.player_id = Some(player_id);
                if token.is_some() {
                    self.token = token;
                }
                self.status = "已进入队列，等待对手...".into();
                self.poll(window, cx);
            }
            Ok(RduelMatchOutput::Matched {
                player_id,
                token,
                room,
            }) => {
                self.status = "匹配成功，正在打开 Rduel...".into();
                self.is_waiting = false;
                // A token only arrives on the initial join; a waiting player that
                // gets matched via polling keeps the token it received earlier.
                let token = token.or_else(|| self.token.clone()).unwrap_or_default();
                self.player_id = None;
                let session = RduelSession {
                    player_id,
                    token,
                    room: Some(room),
                    server_url: self.server_url.clone(),
                    initial_main_rs: None,
                    initial_cargo_toml: None,
                    opponent_main_rs: None,
                    initial_command_output: None,
                    is_history: false,
                };
                let workspace = self.workspace.clone();
                hide_modal_and_then(workspace, window, cx, |workspace, window, cx| {
                    open_rduel_session(workspace, session, window, cx);
                });
            }
            Ok(RduelMatchOutput::RoomStatus { .. }) => {}
            Ok(RduelMatchOutput::SubmissionCheck { .. }) => {}
            Ok(RduelMatchOutput::History { .. }) => {}
            Err(error) => {
                log::warn!("failed to match Rduel player: {error:#}");
                self.status = "连接失败，请确认 Rduel 服务器已启动后重试。".into();
                self.is_waiting = false;
            }
        }
        cx.notify();
    }
}

impl Render for RduelMatchModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("RduelMatchModal")
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(rems(32.))
            .p_4()
            .gap_3()
            .child(Label::new("Rduel").size(LabelSize::Large))
            .child(
                div()
                    .h(px(32.))
                    .flex()
                    .items_center()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .rounded_sm()
                    .px_2()
                    .child(self.atcoder_user_editor.clone()),
            )
            .child(Label::new(self.status.clone()).color(Color::Muted))
            .child(
                h_flex().justify_end().child(
                    Button::new("rduel-start-match", "Match")
                        .size(ButtonSize::Compact)
                        .style(ButtonStyle::Filled)
                        .disabled(self.is_waiting)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.join(window, cx);
                        })),
                ),
            )
            .when(self.is_waiting, |this| {
                this.child(Label::new("等待服务器匹配并准备题面...").size(LabelSize::Default))
            })
            .on_action(cx.listener(|this, _: &Confirm, window, cx| {
                if !this.is_waiting {
                    this.join(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Cancel, window, cx| {
                hide_modal(this.workspace.clone(), window, cx);
            }))
    }
}

impl Focusable for RduelMatchModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for RduelMatchModal {}
impl ModalView for RduelMatchModal {
    fn on_before_dismiss(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> workspace::DismissDecision {
        self.leave_matchmaking(cx);
        workspace::DismissDecision::Dismiss(true)
    }
}

struct RduelHistoryModal {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    server_url: String,
    atcoder_user: Option<String>,
    status: SharedString,
    matches: Vec<MatchHistoryEntry>,
    is_loading: bool,
}

impl RduelHistoryModal {
    fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let settings = RduelSettings::get_global(cx);
        let atcoder_user = settings.atcoder_user.trim();
        let mut modal = Self {
            focus_handle: cx.focus_handle(),
            workspace,
            server_url: settings.server_url.trim().to_string(),
            atcoder_user: (!atcoder_user.is_empty()).then(|| atcoder_user.to_string()),
            status: "Loading match history...".into(),
            matches: Vec::new(),
            is_loading: false,
        };
        modal.load(cx);
        modal
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        if self.is_loading {
            return;
        }
        self.is_loading = true;
        self.status = "Loading match history...".into();
        let server_url = self.server_url.clone();
        let atcoder_user = self.atcoder_user.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    RduelMatchCommand::History {
                        server_url,
                        atcoder_user,
                    }
                    .run()
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(RduelMatchOutput::History { matches }) => {
                        this.status = if matches.is_empty() {
                            "No match history found.".into()
                        } else {
                            SharedString::default()
                        };
                        this.matches = matches;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        log::warn!("failed to load Rduel history: {error:#}");
                        this.status = "Could not load match history from the server.".into();
                    }
                }
                this.is_loading = false;
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn open_history_match(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.matches.get(index).cloned() else {
            return;
        };
        let Some(session) =
            rduel_session_from_history_entry(entry, self.atcoder_user.as_deref(), &self.server_url)
        else {
            self.status = "This match history entry has no players.".into();
            cx.notify();
            return;
        };
        let workspace = self.workspace.clone();
        hide_modal_and_then(workspace, window, cx, |workspace, window, cx| {
            open_rduel_session(workspace, session, window, cx);
        });
    }

    fn render_entry(
        &self,
        index: usize,
        entry: &MatchHistoryEntry,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let outcome = history_outcome(entry, self.atcoder_user.as_deref());
        let time = entry
            .finished_at_second
            .or(Some(entry.started_at_second))
            .map(format_relative)
            .unwrap_or_else(|| "unknown time".to_string());

        let title = format!("{} {}", entry.problem_id, entry.problem_title);

        // Find local player info for additional stats
        let local_player = self.atcoder_user.as_deref().and_then(|atcoder_user| {
            entry
                .players
                .iter()
                .find(|p| p.atcoder_user == atcoder_user)
        });

        // Build stats string (attempts, duration, verdict)
        let mut stats_parts = Vec::new();

        if let Some(player) = local_player {
            if player.attempt_count > 0 {
                stats_parts.push(format!("{}次提交", player.attempt_count));
            }
            if let Some(verdict) = &player.last_verdict {
                stats_parts.push(verdict.clone());
            }
        }

        if let (Some(start), Some(end)) = (Some(entry.started_at_second), entry.finished_at_second)
        {
            let duration_sec = (end - start).max(0);
            let minutes = duration_sec / 60;
            let seconds = duration_sec % 60;
            if minutes > 0 {
                stats_parts.push(format!("{}:{:02}", minutes, seconds));
            } else {
                stats_parts.push(format!("{}秒", seconds));
            }
        }

        let stats = if stats_parts.is_empty() {
            String::new()
        } else {
            stats_parts.join(" · ")
        };

        h_flex()
            .id(("rduel-history-match", index))
            .h(px(40.))
            .gap_3()
            .items_center()
            .px_3()
            .rounded_sm()
            .bg(cx.theme().colors().ghost_element_background)
            .hover(|this| this.bg(cx.theme().colors().ghost_element_hover))
            .cursor_pointer()
            .child(Label::new(title).size(LabelSize::Default).truncate())
            .child(div().flex_1())
            .when(!stats.is_empty(), |this| {
                this.child(Label::new(stats).color(Color::Muted).size(LabelSize::Small))
            })
            .child(Label::new(time).color(Color::Muted))
            .child(Label::new(outcome).size(LabelSize::Default))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_history_match(index, window, cx);
            }))
    }
}

impl Render for RduelHistoryModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("RduelHistoryModal")
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(rems(44.))
            .max_h(rems(36.))
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .justify_between()
                    .child(Label::new("Rduel History").size(LabelSize::Large))
                    .child(
                        Button::new("rduel-history-refresh", "Refresh")
                            .size(ButtonSize::Compact)
                            .disabled(self.is_loading)
                            .on_click(cx.listener(|this, _, _, cx| this.load(cx))),
                    ),
            )
            .when(!self.status.is_empty(), |this| {
                this.child(Label::new(self.status.clone()).color(Color::Muted))
            })
            .child(
                div()
                    .id("rduel-history-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .gap_1()
                            .children(self.matches.iter().enumerate().map(|(index, entry)| {
                                self.render_entry(index, entry, cx).into_any_element()
                            })),
                    ),
            )
            .on_action(cx.listener(|this, _: &Cancel, window, cx| {
                hide_modal(this.workspace.clone(), window, cx);
            }))
    }
}

impl Focusable for RduelHistoryModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for RduelHistoryModal {}
impl ModalView for RduelHistoryModal {}

fn history_outcome(entry: &MatchHistoryEntry, atcoder_user: Option<&str>) -> SharedString {
    let Some(atcoder_user) = atcoder_user else {
        return "Finished".into();
    };
    let Some(winner_player_id) = entry.winner_player_id.as_deref() else {
        return "Finished".into();
    };
    let Some(winner) = entry
        .players
        .iter()
        .find(|player| player.player_id == winner_player_id)
    else {
        return "Finished".into();
    };
    if winner.atcoder_user == atcoder_user {
        "Win".into()
    } else {
        "Loss".into()
    }
}

fn rduel_session_from_history_entry(
    entry: MatchHistoryEntry,
    atcoder_user: Option<&str>,
    server_url: &str,
) -> Option<RduelSession> {
    let local_index = atcoder_user
        .and_then(|atcoder_user| {
            entry
                .players
                .iter()
                .position(|player| player.atcoder_user == atcoder_user)
        })
        .unwrap_or(0);
    let local_player = entry.players.get(local_index)?;
    let opponent_player = entry
        .players
        .iter()
        .enumerate()
        .find_map(|(index, player)| (index != local_index).then_some(player));
    let opponent_main_rs = opponent_player.and_then(|player| {
        player
            .main_rs
            .as_ref()
            .filter(|main_rs| !main_rs.trim().is_empty())
            .map(|main_rs| (player.atcoder_user.clone(), main_rs.clone()))
    });
    let players = entry.players.clone().map(|player| ServerPlayer {
        id: player.player_id,
        name: player.atcoder_user,
    });
    let player_activity = entry
        .players
        .iter()
        .map(|player| {
            (
                player.player_id.clone(),
                ServerPlayerActivity {
                    attempt_count: player.attempt_count,
                    last_verdict: player.last_verdict.clone(),
                    last_submission_epoch: player.last_submission_epoch,
                },
            )
        })
        .collect::<HashMap<_, _>>();
    let initial_command_output = history_submission_output(&entry);
    let room = ServerRoom {
        id: entry.room_id,
        players,
        problem: entry.problem,
        status: ServerRoomStatus::Finished,
        winner_player_id: entry.winner_player_id,
        finish_reason: entry
            .finish_reason
            .and_then(|reason| match reason.as_str() {
                "accepted" => Some(ServerRoomFinishReason::Accepted),
                "manual_complete" => Some(ServerRoomFinishReason::ManualComplete),
                "player_left" => Some(ServerRoomFinishReason::PlayerLeft),
                _ => None,
            }),
        winning_submission: None,
        started_at_second: entry.started_at_second,
        player_activity,
    };

    Some(RduelSession {
        player_id: local_player.player_id.clone(),
        token: String::new(),
        room: Some(room),
        server_url: server_url.to_string(),
        initial_main_rs: local_player.main_rs.clone(),
        initial_cargo_toml: local_player.cargo_toml.clone(),
        opponent_main_rs,
        initial_command_output,
        is_history: true,
    })
}

fn history_submission_output(entry: &MatchHistoryEntry) -> Option<CommandOutputState> {
    let mut sections = Vec::new();
    for player in &entry.players {
        if player.submissions.is_empty() {
            continue;
        }
        let body = player
            .submissions
            .iter()
            .map(|submission| {
                format!(
                    "#{} · {} · {}",
                    submission.id,
                    submission.verdict,
                    format_relative(submission.epoch_second)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(CommandOutputDetailSection {
            title: player.atcoder_user.clone().into(),
            body: body.into(),
        });
    }
    if sections.is_empty() {
        return None;
    }

    Some(CommandOutputState {
        items: vec![CommandOutputItem {
            label: "Submissions".into(),
            status: CommandOutputItemStatus::Warning,
            detail: Some(CommandOutputDetail::with_sections(
                "Recorded submissions",
                sections,
            )),
        }],
    })
}

enum RduelCommand {
    Test {
        root_path: PathBuf,
        test_path: PathBuf,
        target_path: PathBuf,
    },
    Submit {
        root_path: PathBuf,
        problem_rs_path: PathBuf,
        cargo_toml_path: PathBuf,
        test_path: PathBuf,
        target_path: PathBuf,
    },
}

struct RduelCommandOutput {
    success: bool,
    rendered: String,
    items: Vec<CommandOutputItem>,
    case_results: Vec<(usize, CaseResult)>,
    submit_ready: Option<RduelSubmitReady>,
}

struct RduelProcessOutput {
    label: &'static str,
    executable: PathBuf,
    args: Vec<String>,
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

struct RduelSubmitReady {
    source_code: String,
    source_path: PathBuf,
    submit_url: String,
}

impl RduelCommand {
    async fn run(self) -> anyhow::Result<RduelCommandOutput> {
        match self {
            Self::Test {
                root_path,
                test_path,
                target_path,
            } => run_rduel_test(root_path, test_path, target_path).await,
            Self::Submit {
                root_path,
                problem_rs_path,
                cargo_toml_path,
                test_path,
                target_path,
            } => {
                run_rduel_submit(
                    root_path,
                    problem_rs_path,
                    cargo_toml_path,
                    test_path,
                    target_path,
                )
                .await
            }
        }
    }
}

async fn run_rduel_test(
    root_path: PathBuf,
    test_path: PathBuf,
    target_path: PathBuf,
) -> anyhow::Result<RduelCommandOutput> {
    let mut steps = Vec::new();
    steps.push(
        run_process(
            "cargo build",
            &root_path,
            "cargo",
            &["build", "--release"],
            Some(&target_path),
        )
        .await?,
    );

    if !steps.last().is_some_and(|step| step.status.success()) {
        return Ok(render_embedded_test_steps(steps, Vec::new()));
    }

    let test_results = run_embedded_sample_tests(root_path, test_path, target_path).await;
    Ok(render_embedded_test_steps(steps, test_results))
}

async fn run_rduel_submit(
    root_path: PathBuf,
    problem_rs_path: PathBuf,
    cargo_toml_path: PathBuf,
    test_path: PathBuf,
    target_path: PathBuf,
) -> anyhow::Result<RduelCommandOutput> {
    let test_output = run_rduel_test(root_path, test_path, target_path).await?;
    if !test_output.success {
        if test_output.rendered.starts_with("Build failed.") {
            return Ok(RduelCommandOutput {
                success: false,
                rendered: format!(
                    "{}\n\nSubmit was stopped because the solution did not build.",
                    test_output.rendered
                ),
                items: test_output.items,
                case_results: test_output.case_results,
                submit_ready: None,
            });
        }
    }

    let source_path = problem_rs_path;
    let source_code = std::fs::read_to_string(&source_path)?;
    let problem_url = read_problem_url(&cargo_toml_path)
        .ok_or_else(|| anyhow::anyhow!("problem_url was not found in Cargo.toml"))?;
    let submit_url =
        rcontest::atcoder_submit_url(&problem_url).unwrap_or_else(|| problem_url.clone());
    Ok(RduelCommandOutput {
        success: true,
        rendered: test_output.rendered,
        items: test_output.items,
        case_results: test_output.case_results,
        submit_ready: Some(RduelSubmitReady {
            source_code,
            source_path,
            submit_url,
        }),
    })
}

fn read_problem_url(cargo_toml_path: &Path) -> Option<String> {
    let cargo_toml = std::fs::read_to_string(cargo_toml_path).ok()?;
    cargo_toml.lines().find_map(|line| {
        let line = line.trim();
        let value = line.strip_prefix("problem_url")?.trim();
        let value = value.strip_prefix('=')?.trim();
        value
            .strip_prefix('"')?
            .strip_suffix('"')
            .map(str::to_string)
    })
}

impl RduelMatchCommand {
    fn run(self) -> anyhow::Result<RduelMatchOutput> {
        match self {
            Self::Join {
                server_url,
                player_id,
                name,
            } => {
                let response: JoinResponse = rduel_http_json(
                    &server_url,
                    "POST",
                    "/join",
                    Some(&JoinRequest { name, player_id }),
                )?;
                Ok(match response {
                    JoinResponse::Waiting { player_id, token } => RduelMatchOutput::Waiting {
                        player_id,
                        token: Some(token),
                    },
                    JoinResponse::Matched {
                        player_id,
                        token,
                        room,
                    } => RduelMatchOutput::Matched {
                        player_id,
                        token: Some(token),
                        room,
                    },
                })
            }
            Self::Poll {
                server_url,
                player_id,
            } => {
                let path = format!("/players/{player_id}");
                let response: PlayerStateResponse =
                    rduel_http_json::<(), _>(&server_url, "GET", &path, None)?;
                Ok(match response {
                    PlayerStateResponse::Waiting { player_id } => RduelMatchOutput::Waiting {
                        player_id,
                        token: None,
                    },
                    PlayerStateResponse::Matched { player_id, room } => RduelMatchOutput::Matched {
                        player_id,
                        token: None,
                        room,
                    },
                })
            }
            Self::PollRoom {
                server_url,
                room_id,
            } => {
                let path = format!("/rooms/{room_id}");
                let room: ServerRoom = rduel_http_json::<(), _>(&server_url, "GET", &path, None)?;
                Ok(RduelMatchOutput::RoomStatus { room })
            }
            Self::WatchSubmissions {
                server_url,
                room_id,
                player_id,
                token,
                from_second,
            } => {
                let path = format!("/rooms/{room_id}/watch-submissions");
                let response: ServerSubmissionCheckResponse = rduel_http_json(
                    &server_url,
                    "POST",
                    &path,
                    Some(&SubmissionCheckRequest {
                        player_id,
                        token,
                        from_second,
                    }),
                )?;
                Ok(RduelMatchOutput::SubmissionCheck {
                    room: response.room,
                    detected_submission: response.detected_submission,
                })
            }
            Self::UploadCodeSnapshot {
                server_url,
                room_id,
                player_id,
                token,
                main_rs,
                cargo_toml,
            } => {
                let path = format!("/rooms/{room_id}/code-snapshot");
                let room: ServerRoom = rduel_http_json(
                    &server_url,
                    "POST",
                    &path,
                    Some(&CodeSnapshotRequest {
                        player_id,
                        token,
                        main_rs,
                        cargo_toml,
                    }),
                )?;
                Ok(RduelMatchOutput::RoomStatus { room })
            }
            Self::History {
                server_url,
                atcoder_user,
            } => {
                let path = match atcoder_user {
                    Some(atcoder_user) => format!("/history/users/{atcoder_user}?limit=50"),
                    None => "/history?limit=50".to_string(),
                };
                let response: MatchHistoryResponse =
                    rduel_http_json::<(), _>(&server_url, "GET", &path, None)?;
                Ok(RduelMatchOutput::History {
                    matches: response.matches,
                })
            }
            Self::Leave {
                server_url,
                player_id,
                token,
                main_rs,
                cargo_toml,
            } => {
                let path = format!("/players/{player_id}/leave");
                let _: serde_json::Value = rduel_http_json(
                    &server_url,
                    "POST",
                    &path,
                    Some(&LeaveRequest {
                        token,
                        main_rs,
                        cargo_toml,
                    }),
                )?;
                Ok(RduelMatchOutput::Waiting {
                    player_id,
                    token: None,
                })
            }
        }
    }
}

fn rduel_http_json<B, R>(
    server_url: &str,
    method: &str,
    path: &str,
    body: Option<&B>,
) -> anyhow::Result<R>
where
    B: Serialize,
    R: for<'de> Deserialize<'de>,
{
    rduel_http_json_with_retry(server_url, method, path, body, 2)
}

fn rduel_http_json_with_retry<B, R>(
    server_url: &str,
    method: &str,
    path: &str,
    body: Option<&B>,
    max_retries: usize,
) -> anyhow::Result<R>
where
    B: Serialize,
    R: for<'de> Deserialize<'de>,
{
    let mut last_error = None;
    for attempt in 0..=max_retries {
        match rduel_http_json_single_attempt(server_url, method, path, body) {
            Ok(result) => return Ok(result),
            Err(error) => {
                // Don't retry on client errors (4xx) or serialization errors
                let error_str = error.to_string();
                if error_str.contains("HTTP 4") || error_str.contains("JSON") {
                    return Err(error);
                }

                last_error = Some(error);
                if attempt < max_retries {
                    // Brief delay before retry (exponential backoff)
                    std::thread::sleep(Duration::from_millis(100 * (1 << attempt)));
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("HTTP request failed after retries")))
}

fn rduel_http_json_single_attempt<B, R>(
    server_url: &str,
    method: &str,
    path: &str,
    body: Option<&B>,
) -> anyhow::Result<R>
where
    B: Serialize,
    R: for<'de> Deserialize<'de>,
{
    let endpoint =
        parse_local_http_endpoint(server_url, path).context("Failed to parse server URL")?;
    let body_str = match body {
        Some(body) => serde_json::to_string(body).context("Failed to serialize request body")?,
        None => String::new(),
    };
    let request = format!(
        "{method} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        endpoint.path,
        endpoint.host_header,
        body_str.len(),
        body_str
    );

    let socket_address = (endpoint.host.as_str(), endpoint.port)
        .to_socket_addrs()
        .context("Failed to resolve server address")?
        .next()
        .ok_or_else(|| anyhow::anyhow!("Server address did not resolve to any IP"))?;

    let mut stream = TcpStream::connect_timeout(&socket_address, Duration::from_secs(5))
        .context("Failed to connect to server")?;
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .context("Failed to set read timeout")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .context("Failed to set write timeout")?;
    stream
        .write_all(request.as_bytes())
        .context("Failed to send request to server")?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .context("Failed to read response from server")?;
    let (head, body) = response.split_once("\r\n\r\n").ok_or_else(|| {
        anyhow::anyhow!("Server returned invalid HTTP response (missing header/body separator)")
    })?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| anyhow::anyhow!("Server returned invalid HTTP status line"))?;
    if !(200..300).contains(&status) {
        return Err(anyhow::anyhow!("Server returned HTTP {status}: {body}"));
    }

    serde_json::from_str(body).with_context(|| {
        format!(
            "Failed to parse server response: {}",
            body.chars().take(200).collect::<String>()
        )
    })
}

struct LocalHttpEndpoint {
    host: String,
    port: u16,
    host_header: String,
    path: String,
}

fn parse_local_http_endpoint(server_url: &str, path: &str) -> anyhow::Result<LocalHttpEndpoint> {
    let rest = server_url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("Rduel server URL must start with http://"))?;
    // Keep only the authority; drop any path/query/fragment the URL carries.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    anyhow::ensure!(!authority.is_empty(), "Rduel server URL is missing a host");

    let (host, port) = if let Some(after_bracket) = authority.strip_prefix('[') {
        // IPv6 literal: `[host]` or `[host]:port`.
        let (host, after) = after_bracket
            .split_once(']')
            .ok_or_else(|| anyhow::anyhow!("Rduel server URL has an unterminated IPv6 host"))?;
        let port = match after.strip_prefix(':') {
            Some(port) => parse_port(port)?,
            None if after.is_empty() => 80,
            None => anyhow::bail!("Rduel server URL has unexpected characters after the IPv6 host"),
        };
        (host.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), parse_port(port)?),
            None => (authority.to_string(), 80),
        }
    };
    anyhow::ensure!(!host.is_empty(), "Rduel server URL is missing a host");

    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    // IPv6 hosts must be bracketed in the HTTP Host header.
    let host_header = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };

    Ok(LocalHttpEndpoint {
        host,
        port,
        host_header,
        path,
    })
}

fn parse_port(port: &str) -> anyhow::Result<u16> {
    port.parse()
        .map_err(|_| anyhow::anyhow!("Rduel server URL has an invalid port: {port:?}"))
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// Formats the elapsed match time as `MM:SS` (or `H:MM:SS` past an hour).
fn format_elapsed(started_at_second: i64) -> String {
    let elapsed = (unix_now() - started_at_second).max(0);
    let hours = elapsed / 3600;
    let minutes = (elapsed % 3600) / 60;
    let seconds = elapsed % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

/// Formats how long ago a submission happened, in Chinese.
fn format_relative(epoch_second: i64) -> String {
    let delta = (unix_now() - epoch_second).max(0);
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;

    if delta < MINUTE {
        format!("{}秒前", delta)
    } else if delta < HOUR {
        format!("{}分钟前", delta / MINUTE)
    } else if delta < DAY {
        format!("{}小时前", delta / HOUR)
    } else if delta < WEEK {
        format!("{}天前", delta / DAY)
    } else if delta < MONTH {
        format!("{}周前", delta / WEEK)
    } else if delta < YEAR {
        format!("{}月前", delta / MONTH)
    } else {
        format!("{}年前", delta / YEAR)
    }
}

fn format_problem_heading(problem_id: &str, title: &str) -> String {
    let title = title.trim().trim_start_matches('「').trim_end_matches('」');
    if title.is_empty() {
        problem_id.to_string()
    } else {
        format!("{problem_id} 「{title}」")
    }
}

async fn run_process(
    label: &'static str,
    current_dir: &Path,
    program: &str,
    args: &[&str],
    target_path: Option<&Path>,
) -> anyhow::Result<RduelProcessOutput> {
    let executable = resolve_rduel_executable(program).ok_or_else(|| {
        anyhow::anyhow!(
            "could not find `{}` in PATH or common user bin directories. PATH={}",
            program,
            std::env::var("PATH").unwrap_or_default()
        )
    })?;
    let mut command = smol::process::Command::new(&executable);
    command.args(args).current_dir(current_dir);
    if let Some(target_path) = target_path {
        command.env("CARGO_TARGET_DIR", target_path);
    }
    let output = command.output().await?;

    Ok(RduelProcessOutput {
        label,
        executable,
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn write_server_problem_to_project(
    rduel_project: &RduelProjectFiles,
    problem: &ServerProblem,
) -> anyhow::Result<()> {
    std::fs::write(
        &rduel_project.problem_markdown_path,
        &problem.statement_markdown,
    )?;
    std::fs::create_dir_all(&rduel_project.test_path)?;

    for entry in std::fs::read_dir(&rduel_project.test_path)? {
        let entry = entry?;
        let path = entry.path();
        let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
            continue;
        };
        if extension == "in" || extension == "out" {
            std::fs::remove_file(path)?;
        }
    }

    for (index, sample) in problem.samples.iter().enumerate() {
        std::fs::write(
            rduel_project.test_path.join(format!("{index}.in")),
            &sample.input,
        )?;
        std::fs::write(
            rduel_project.test_path.join(format!("{index}.out")),
            &sample.output,
        )?;
    }

    let cargo_toml = std::fs::read_to_string(&rduel_project.cargo_toml_path)?;
    let cargo_toml = replace_problem_url(&cargo_toml, &problem.url);
    std::fs::write(&rduel_project.cargo_toml_path, cargo_toml)?;
    Ok(())
}

/// Rewrites the test directory from the current (edited/added) cases, 0-based and
/// contiguous, so `run_embedded_sample_tests` picks them up.
fn write_test_case_files(
    rduel_project: &RduelProjectFiles,
    cases: &[(String, String)],
) -> anyhow::Result<()> {
    std::fs::create_dir_all(&rduel_project.test_path)?;
    for entry in std::fs::read_dir(&rduel_project.test_path)? {
        let entry = entry?;
        let path = entry.path();
        let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
            continue;
        };
        if extension == "in" || extension == "out" {
            std::fs::remove_file(path)?;
        }
    }
    for (index, (input, expected)) in cases.iter().enumerate() {
        std::fs::write(rduel_project.test_path.join(format!("{index}.in")), input)?;
        std::fs::write(
            rduel_project.test_path.join(format!("{index}.out")),
            expected,
        )?;
    }
    Ok(())
}

fn replace_problem_url(cargo_toml: &str, problem_url: &str) -> String {
    let mut replaced = false;
    let mut output = String::new();
    for line in cargo_toml.lines() {
        if line.trim_start().starts_with("problem_url") {
            output.push_str(&format!("problem_url = \"{problem_url}\"\n"));
            replaced = true;
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }

    if !replaced {
        output.push_str("\n[package.metadata.acr]\n");
        output.push_str(&format!("problem_url = \"{problem_url}\"\n"));
    }

    output
}

fn resolve_rduel_executable(program: &str) -> Option<PathBuf> {
    let program_path = Path::new(program);
    if program_path.components().count() > 1 && program_path.is_file() {
        return Some(program_path.to_path_buf());
    }

    executable_search_paths()
        .into_iter()
        .map(|path| path.join(program))
        .find(|path| path.is_file())
}

fn executable_search_paths() -> Vec<PathBuf> {
    let mut paths = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();

    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let home = PathBuf::from(home);
        paths.push(home.join(".cargo").join("bin"));
        paths.push(home.join(".local").join("bin"));
    }

    paths.push(PathBuf::from("/usr/local/bin"));
    paths.push(PathBuf::from("/usr/bin"));

    paths
}

// Adapted from t-seki/acr (MIT): workspace/testcase.rs and runner/tester.rs.
// Rduel vendors the runner behavior so users do not need an external `acr` binary.
#[derive(Debug)]
enum EmbeddedAcrTestResult {
    Ac { actual: String },
    Wa { actual: String, expected: String },
    Re { stderr: String },
    Tle,
}

enum SampleProcessWait {
    Finished(std::io::Result<SampleProcessOutput>),
    TimedOut,
}

struct SampleProcessOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

async fn run_embedded_sample_tests(
    root_path: PathBuf,
    test_path: PathBuf,
    target_path: PathBuf,
) -> Vec<(usize, EmbeddedAcrTestResult)> {
    let mut results = Vec::new();
    let mut index = 0;

    loop {
        let input_path = test_path.join(format!("{index}.in"));
        let output_path = test_path.join(format!("{index}.out"));
        if !input_path.exists() || !output_path.exists() {
            break;
        }

        let input = match std::fs::read_to_string(&input_path) {
            Ok(input) => input,
            Err(error) => {
                results.push((
                    index,
                    EmbeddedAcrTestResult::Re {
                        stderr: format!("Failed to read {}: {error}", input_path.display()),
                    },
                ));
                index += 1;
                continue;
            }
        };
        let expected = match std::fs::read_to_string(&output_path) {
            Ok(expected) => expected,
            Err(error) => {
                results.push((
                    index,
                    EmbeddedAcrTestResult::Re {
                        stderr: format!("Failed to read {}: {error}", output_path.display()),
                    },
                ));
                index += 1;
                continue;
            }
        };

        results.push((
            index,
            run_embedded_sample_test(&root_path, &target_path, input, expected).await,
        ));
        index += 1;
    }

    results
}

async fn run_embedded_sample_test(
    root_path: &Path,
    target_path: &Path,
    input: String,
    expected: String,
) -> EmbeddedAcrTestResult {
    let Some(cargo) = resolve_rduel_executable("cargo") else {
        return EmbeddedAcrTestResult::Re {
            stderr: "could not find `cargo` in PATH or common user bin directories".into(),
        };
    };
    let mut child = match smol::process::Command::new(cargo)
        .args(["run", "--release", "-q"])
        .current_dir(root_path)
        .env("CARGO_TARGET_DIR", target_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return EmbeddedAcrTestResult::Re {
                stderr: error.to_string(),
            };
        }
    };

    use smol::io::{AsyncReadExt, AsyncWriteExt};
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let input_bytes = input.clone().into_bytes();
    // Feed stdin concurrently with draining stdout/stderr. Writing all input
    // before reading output deadlocks once a solution fills the stdout pipe
    // buffer (it blocks on write while we block on our stdin write).
    let write_input = async move {
        if let Some(stdin) = stdin.as_mut() {
            // A solution that consumes only part of its input closes the pipe
            // early, surfacing as BrokenPipe — that is not a failure of the run.
            if let Err(error) = stdin.write_all(&input_bytes).await {
                if error.kind() != std::io::ErrorKind::BrokenPipe {
                    log::debug!("failed to write Rduel sample input to stdin: {error}");
                }
            }
        }
        // Drop the handle to signal EOF to the child.
        drop(stdin);
    };
    let read_stdout = async move {
        let mut stdout_text = String::new();
        if let Some(stdout) = stdout.as_mut() {
            stdout.read_to_string(&mut stdout_text).await?;
        }
        std::io::Result::Ok(stdout_text)
    };
    let read_stderr = async move {
        let mut stderr_text = String::new();
        if let Some(stderr) = stderr.as_mut() {
            stderr.read_to_string(&mut stderr_text).await?;
        }
        std::io::Result::Ok(stderr_text)
    };

    let output = smol::future::race(
        async {
            let (status, (_, (stdout, stderr))) = smol::future::zip(
                child.status(),
                smol::future::zip(write_input, smol::future::zip(read_stdout, read_stderr)),
            )
            .await;
            SampleProcessWait::Finished(status.and_then(|status| {
                Ok(SampleProcessOutput {
                    status,
                    stdout: stdout?,
                    stderr: stderr?,
                })
            }))
        },
        async {
            smol::Timer::after(SAMPLE_TEST_TIMEOUT).await;
            SampleProcessWait::TimedOut
        },
    )
    .await;
    let output = match output {
        SampleProcessWait::Finished(output) => output,
        SampleProcessWait::TimedOut => {
            if let Err(error) = child.kill() {
                log::debug!("failed to kill timed out Rduel sample process: {error}");
            }
            return EmbeddedAcrTestResult::Tle;
        }
    };
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            return EmbeddedAcrTestResult::Re {
                stderr: error.to_string(),
            };
        }
    };

    if !output.status.success() {
        return EmbeddedAcrTestResult::Re {
            stderr: output.stderr,
        };
    }

    if output.stdout.trim_end() == expected.trim_end() {
        EmbeddedAcrTestResult::Ac {
            actual: output.stdout,
        }
    } else {
        EmbeddedAcrTestResult::Wa {
            actual: output.stdout,
            expected,
        }
    }
}

fn render_embedded_test_steps(
    steps: Vec<RduelProcessOutput>,
    results: Vec<(usize, EmbeddedAcrTestResult)>,
) -> RduelCommandOutput {
    let success = steps.iter().all(|step| step.status.success());
    let Some(build_step) = steps.iter().find(|step| step.label == "cargo build") else {
        return render_command_steps(steps);
    };
    let mut build_item = process_output_item(build_step);
    // A successful build that still emits warnings is flagged yellow rather than
    // green, without failing the run.
    if build_step.status.success() && build_step.stderr.contains("warning") {
        build_item.status = CommandOutputItemStatus::Warning;
    }

    if !build_step.status.success() {
        let mut rendered = String::from("Build failed.\n\n");
        rendered.push_str(build_step.stderr.trim());
        if !build_step.stdout.trim().is_empty() {
            rendered.push_str("\n\n[stdout]\n");
            rendered.push_str(build_step.stdout.trim());
        }
        return RduelCommandOutput {
            success,
            rendered,
            items: vec![build_item],
            case_results: Vec::new(),
            submit_ready: None,
        };
    }

    let passed = results
        .iter()
        .filter(|(_, result)| matches!(result, EmbeddedAcrTestResult::Ac { .. }))
        .count();
    let success = success && passed == results.len();

    let rendered = if results.is_empty() {
        "Build: OK\nTest: no sample cases.".to_string()
    } else if success {
        format!("Build: OK\nTest: AC ({passed} cases)")
    } else {
        let mut rendered = String::from("Build: OK\nTest: Failed\n");
        rendered.push_str(&format!("{passed}/{} cases passed\n", results.len()));
        for (index, result) in &results {
            match result {
                EmbeddedAcrTestResult::Ac { .. } => {}
                EmbeddedAcrTestResult::Wa {
                    actual, expected, ..
                } => {
                    rendered.push_str(&format!(
                        "\nCase {index}: WA\nExpected:\n{}\n\nActual:\n{}\n",
                        expected.trim_end(),
                        actual.trim_end()
                    ));
                }
                EmbeddedAcrTestResult::Re { stderr, .. } => {
                    rendered.push_str(&format!("\nCase {index}: RE\n{}\n", stderr.trim_end()));
                }
                EmbeddedAcrTestResult::Tle => {
                    rendered.push_str(&format!("\nCase {index}: TLE\n"));
                }
            }
        }
        rendered
    };

    RduelCommandOutput {
        success,
        rendered,
        items: vec![build_item],
        case_results: embedded_case_results(&results),
        submit_ready: None,
    }
}

fn render_command_steps(steps: Vec<RduelProcessOutput>) -> RduelCommandOutput {
    let success = steps.iter().all(|step| step.status.success());
    let mut rendered = String::new();

    for step in &steps {
        if !rendered.is_empty() {
            rendered.push_str("\n\n");
        }

        let code = step
            .status
            .code()
            .map_or_else(|| "signal".to_string(), |code| code.to_string());
        rendered.push_str(&format!(
            "$ {} (exit {code})\n{} {}\n",
            step.label,
            step.executable.display(),
            step.args.join(" ")
        ));

        if !step.stdout.is_empty() {
            rendered.push_str("\n[stdout]\n");
            rendered.push_str(step.stdout.trim_end());
            rendered.push('\n');
        }

        if !step.stderr.is_empty() {
            rendered.push_str("\n[stderr]\n");
            rendered.push_str(step.stderr.trim_end());
            rendered.push('\n');
        }
    }

    if rendered.is_empty() {
        rendered.push_str("No command was run.");
    }

    RduelCommandOutput {
        success,
        rendered,
        items: steps.iter().map(process_output_item).collect(),
        case_results: Vec::new(),
        submit_ready: None,
    }
}

fn embedded_case_results(results: &[(usize, EmbeddedAcrTestResult)]) -> Vec<(usize, CaseResult)> {
    results
        .iter()
        .map(|(index, result)| {
            let case = match result {
                EmbeddedAcrTestResult::Ac { actual, .. } => CaseResult {
                    status: CommandOutputItemStatus::Passed,
                    heading: "Accepted".into(),
                    actual: Some(actual.clone()),
                    stderr: None,
                },
                EmbeddedAcrTestResult::Wa { actual, .. } => CaseResult {
                    status: CommandOutputItemStatus::Failed,
                    heading: "Wrong Answer".into(),
                    actual: Some(actual.clone()),
                    stderr: None,
                },
                EmbeddedAcrTestResult::Re { stderr, .. } => CaseResult {
                    status: CommandOutputItemStatus::Failed,
                    heading: "Runtime Error".into(),
                    actual: None,
                    stderr: Some(stderr.clone()),
                },
                EmbeddedAcrTestResult::Tle => CaseResult {
                    status: CommandOutputItemStatus::Failed,
                    heading: "Time Limit Exceeded".into(),
                    actual: None,
                    stderr: None,
                },
            };
            (*index, case)
        })
        .collect()
}

fn status_dot_color(status: CommandOutputItemStatus, cx: &App) -> gpui::Hsla {
    match status {
        CommandOutputItemStatus::Pending => cx.theme().colors().border_variant,
        CommandOutputItemStatus::Passed => cx.theme().status().success,
        CommandOutputItemStatus::Warning => cx.theme().status().warning,
        CommandOutputItemStatus::Failed => cx.theme().status().error,
    }
}

fn process_output_item(step: &RduelProcessOutput) -> CommandOutputItem {
    let code = step
        .status
        .code()
        .map_or_else(|| "signal".to_string(), |code| code.to_string());
    let mut sections = Vec::new();
    if !step.stdout.trim().is_empty() {
        sections.push(CommandOutputDetailSection {
            title: "stdout".into(),
            body: step.stdout.trim_end().to_string().into(),
        });
    }
    if !step.stderr.trim().is_empty() {
        sections.push(CommandOutputDetailSection {
            title: "stderr".into(),
            body: step.stderr.trim_end().to_string().into(),
        });
    }
    let heading = if step.status.success() {
        format!("{} succeeded", step.label)
    } else {
        format!("{} failed (exit {code})", step.label)
    };

    CommandOutputItem {
        label: step.label.into(),
        status: if step.status.success() {
            CommandOutputItemStatus::Passed
        } else {
            CommandOutputItemStatus::Failed
        },
        detail: Some(CommandOutputDetail::with_sections(heading, sections)),
    }
}

impl RduelView {
    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        rduel_project: Option<RduelProjectFiles>,
        language_registry: Arc<LanguageRegistry>,
        session: RduelSession,
        main_rs_buffer: Entity<Buffer>,
        cargo_toml_buffer: Entity<Buffer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let problem = session
            .room
            .as_ref()
            .map(|room| RduelProblem::from_server(&room.problem))
            .unwrap_or_else(|| RduelProblem {
                id: "Rduel".into(),
                title: "Rduel".into(),
                markdown: "No problem was received from the server.".into(),
                samples: Vec::new(),
            });
        let room_id = if session.is_history {
            None
        } else {
            session.room.as_ref().map(|room| room.id.clone())
        };
        let room_status = session
            .room
            .as_ref()
            .map(|room| room.status.clone())
            .unwrap_or(ServerRoomStatus::Playing);
        let configured_atcoder_user = RduelSettings::get_global(cx).atcoder_user.clone();
        let local_atcoder_user = if session.is_history {
            session.room.as_ref().and_then(|room| {
                room.players
                    .iter()
                    .find(|player| player.id == session.player_id)
                    .map(|player| player.name.clone())
            })
        } else {
            let configured_atcoder_user = configured_atcoder_user.trim();
            (!configured_atcoder_user.is_empty()).then(|| configured_atcoder_user.to_string())
        };
        let player_id = session
            .room
            .as_ref()
            .map(|room| {
                resolve_local_player_id(
                    room,
                    session.player_id.clone(),
                    local_atcoder_user.as_deref(),
                )
            })
            .unwrap_or_else(|| session.player_id.clone());
        let token = (!session.token.is_empty()).then(|| session.token.clone());
        let opponent_main_rs_editor =
            session
                .opponent_main_rs
                .as_ref()
                .map(|(atcoder_user, source_code)| {
                    Self::new_opponent_main_rs_editor(
                        source_code,
                        atcoder_user,
                        language_registry.clone(),
                        window,
                        cx,
                    )
                });
        let main_rs_buffer_for_view = main_rs_buffer.clone();
        let cargo_toml_buffer_for_view = cargo_toml_buffer.clone();
        let problem_markdown =
            Self::new_problem_markdown(problem.markdown.clone(), language_registry.clone(), cx);
        let samples = problem.samples.clone();

        let main_rs_multibuffer = cx
            .new(|cx| MultiBuffer::singleton(main_rs_buffer, cx).with_title("src/main.rs".into()));
        let main_rs_editor = cx.new(|cx| {
            let mut editor =
                Editor::for_multibuffer(main_rs_multibuffer, Some(project.clone()), window, cx);
            editor.set_edit_predictions_disabled(true, cx);
            editor.set_should_serialize_selection_changes(false);
            editor
        });
        let cargo_toml_multibuffer = cx.new(|cx| {
            MultiBuffer::singleton(cargo_toml_buffer, cx).with_title("Cargo.toml".into())
        });
        let cargo_toml_editor = cx.new(|cx| {
            let mut editor =
                Editor::for_multibuffer(cargo_toml_multibuffer, Some(project.clone()), window, cx);
            editor.set_edit_predictions_disabled(true, cx);
            editor.set_should_serialize_selection_changes(false);
            editor
        });
        let initial_presence = session.room.as_ref().map(|room| {
            RoomPresence::from_room(
                room,
                Some(player_id.as_str()),
                local_atcoder_user.as_deref(),
            )
        });
        let test_cases = samples
            .iter()
            .map(|sample| RduelTestCase {
                input: Self::new_case_editor(&sample.input, window, cx),
                expected: Self::new_case_editor(&sample.output, window, cx),
                default: Some(sample.clone()),
                result: None,
            })
            .collect::<Vec<_>>();
        let initial_command_output = session
            .initial_command_output
            .unwrap_or_else(CommandOutputState::empty);
        let output_selection = if !initial_command_output.items.is_empty() {
            OutputSelection::Step(0)
        } else if test_cases.is_empty() {
            OutputSelection::Step(0)
        } else {
            OutputSelection::Case(0)
        };
        let view = Self {
            focus_handle: cx.focus_handle(),
            project,
            language_registry,
            room: RoomState {
                match_state: MatchState {
                    player_id: Some(player_id),
                    token,
                    room_id,
                    server_url: session.server_url,
                    local_atcoder_user,
                    room_status,
                },
            },
            problem,
            rduel_project,
            problem_markdown,
            main_rs_buffer: main_rs_buffer_for_view,
            cargo_toml_buffer: cargo_toml_buffer_for_view,
            main_rs_editor,
            cargo_toml_editor,
            opponent_main_rs_editor,
            workspace,
            search_target_editor: None,
            search_bar_subscriptions: None,
            active_code_tab: ActiveCodeTab::MainRs,
            layout_order: LayoutOrder::ProblemLeft,
            problem_width_fraction: DEFAULT_PROBLEM_WIDTH_FRACTION,
            command_output_height: DEFAULT_COMMAND_OUTPUT_HEIGHT,
            problem_scroll_handle: ScrollHandle::new(),
            command_output: initial_command_output,
            command_status: CommandStatus::Idle,
            test_cases,
            output_selection,
            presence: initial_presence,
            opponent_flash: None,
            last_snapshot_upload: None,
            submission_watch_started_at: None,
        };
        if view.room.match_state.room_status == ServerRoomStatus::Playing {
            view.poll_room_after_delay(cx);
            view.tick_match_timer(cx);
            view.schedule_snapshot_upload(cx);
        }
        view
    }

    fn new_case_editor(text: &str, window: &mut Window, cx: &mut Context<Self>) -> Entity<Editor> {
        cx.new(|cx| {
            let buffer = cx.new(|cx| Buffer::local("", cx));
            let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: SizingBehavior::SizeByContent,
                },
                buffer,
                None,
                window,
                cx,
            );
            editor.set_text(text, window, cx);
            editor.set_edit_predictions_disabled(true, cx);
            editor.set_show_gutter(false, cx);
            editor.disable_scrollbars_and_minimap(window, cx);
            editor.set_forbid_vertical_scroll(true);
            // Disable soft wrap: with wrapping, the editor's auto height depends on
            // its width, so the detail scrollbar appearing re-wraps it, which
            // toggles the scrollbar again — an infinite relayout loop that strobes
            // the whole pane (including the toolbar buttons).
            editor.set_soft_wrap_mode(language::language_settings::SoftWrap::None, cx);
            editor
        })
    }

    fn new_problem_markdown(
        markdown: SharedString,
        language_registry: Arc<LanguageRegistry>,
        cx: &mut Context<Self>,
    ) -> Entity<Markdown> {
        cx.new(|cx| {
            Markdown::new_with_options(
                markdown,
                Some(language_registry),
                None,
                MarkdownOptions {
                    parse_html: true,
                    render_math: true,
                    parse_heading_slugs: true,
                    ..Default::default()
                },
                cx,
            )
        })
    }

    fn run_samples(&mut self, _: &RunSamples, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rduel_project) = self.rduel_project.clone() else {
            self.command_status = CommandStatus::Failed;
            self.set_command_output(
                single_command_output_state(
                    "Project",
                    CommandOutputItemStatus::Failed,
                    "Rduel project files are not available. Reopen Rduel after checking ~/.rduel.",
                ),
                cx,
            );
            return;
        };

        self.spawn_rduel_command(
            "Test",
            rduel_project.clone(),
            RduelCommand::Test {
                root_path: rduel_project.root_path.clone(),
                test_path: rduel_project.test_path.clone(),
                target_path: rduel_project.target_path,
            },
            window,
            cx,
        );
    }

    fn toggle_layout(&mut self, _: &ToggleLayout, _window: &mut Window, cx: &mut Context<Self>) {
        self.layout_order = match self.layout_order {
            LayoutOrder::ProblemLeft => LayoutOrder::CodeLeft,
            LayoutOrder::CodeLeft => LayoutOrder::ProblemLeft,
        };
        cx.notify();
    }

    fn select_main_rs(&mut self, _: &SelectMainRs, window: &mut Window, cx: &mut Context<Self>) {
        self.active_code_tab = ActiveCodeTab::MainRs;
        self.main_rs_editor
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
        cx.notify();
    }

    fn select_cargo_toml(
        &mut self,
        _: &SelectCargoToml,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_code_tab = ActiveCodeTab::CargoToml;
        self.cargo_toml_editor
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
        cx.notify();
    }

    fn select_opponent_main_rs(
        &mut self,
        _: &SelectOpponentMainRs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.opponent_main_rs_editor.as_ref() else {
            return;
        };
        self.active_code_tab = ActiveCodeTab::OpponentMainRs;
        editor.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn rename_symbol(&mut self, action: &Rename, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.focused_code_editor(window, cx) else {
            cx.propagate();
            return;
        };
        let handled = editor.update(cx, |editor, cx| {
            if let Some(task) = editor.rename(action, window, cx) {
                editor.detach_and_notify_err(task, window, cx);
                true
            } else {
                false
            }
        });
        if !handled {
            cx.propagate();
        }
    }

    fn confirm_rename(
        &mut self,
        action: &ConfirmRename,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self
            .code_editor_with_pending_rename(cx)
            .or_else(|| self.focused_code_editor(window, cx))
        else {
            cx.propagate();
            return;
        };
        let handled = editor.update(cx, |editor, cx| {
            if let Some(task) = editor.confirm_rename(action, window, cx) {
                editor.detach_and_notify_err(task, window, cx);
                true
            } else {
                false
            }
        });
        if !handled {
            cx.propagate();
        }
    }

    fn active_code_editor(&self) -> Option<Entity<Editor>> {
        match self.active_code_tab {
            ActiveCodeTab::MainRs => Some(self.main_rs_editor.clone()),
            ActiveCodeTab::CargoToml => Some(self.cargo_toml_editor.clone()),
            ActiveCodeTab::OpponentMainRs => self.opponent_main_rs_editor.clone(),
        }
    }

    fn focused_code_editor(&self, window: &Window, cx: &App) -> Option<Entity<Editor>> {
        for editor in [&self.main_rs_editor, &self.cargo_toml_editor] {
            if editor
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
            {
                return Some(editor.clone());
            }
        }

        self.active_code_editor()
    }

    fn code_editor_with_pending_rename(&self, cx: &App) -> Option<Entity<Editor>> {
        for editor in [&self.main_rs_editor, &self.cargo_toml_editor] {
            if editor.read(cx).pending_rename().is_some() {
                return Some(editor.clone());
            }
        }

        None
    }

    fn focused_search_editor(&self, window: &Window, cx: &App) -> Option<Entity<Editor>> {
        let active_code_editor = self.active_code_editor();
        for editor in active_code_editor.iter() {
            if editor
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
            {
                return Some(editor.clone());
            }
        }

        for case in &self.test_cases {
            if case
                .input
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
            {
                return Some(case.input.clone());
            }
            if case
                .expected
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
            {
                return Some(case.expected.clone());
            }
        }

        if let Some(editor) = self.search_target_editor.as_ref() {
            return Some(editor.clone());
        }

        active_code_editor
    }

    fn active_pane_search_bar(&self, cx: &App) -> Option<Entity<BufferSearchBar>> {
        let pane = self
            .workspace
            .read_with(cx, |workspace, _| workspace.active_pane().clone())
            .log_err()?;
        pane.read(cx)
            .toolbar()
            .read(cx)
            .item_of_type::<BufferSearchBar>()
    }

    fn sync_search_target(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<BufferSearchBar>> {
        let editor = self.focused_search_editor(window, cx)?;
        let search_bar = self.active_pane_search_bar(cx)?;
        self.ensure_search_bar_subscriptions(&search_bar, cx);
        if self.search_target_editor.as_ref() == Some(&editor)
            && !search_bar.read(cx).is_dismissed()
        {
            return Some(search_bar);
        }
        self.search_target_editor = Some(editor.clone());
        search_bar.update(cx, |search_bar, cx| {
            search_bar.set_active_pane_item(Some(&editor), window, cx);
        });
        Some(search_bar)
    }

    fn ensure_search_bar_subscriptions(
        &mut self,
        search_bar: &Entity<BufferSearchBar>,
        cx: &mut Context<Self>,
    ) {
        if self
            .search_bar_subscriptions
            .as_ref()
            .is_some_and(|(entity_id, _)| *entity_id == search_bar.entity_id())
        {
            return;
        }

        self.search_bar_subscriptions = Some((
            search_bar.entity_id(),
            vec![
                cx.subscribe(search_bar, |_, _, _: &search::buffer_search::Event, cx| {
                    cx.notify()
                }),
                cx.subscribe(search_bar, |_, _, _: &ToolbarItemEvent, cx| cx.notify()),
            ],
        ));
    }

    fn new_opponent_main_rs_editor(
        source_code: &str,
        atcoder_user: &str,
        language_registry: Arc<LanguageRegistry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Editor> {
        let title = if atcoder_user.trim().is_empty() {
            "opponent/main.rs".to_string()
        } else {
            format!("{}/main.rs", atcoder_user.trim())
        };
        let buffer = cx.new(|cx| Buffer::local(source_code, cx));
        let buffer_for_language = buffer.clone();
        cx.spawn(async move |_, cx| {
            let Some(language) = language_registry.language_for_name("Rust").await.log_err() else {
                return anyhow::Ok(());
            };
            buffer_for_language.update(cx, |buffer, cx| {
                buffer.set_language(Some(language), cx);
            });
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
        let multi_buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title.into()));
        cx.new(|cx| {
            let mut editor = Editor::for_multibuffer(multi_buffer, None, window, cx);
            editor.set_read_only(true);
            editor.set_edit_predictions_disabled(true, cx);
            editor
        })
    }

    fn submit_solution(&mut self, _: &SubmitSolution, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rduel_project) = self.rduel_project.clone() else {
            self.command_status = CommandStatus::Failed;
            self.set_command_output(
                single_command_output_state(
                    "Project",
                    CommandOutputItemStatus::Failed,
                    "Rduel project files are not available. Reopen Rduel after checking ~/.rduel.",
                ),
                cx,
            );
            return;
        };

        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            "Submit solution?",
            Some("Rduel will save src/main.rs and Cargo.toml, then prepare an acr-style submit."),
            &["Submit", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await? != 0 {
                return Ok(());
            }

            this.update_in(cx, |this, window, cx| {
                this.spawn_rduel_command(
                    "Submit",
                    rduel_project.clone(),
                    RduelCommand::Submit {
                        root_path: rduel_project.root_path.clone(),
                        problem_rs_path: rduel_project.problem_rs_path.clone(),
                        cargo_toml_path: rduel_project.cargo_toml_path.clone(),
                        test_path: rduel_project.test_path.clone(),
                        target_path: rduel_project.target_path.clone(),
                    },
                    window,
                    cx,
                );
            })
        })
        .detach_and_log_err(cx);
    }

    fn spawn_rduel_command(
        &mut self,
        label: &'static str,
        rduel_project: RduelProjectFiles,
        command: RduelCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.command_status.is_running() {
            return;
        }

        // Persist the (possibly edited/added) test cases to disk so the runner
        // judges against exactly what the user sees.
        if let Err(error) = self.write_test_cases_to_disk(&rduel_project, cx) {
            self.command_status = CommandStatus::Failed;
            self.command_output = single_command_output_state(
                "Test cases",
                CommandOutputItemStatus::Failed,
                format!("Failed to write test cases:\n{error:#}"),
            );
            self.output_selection = OutputSelection::Step(0);
            cx.notify();
            return;
        }

        let is_submit = matches!(command, RduelCommand::Submit { .. });
        self.command_status = CommandStatus::Running;
        if !is_submit {
            self.output_selection = OutputSelection::Step(0);
            self.set_command_output(CommandOutputState::running(label), cx);
        }

        let save_task =
            self.save_solution_editors(SaveOptions::default(), self.project.clone(), window, cx);

        cx.spawn(async move |this, cx| {
            let result = async {
                save_task.await?;
                cx.background_spawn(async move { command.run().await })
                    .await
            }
            .await;
            this.update_in(cx, |this, _window, cx| {
                for case in &mut this.test_cases {
                    case.result = None;
                }
                let mut start_submission_watch = false;
                let mut output_state = match result {
                    Ok(output) => {
                        this.command_status = if output.success {
                            CommandStatus::Succeeded
                        } else {
                            CommandStatus::Failed
                        };
                        if let Some(submit_ready) = output.submit_ready {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                submit_ready.source_code,
                            ));
                            this.upload_code_snapshot(cx);
                            cx.open_url(&submit_ready.submit_url);
                            start_submission_watch = true;
                            log::info!(
                                "prepared Rduel submit for {}",
                                submit_ready.source_path.display()
                            );
                        }
                        for (index, case_result) in output.case_results {
                            if let Some(case) = this.test_cases.get_mut(index) {
                                case.result = Some(case_result);
                            }
                        }
                        CommandOutputState {
                            items: output.items,
                        }
                    }
                    Err(error) => {
                        this.command_status = CommandStatus::Failed;
                        let detail = format!("Failed before running command:\n{error:#}");
                        single_command_output_state(
                            "Command",
                            CommandOutputItemStatus::Failed,
                            detail,
                        )
                    }
                };

                if !rduel_project.root_path.exists() {
                    output_state.items.push(CommandOutputItem {
                        label: "Project".into(),
                        status: CommandOutputItemStatus::Failed,
                        detail: Some(CommandOutputDetail::new(format!(
                            "Rduel project directory no longer exists: {}",
                            rduel_project.root_path.display()
                        ))),
                    });
                }

                this.command_output = output_state;
                if start_submission_watch {
                    this.start_server_submission_watch(cx);
                }
                this.focus_after_run();
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    /// Selects the first failed test case after a run; falls back to the first
    /// failed run step (e.g. a compile failure) and otherwise the first chip.
    fn focus_after_run(&mut self) {
        if let Some(index) = self
            .test_cases
            .iter()
            .position(|case| matches!(&case.result, Some(result) if result.status == CommandOutputItemStatus::Failed))
        {
            self.output_selection = OutputSelection::Case(index);
        } else if let Some(index) = self
            .command_output
            .items
            .iter()
            .position(|item| item.status == CommandOutputItemStatus::Failed)
        {
            self.output_selection = OutputSelection::Step(index);
        } else if !self.test_cases.is_empty() {
            self.output_selection = OutputSelection::Case(0);
        } else if !self.command_output.items.is_empty() {
            self.output_selection = OutputSelection::Step(0);
        }
    }

    fn write_test_cases_to_disk(
        &self,
        rduel_project: &RduelProjectFiles,
        cx: &App,
    ) -> anyhow::Result<()> {
        let cases: Vec<(String, String)> = self
            .test_cases
            .iter()
            .map(|case| {
                (
                    case.input.read(cx).text(cx),
                    case.expected.read(cx).text(cx),
                )
            })
            .collect();
        write_test_case_files(rduel_project, &cases)
    }

    fn upsert_submission_output_item(
        &mut self,
        item: CommandOutputItem,
        select_item: bool,
        cx: &mut Context<Self>,
    ) {
        let index = match self
            .command_output
            .items
            .iter()
            .position(|existing| existing.label.as_ref() == "Submission")
        {
            Some(index) => {
                self.command_output.items[index] = item;
                index
            }
            None => {
                self.command_output.items.push(item);
                self.command_output.items.len().saturating_sub(1)
            }
        };
        if select_item {
            self.output_selection = OutputSelection::Step(index);
        }
        cx.notify();
    }

    fn start_server_submission_watch(&mut self, cx: &mut Context<Self>) {
        if self.room.match_state.room_status != ServerRoomStatus::Playing {
            return;
        }
        let (Some(room_id), Some(player_id), Some(token), server_url) = (
            self.room.match_state.room_id.clone(),
            self.room.match_state.player_id.clone(),
            self.room.match_state.token.clone(),
            self.room.match_state.server_url.clone(),
        ) else {
            self.upsert_submission_output_item(
                CommandOutputItem {
                    label: "Submission".into(),
                    status: CommandOutputItemStatus::Failed,
                    detail: Some(CommandOutputDetail::new(
                        "Could not start submission detection for this room.",
                    )),
                },
                true,
                cx,
            );
            return;
        };

        let from_second = unix_now();
        self.submission_watch_started_at = Some(from_second);
        self.upsert_submission_output_item(
            CommandOutputItem {
                label: "Submission".into(),
                status: CommandOutputItemStatus::Pending,
                detail: Some(CommandOutputDetail::new(
                    "Waiting for the next AtCoder submission...",
                )),
            },
            true,
            cx,
        );
        self.poll_submission_after_delay(room_id, player_id, token, server_url, from_second, cx);
    }

    fn poll_submission_after_delay(
        &self,
        room_id: String,
        player_id: String,
        token: String,
        server_url: String,
        from_second: i64,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(3)).await;
            let server_url_for_request = server_url.clone();
            let room_id_for_request = room_id.clone();
            let player_id_for_request = player_id.clone();
            let token_for_request = token.clone();
            let result = cx
                .background_spawn(async move {
                    RduelMatchCommand::WatchSubmissions {
                        server_url: server_url_for_request,
                        room_id: room_id_for_request,
                        player_id: player_id_for_request,
                        token: token_for_request,
                        from_second,
                    }
                    .run()
                })
                .await;

            this.update_in(cx, |this, window, cx| {
                if this.submission_watch_started_at != Some(from_second) {
                    return;
                }
                match result {
                    Ok(RduelMatchOutput::SubmissionCheck {
                        room,
                        detected_submission,
                    }) => {
                        this.update_room_presence(&room);
                        let is_finished = this.handle_room_status(room, window, cx);
                        if let Some(submission) = detected_submission {
                            this.submission_watch_started_at = None;
                            this.upsert_submission_output_item(
                                submission_output_item(&submission),
                                true,
                                cx,
                            );
                        } else if !is_finished {
                            this.poll_submission_after_delay(
                                room_id,
                                player_id,
                                token,
                                server_url,
                                from_second,
                                cx,
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        this.submission_watch_started_at = None;
                        log::warn!("failed to check Rduel submission: {error:#}");
                        this.upsert_submission_output_item(
                            CommandOutputItem {
                                label: "Submission".into(),
                                status: CommandOutputItemStatus::Failed,
                                detail: Some(CommandOutputDetail::new(format!(
                                    "Could not check AtCoder submissions:\n{error:#}"
                                ))),
                            },
                            true,
                            cx,
                        );
                    }
                }
                cx.notify();
            })?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Rebuilds the presence snapshot from a fresh room state and flashes a
    /// banner when the opponent has made a new submission since the last poll.
    fn update_room_presence(&mut self, room: &ServerRoom) {
        let local_id = self.room.match_state.player_id.as_deref();
        let next = RoomPresence::from_room(
            room,
            local_id,
            self.room.match_state.local_atcoder_user.as_deref(),
        );

        let previous_opponent = self
            .presence
            .as_ref()
            .and_then(|presence| presence.opponent.as_ref());
        if let (Some(previous), Some(current)) = (previous_opponent, next.opponent.as_ref()) {
            let attempts_increased =
                current.activity.attempt_count > previous.activity.attempt_count;
            let newer_submission = match (
                current.activity.last_submission_epoch,
                previous.activity.last_submission_epoch,
            ) {
                (Some(current_epoch), Some(previous_epoch)) => current_epoch > previous_epoch,
                (Some(_), None) => true,
                _ => false,
            };
            if attempts_increased || newer_submission {
                let verdict = current
                    .activity
                    .last_verdict
                    .clone()
                    .unwrap_or_else(|| "提交".to_string());
                self.opponent_flash = Some(OpponentFlash {
                    message: format!("对手提交了：{verdict}").into(),
                    shown_at: Instant::now(),
                });
            }
        }

        self.presence = Some(next);
    }

    /// Re-renders once per second so the elapsed-time clock advances and a stale
    /// opponent flash is cleared. Stops when the match finishes.
    fn tick_match_timer(&self, cx: &mut Context<Self>) {
        if self.room.match_state.room_status != ServerRoomStatus::Playing {
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            this.update(cx, |this, cx| {
                if let Some(flash) = &this.opponent_flash {
                    if flash.shown_at.elapsed() > OPPONENT_FLASH_DURATION {
                        this.opponent_flash = None;
                    }
                }
                if this.room.match_state.room_status == ServerRoomStatus::Playing {
                    this.tick_match_timer(cx);
                }
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn poll_room_after_delay(&self, cx: &mut Context<Self>) {
        let (Some(room_id), server_url) = (
            self.room.match_state.room_id.clone(),
            self.room.match_state.server_url.clone(),
        ) else {
            return;
        };

        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(3)).await;
            let result = cx
                .background_spawn(async move {
                    RduelMatchCommand::PollRoom {
                        server_url,
                        room_id,
                    }
                    .run()
                })
                .await;

            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(RduelMatchOutput::RoomStatus { room }) => {
                        this.update_room_presence(&room);
                        if !this.handle_room_status(room, window, cx) {
                            this.poll_room_after_delay(cx);
                        }
                    }
                    Ok(RduelMatchOutput::Waiting { .. } | RduelMatchOutput::Matched { .. }) => {
                        this.poll_room_after_delay(cx);
                    }
                    Ok(RduelMatchOutput::History { .. }) => {
                        this.poll_room_after_delay(cx);
                    }
                    Ok(RduelMatchOutput::SubmissionCheck { .. }) => {
                        this.poll_room_after_delay(cx);
                    }
                    Err(error) => {
                        log::debug!("failed to poll Rduel room: {error:#}");
                        this.poll_room_after_delay(cx);
                    }
                }
                cx.notify();
            })?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Applies an authoritative `RoomStatus`: shows the win/loss prompt the first
    /// time the match transitions to finished, and reports whether the match is
    /// finished so the caller can decide whether to keep polling.
    fn handle_room_status(
        &mut self,
        room: ServerRoom,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.add_opponent_solution_if_lost(&room, window, cx);
        if let Some(message) = self.apply_room_status(room) {
            self.upload_code_snapshot(cx);
            drop(window.prompt(gpui::PromptLevel::Info, &message, None, &["OK"], cx));
        }
        self.room.match_state.room_status == ServerRoomStatus::Finished
    }

    fn add_opponent_solution_if_lost(
        &mut self,
        room: &ServerRoom,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.opponent_main_rs_editor.is_some() || room.status != ServerRoomStatus::Finished {
            return;
        }
        let local_player_id = self.room.match_state.player_id.as_deref();
        let Some(winning_submission) = room.winning_submission.as_ref() else {
            return;
        };
        if Some(winning_submission.player_id.as_str()) == local_player_id {
            return;
        }
        let Some(source_code) = winning_submission
            .source_code
            .as_deref()
            .filter(|source_code| !source_code.is_empty())
        else {
            return;
        };

        self.opponent_main_rs_editor = Some(Self::new_opponent_main_rs_editor(
            source_code,
            &winning_submission.atcoder_user,
            self.language_registry.clone(),
            window,
            cx,
        ));
    }

    fn apply_room_status(&mut self, room: ServerRoom) -> Option<String> {
        if self.room.match_state.room_status == ServerRoomStatus::Finished {
            return None;
        }
        if room.status != ServerRoomStatus::Finished {
            return None;
        }

        let winner = room.winner_player_id.as_deref();
        let local_player_id = self.room.match_state.player_id.as_deref();
        let remote_name = room
            .players
            .iter()
            .find(|player| Some(player.id.as_str()) != local_player_id)
            .map(|player| player.name.as_str())
            .unwrap_or("opponent");
        let message = match room.finish_reason {
            Some(ServerRoomFinishReason::PlayerLeft) if winner == local_player_id => {
                format!("The match has finished. You won because {remote_name} left the match.")
            }
            Some(ServerRoomFinishReason::PlayerLeft) if winner.is_some() => {
                format!("The match has finished. {remote_name} won because you left the match.")
            }
            Some(ServerRoomFinishReason::ManualComplete) if winner == local_player_id => {
                "The match has finished. You won.".to_string()
            }
            Some(ServerRoomFinishReason::ManualComplete) if winner.is_some() => {
                format!("The match has finished. {remote_name} completed first.")
            }
            _ if winner == local_player_id => "The match has finished. You won.".to_string(),
            _ if winner.is_some() => {
                format!("The match has finished. {remote_name} got AC first.")
            }
            _ => "The match has finished.".to_string(),
        };
        self.room.match_state.room_status = ServerRoomStatus::Finished;
        Some(message)
    }

    fn leave_active_match(&mut self, cx: &mut Context<Self>) {
        if self.room.match_state.room_status != ServerRoomStatus::Playing {
            return;
        }
        let Some(player_id) = self.room.match_state.player_id.take() else {
            return;
        };
        self.room.match_state.room_id.take();
        let token = self.room.match_state.token.take().unwrap_or_default();
        let server_url = self.room.match_state.server_url.clone();
        let main_rs = Some(self.main_rs_buffer.read(cx).text());
        let cargo_toml = Some(self.cargo_toml_buffer.read(cx).text());
        cx.background_spawn(async move {
            RduelMatchCommand::Leave {
                server_url,
                player_id,
                token,
                main_rs,
                cargo_toml,
            }
            .run()
        })
        .detach_and_log_err(cx);
    }

    fn upload_code_snapshot(&mut self, cx: &mut Context<Self>) {
        let (Some(room_id), Some(player_id), Some(token)) = (
            self.room.match_state.room_id.clone(),
            self.room.match_state.player_id.clone(),
            self.room.match_state.token.clone(),
        ) else {
            return;
        };

        // Check if enough time has passed since last upload
        if let Some(last_upload) = self.last_snapshot_upload {
            if last_upload.elapsed() < CODE_SNAPSHOT_INTERVAL {
                return;
            }
        }

        self.last_snapshot_upload = Some(Instant::now());
        let server_url = self.room.match_state.server_url.clone();
        let main_rs = self.main_rs_buffer.read(cx).text();
        let cargo_toml = self.cargo_toml_buffer.read(cx).text();
        cx.spawn(async move |_, cx| {
            let result = cx
                .background_spawn(async move {
                    RduelMatchCommand::UploadCodeSnapshot {
                        server_url,
                        room_id,
                        player_id,
                        token,
                        main_rs,
                        cargo_toml,
                    }
                    .run()
                })
                .await;
            if let Err(error) = result {
                log::warn!("Failed to upload Rduel code snapshot: {error:#}");
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn schedule_snapshot_upload(&self, cx: &mut Context<Self>) {
        if self.room.match_state.room_status != ServerRoomStatus::Playing {
            return;
        }

        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CODE_SNAPSHOT_INTERVAL).await;
            this.update(cx, |this, cx| {
                if this.room.match_state.room_status == ServerRoomStatus::Playing {
                    this.upload_code_snapshot(cx);
                    this.schedule_snapshot_upload(cx);
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn set_command_output(&mut self, output: CommandOutputState, cx: &mut Context<Self>) {
        self.command_output = output;
        cx.notify();
    }

    fn has_unsaved_solution_buffers(&self, cx: &App) -> bool {
        self.main_rs_buffer.read(cx).is_dirty() || self.cargo_toml_buffer.read(cx).is_dirty()
    }

    fn save_solution_editors(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Task<anyhow::Result<()>> {
        let main_rs_editor = self.main_rs_editor.clone();
        let cargo_toml_editor = self.cargo_toml_editor.clone();

        cx.spawn_in(window, async move |this, cx| {
            let main_rs_save = main_rs_editor.update_in(cx, |editor, window, cx| {
                editor.save(options, project.clone(), window, cx)
            })?;
            if let Err(error) = main_rs_save.await {
                return Err(error);
            }

            let cargo_toml_save = cargo_toml_editor.update_in(cx, |editor, window, cx| {
                editor.save(options, project, window, cx)
            })?;
            if let Err(error) = cargo_toml_save.await {
                return Err(error);
            }

            this.update(cx, |_this, cx| cx.notify())?;
            Ok(())
        })
    }

    fn resize_problem_width(
        &mut self,
        event: &DragMoveEvent<DraggedRduelDivider>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let bounds_width = event.bounds.size.width.as_f32();
        if bounds_width <= 0.0 {
            return;
        }

        let pointer_x = event.event.position.x.as_f32();
        let bounds_left = event.bounds.left().as_f32();
        let bounds_right = event.bounds.right().as_f32();
        let problem_width_fraction = match self.layout_order {
            LayoutOrder::ProblemLeft => (pointer_x - bounds_left) / bounds_width,
            LayoutOrder::CodeLeft => (bounds_right - pointer_x) / bounds_width,
        }
        .clamp(MIN_PROBLEM_WIDTH_FRACTION, MAX_PROBLEM_WIDTH_FRACTION);

        if (self.problem_width_fraction - problem_width_fraction).abs() > f32::EPSILON {
            self.problem_width_fraction = problem_width_fraction;
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn reset_problem_width(&mut self, cx: &mut Context<Self>) {
        self.problem_width_fraction = DEFAULT_PROBLEM_WIDTH_FRACTION;
        cx.notify();
    }

    fn resize_command_output(
        &mut self,
        event: &DragMoveEvent<DraggedRduelOutputDivider>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let bounds_bottom = event.bounds.bottom().as_f32();
        let bounds_top = event.bounds.top().as_f32();
        let pointer_y = event.event.position.y.as_f32();
        // Allow the panel to span from a thin toolbar strip at the very bottom up
        // to just below the code tab bar at the very top of the code area.
        let available = (bounds_bottom - bounds_top).max(0.0);
        let max_height = (available - CODE_AREA_TOP_RESERVE).max(MIN_COMMAND_OUTPUT_HEIGHT);
        let command_output_height =
            (bounds_bottom - pointer_y).clamp(MIN_COMMAND_OUTPUT_HEIGHT, max_height);

        if (self.command_output_height - command_output_height).abs() > f32::EPSILON {
            self.command_output_height = command_output_height;
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn reset_command_output_height(&mut self, cx: &mut Context<Self>) {
        self.command_output_height = DEFAULT_COMMAND_OUTPUT_HEIGHT;
        cx.notify();
    }

    fn problem_markdown_style(&self, window: &mut Window, cx: &mut Context<Self>) -> MarkdownStyle {
        let mut style = MarkdownStyle::themed(MarkdownFont::Preview, window, cx);
        let font_size = style.base_text_style.font_size.to_pixels(window.rem_size())
            * PROBLEM_MARKDOWN_FONT_SCALE;
        style.base_text_style.font_size = font_size.into();
        style.container_style.text.font_size = Some(font_size.into());
        style
    }

    fn render_action_button(
        &self,
        id: &'static str,
        icon: IconName,
        label: &'static str,
        disabled: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .id(id)
            .h(px(28.))
            .items_center()
            .gap_1()
            .px_2()
            .rounded_sm()
            .bg(cx.theme().colors().ghost_element_background)
            .when(!disabled, |this| {
                this.hover(|this| this.bg(cx.theme().colors().ghost_element_hover))
                    .cursor_pointer()
            })
            .when(disabled, |this| this.opacity(0.5))
            .child(Icon::new(icon).size(IconSize::Small))
            .child(Label::new(label).size(LabelSize::Default))
            .on_click(cx.listener(move |this, _, window, cx| {
                if disabled {
                    return;
                }
                match id {
                    "rduel-run-samples" => this.run_samples(&RunSamples, window, cx),
                    "rduel-submit" => this.submit_solution(&SubmitSolution, window, cx),
                    _ => {}
                }
            }))
    }

    fn render_problem(
        &self,
        markdown_style: MarkdownStyle,
        _window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        v_flex()
            .w(relative(self.problem_width_fraction))
            .h_full()
            .min_w(px(280.))
            .overflow_hidden()
            .child(
                h_flex()
                    .h(px(40.))
                    .items_center()
                    .gap_2()
                    .px_4()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Label::new(format_problem_heading(
                            &self.problem.id,
                            &self.problem.title,
                        ))
                        .size(LabelSize::Default)
                        .truncate(),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("rduel-problem")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_y_scroll()
                            .p_4()
                            .track_scroll(&self.problem_scroll_handle)
                            .child(
                                MarkdownElement::new(self.problem_markdown.clone(), markdown_style)
                                    .code_block_renderer(CodeBlockRenderer::Default {
                                        copy_button_visibility: CopyButtonVisibility::Hidden,
                                        wrap_button_visibility: WrapButtonVisibility::Hidden,
                                        border: false,
                                    })
                                    .image_resolver(|dest_url| {
                                        // Remote statement figures (AtCoder hosts them at
                                        // absolute https URLs) need an explicit resolver; the
                                        // markdown component only auto-loads `data:` images.
                                        (dest_url.starts_with("http://")
                                            || dest_url.starts_with("https://"))
                                        .then(|| {
                                            ImageSource::Resource(Resource::Uri(SharedUri::from(
                                                dest_url.to_string(),
                                            )))
                                        })
                                    })
                                    .scroll_handle(self.problem_scroll_handle.clone()),
                            ),
                    )
                    .child(self.render_problem_scroll_progress(cx)),
            )
    }

    fn render_problem_scroll_progress(&self, cx: &App) -> impl IntoElement {
        let max_offset = self.problem_scroll_handle.max_offset().y.as_f32().max(0.0);
        let offset = (-self.problem_scroll_handle.offset().y.as_f32()).clamp(0.0, max_offset);
        let viewport_height = self
            .problem_scroll_handle
            .bounds()
            .size
            .height
            .as_f32()
            .max(0.0);
        let content_height = viewport_height + max_offset;
        let thumb_fraction = if content_height > 0.0 {
            (viewport_height / content_height).clamp(0.08_f32, 1.0_f32)
        } else {
            1.0
        };
        let travel_fraction = (1.0_f32 - thumb_fraction).max(0.0);
        let scroll_fraction = if max_offset > 0.0 {
            offset / max_offset
        } else {
            0.0
        };
        let leading_fraction = scroll_fraction * travel_fraction;

        v_flex()
            .w(px(8.))
            .h_full()
            .flex_none()
            .items_center()
            .py_1()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .w(px(3.))
                    .h_full()
                    .rounded_full()
                    .bg(cx.theme().colors().border.opacity(0.28))
                    .when(max_offset > 0.0, |track| {
                        track
                            .child(div().flex_none().h(relative(leading_fraction)))
                            .child(
                                div()
                                    .flex_none()
                                    .min_h(px(24.))
                                    .h(relative(thumb_fraction))
                                    .rounded_full()
                                    .bg(cx.theme().colors().text_muted.opacity(0.55)),
                            )
                            .child(div().flex_1())
                    }),
            )
    }

    fn render_editor(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active_editor = match self.active_code_tab {
            ActiveCodeTab::MainRs => self.main_rs_editor.clone(),
            ActiveCodeTab::CargoToml => self.cargo_toml_editor.clone(),
            ActiveCodeTab::OpponentMainRs => self
                .opponent_main_rs_editor
                .clone()
                .unwrap_or_else(|| self.main_rs_editor.clone()),
        };

        v_flex()
            .flex_1()
            .h_full()
            .overflow_hidden()
            .child(self.render_code_tabs(cx))
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .child(self.render_search_bar_for_editor(active_editor.clone(), cx))
                    .child(active_editor),
            )
            .child(self.render_command_output_divider(cx))
            .child(self.render_command_output(cx))
    }

    fn render_search_bar_for_editor(
        &self,
        editor: Entity<Editor>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(search_bar) = self.active_pane_search_bar(cx) else {
            return Empty.into_any_element();
        };
        if self.search_target_is_editor(&editor, &search_bar, cx) {
            search_bar.into_any_element()
        } else {
            Empty.into_any_element()
        }
    }

    fn search_target_is_editor(
        &self,
        editor: &Entity<Editor>,
        search_bar: &Entity<BufferSearchBar>,
        cx: &App,
    ) -> bool {
        self.search_target_editor.as_ref() == Some(editor) && !search_bar.read(cx).is_dismissed()
    }

    fn render_code_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_1()
            .h(px(40.))
            .items_center()
            .px_3()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(self.render_code_tab("src/main.rs", ActiveCodeTab::MainRs, cx))
            .child(self.render_code_tab("Cargo.toml", ActiveCodeTab::CargoToml, cx))
            .when(self.opponent_main_rs_editor.is_some(), |this| {
                this.child(self.render_code_tab(
                    "opponent/main.rs",
                    ActiveCodeTab::OpponentMainRs,
                    cx,
                ))
            })
    }

    fn render_code_tab(
        &self,
        label: &'static str,
        tab: ActiveCodeTab,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = match tab {
            ActiveCodeTab::MainRs => "rduel-code-tab-main-rs",
            ActiveCodeTab::CargoToml => "rduel-code-tab-cargo-toml",
            ActiveCodeTab::OpponentMainRs => "rduel-code-tab-opponent-main-rs",
        };
        let is_dirty = match tab {
            ActiveCodeTab::MainRs => self.main_rs_buffer.read(cx).is_dirty(),
            ActiveCodeTab::CargoToml => self.cargo_toml_buffer.read(cx).is_dirty(),
            ActiveCodeTab::OpponentMainRs => false,
        };
        let is_selected = self.active_code_tab == tab;
        let background_color = if is_selected {
            cx.theme().colors().element_background
        } else {
            cx.theme().colors().ghost_element_background
        };
        let label = if is_dirty {
            format!("{label} *").into()
        } else {
            SharedString::from(label)
        };

        h_flex()
            .id(id)
            .h(px(28.))
            .items_center()
            .px_2p5()
            .rounded_sm()
            .bg(background_color)
            .hover(|this| this.bg(cx.theme().colors().ghost_element_hover))
            .cursor_pointer()
            .child(Label::new(label).size(LabelSize::Default))
            .on_click(cx.listener(move |this, _, window, cx| {
                match tab {
                    ActiveCodeTab::MainRs => this.select_main_rs(&SelectMainRs, window, cx),
                    ActiveCodeTab::CargoToml => {
                        this.select_cargo_toml(&SelectCargoToml, window, cx)
                    }
                    ActiveCodeTab::OpponentMainRs => {
                        this.select_opponent_main_rs(&SelectOpponentMainRs, window, cx)
                    }
                };
            }))
    }

    fn render_command_output(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_command_running = self.command_status.is_running();

        let mut chips: Vec<AnyElement> = Vec::new();
        for (index, item) in self.command_output.items.iter().enumerate() {
            chips.push(self.render_step_chip(index, item, cx).into_any_element());
        }
        for index in 0..self.test_cases.len() {
            chips.push(self.render_case_chip(index, cx).into_any_element());
        }
        chips.push(self.render_add_case_chip(cx).into_any_element());

        v_flex()
            .h(px(self.command_output_height))
            .flex_none()
            .bg(cx.theme().colors().editor_background)
            .child(
                h_flex()
                    .h(px(32.))
                    .items_center()
                    .px_3()
                    .gap_1p5()
                    .overflow_hidden()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        h_flex()
                            .id("rduel-chip-row")
                            .flex_1()
                            .min_w_0()
                            .gap_1p5()
                            .overflow_x_scroll()
                            .children(chips),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .child(self.render_action_button(
                                "rduel-run-samples",
                                IconName::PlayFilled,
                                "Run",
                                is_command_running,
                                cx,
                            ))
                            .child(self.render_action_button(
                                "rduel-submit",
                                IconName::Send,
                                "Submit",
                                is_command_running,
                                cx,
                            )),
                    ),
            )
            .child(
                v_flex().flex_1().min_h_0().overflow_hidden().p_3().child(
                    div()
                        .id("rduel-command-output-detail")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .p_2()
                        .rounded_sm()
                        .border_1()
                        .border_color(cx.theme().colors().border)
                        .bg(cx.theme().colors().editor_background)
                        .child(self.render_output_detail(cx)),
                ),
            )
    }

    fn render_output_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.output_selection {
            OutputSelection::Step(index) => {
                self.render_command_output_detail(self.command_output.items.get(index), cx)
            }
            OutputSelection::Case(index) => match self.test_cases.get(index) {
                Some(case) => self.render_case_detail(index, case, cx),
                None => Empty.into_any_element(),
            },
        }
    }

    fn render_command_output_detail(
        &self,
        selected_item: Option<&CommandOutputItem>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(detail) = selected_item.and_then(|item| item.detail.as_ref()) else {
            return Empty.into_any_element();
        };

        v_flex()
            .gap_2()
            .when(!detail.heading.is_empty(), |this| {
                this.child(Label::new(detail.heading.clone()).size(LabelSize::Default))
            })
            .when_some(detail.diff.as_ref(), |this, diff| {
                this.child(self.render_output_diff(diff, cx))
            })
            .children(detail.sections.iter().map(|section| {
                self.render_command_output_detail_section(section, cx)
                    .into_any_element()
            }))
            .into_any_element()
    }

    fn render_output_diff(&self, diff: &OutputDiff, cx: &mut Context<Self>) -> impl IntoElement {
        let split = |text: &str| -> Vec<String> {
            if text.is_empty() {
                Vec::new()
            } else {
                text.split('\n').map(|line| line.to_string()).collect()
            }
        };
        let expected_lines = split(diff.expected.as_ref());
        let actual_lines = split(diff.actual.as_ref());
        let line_count = expected_lines.len().max(actual_lines.len());

        let deleted = cx.theme().status().deleted;
        let deleted_bg = deleted.opacity(0.12);
        let created = cx.theme().status().created;
        let created_bg = created.opacity(0.12);

        let mut rows: Vec<AnyElement> = Vec::new();
        for index in 0..line_count {
            let expected_line = expected_lines.get(index).map(String::as_str);
            let actual_line = actual_lines.get(index).map(String::as_str);
            match (expected_line, actual_line) {
                (Some(expected), Some(actual)) if expected == actual => {
                    rows.push(self.render_diff_row(' ', expected, None, None, cx));
                }
                (expected, actual) => {
                    if let Some(expected) = expected {
                        rows.push(self.render_diff_row(
                            '-',
                            expected,
                            Some(deleted),
                            Some(deleted_bg),
                            cx,
                        ));
                    }
                    if let Some(actual) = actual {
                        rows.push(self.render_diff_row(
                            '+',
                            actual,
                            Some(created),
                            Some(created_bg),
                            cx,
                        ));
                    }
                }
            }
        }

        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new("− Expected")
                            .size(LabelSize::XSmall)
                            .color(Color::Error),
                    )
                    .child(
                        Label::new("＋ Output")
                            .size(LabelSize::XSmall)
                            .color(Color::Success),
                    ),
            )
            .child(
                v_flex()
                    .rounded_sm()
                    .overflow_hidden()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .children(rows),
            )
    }

    fn render_diff_row(
        &self,
        marker: char,
        text: &str,
        color: Option<gpui::Hsla>,
        background: Option<gpui::Hsla>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = Label::new(format!("{marker} {text}"))
            .size(LabelSize::Default)
            .buffer_font(cx);
        let label = match color {
            Some(color) => label.color(Color::Custom(color)),
            None => label.color(Color::Muted),
        };
        div()
            .w_full()
            .px_1p5()
            .when_some(background, |this, background| this.bg(background))
            .child(label)
            .into_any_element()
    }

    fn render_command_output_detail_section(
        &self,
        section: &CommandOutputDetailSection,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(
                Label::new(section.title.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .p_2()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().element_background)
                    .child(
                        Label::new(section.body.clone())
                            .size(LabelSize::Default)
                            .buffer_font(cx),
                    ),
            )
    }

    fn render_step_chip(
        &self,
        index: usize,
        item: &CommandOutputItem,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = self.output_selection == OutputSelection::Step(index);
        h_flex()
            .id(("rduel-step-chip", index))
            .h(px(24.))
            .flex_none()
            .gap_1p5()
            .items_center()
            .justify_center()
            .px_2p5()
            .rounded_sm()
            .bg(if is_selected {
                cx.theme().colors().ghost_element_selected
            } else {
                cx.theme().colors().ghost_element_hover
            })
            .cursor_pointer()
            .child(
                div()
                    .size(px(8.))
                    .rounded_full()
                    .bg(status_dot_color(item.status, cx)),
            )
            .child(Label::new(item.label.clone()).size(LabelSize::Default))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.output_selection = OutputSelection::Step(index);
                cx.notify();
            }))
    }

    fn render_case_chip(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.test_cases[index]
            .result
            .as_ref()
            .map(|result| result.status)
            .unwrap_or(CommandOutputItemStatus::Pending);
        let is_selected = self.output_selection == OutputSelection::Case(index);
        h_flex()
            .id(("rduel-case-chip", index))
            .h(px(24.))
            .flex_none()
            .gap_1p5()
            .items_center()
            .justify_center()
            .px_2p5()
            .rounded_sm()
            .bg(if is_selected {
                cx.theme().colors().ghost_element_selected
            } else {
                cx.theme().colors().ghost_element_hover
            })
            .cursor_pointer()
            .child(
                div()
                    .size(px(8.))
                    .rounded_full()
                    .bg(status_dot_color(status, cx)),
            )
            .child(Label::new(format!("Input {}", index + 1)).size(LabelSize::Default))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.output_selection = OutputSelection::Case(index);
                cx.notify();
            }))
    }

    fn render_add_case_chip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .id("rduel-add-case")
            .h(px(24.))
            .flex_none()
            .gap_1p5()
            .items_center()
            .justify_center()
            .px_2p5()
            .rounded_sm()
            .bg(cx.theme().colors().ghost_element_hover)
            .hover(|this| this.bg(cx.theme().colors().ghost_element_selected))
            .cursor_pointer()
            .child(
                Icon::new(IconName::Plus)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new("Sample")
                    .size(LabelSize::Default)
                    .color(Color::Muted),
            )
            .on_click(cx.listener(|this, _, window, cx| this.add_test_case(window, cx)))
    }

    fn render_case_detail(
        &self,
        index: usize,
        case: &RduelTestCase,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let input_text = case.input.read(cx).text(cx);
        let expected_text = case.expected.read(cx).text(cx);
        let can_restore = case
            .default
            .as_ref()
            .is_some_and(|default| default.input != input_text || default.output != expected_text);
        let is_default = case.default.is_some();
        let result_diff = case
            .result
            .as_ref()
            .and_then(|result| {
                (result.status == CommandOutputItemStatus::Failed)
                    .then_some(result)
                    .and_then(|result| result.actual.as_ref())
            })
            .map(|actual| OutputDiff {
                expected: expected_text.trim_end().to_string().into(),
                actual: actual.trim_end().to_string().into(),
            });
        let stderr = case
            .result
            .as_ref()
            .and_then(|result| result.stderr.clone());

        let editor_box = |editor: Entity<Editor>, cx: &mut Context<Self>| {
            div()
                .w_full()
                .min_h(px(24.))
                .rounded_sm()
                .border_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().editor_background)
                .overflow_hidden()
                .child(self.render_search_bar_for_editor(editor.clone(), cx))
                .child(editor)
        };

        v_flex()
            .gap_2()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .child(
                                Label::new("Input")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .when(is_default, |this| {
                                        this.child(
                                            Button::new(("rduel-case-restore", index), "Restore")
                                                .disabled(!can_restore)
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.restore_test_case(index, window, cx)
                                                    },
                                                )),
                                        )
                                    })
                                    .child(
                                        Button::new(("rduel-case-delete", index), "Delete")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.delete_test_case(index, cx)
                                            })),
                                    ),
                            ),
                    )
                    .child(editor_box(case.input.clone(), cx)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Label::new("Expected")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(editor_box(case.expected.clone(), cx)),
            )
            .when_some(case.result.as_ref(), |this, result| {
                let color = match result.status {
                    CommandOutputItemStatus::Passed => Color::Success,
                    CommandOutputItemStatus::Warning => Color::Warning,
                    _ => Color::Error,
                };
                this.child(
                    Label::new(result.heading.clone())
                        .size(LabelSize::Default)
                        .color(color),
                )
            })
            .when_some(result_diff, |this, diff| {
                this.child(self.render_output_diff(&diff, cx))
            })
            .when_some(stderr, |this, stderr| {
                this.child(self.render_command_output_detail_section(
                    &CommandOutputDetailSection {
                        title: "Stderr".into(),
                        body: stderr.trim_end().to_string().into(),
                    },
                    cx,
                ))
            })
            .into_any_element()
    }

    fn add_test_case(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = Self::new_case_editor("", window, cx);
        let expected = Self::new_case_editor("", window, cx);
        self.test_cases.push(RduelTestCase {
            input,
            expected,
            default: None,
            result: None,
        });
        self.output_selection = OutputSelection::Case(self.test_cases.len() - 1);
        cx.notify();
    }

    fn delete_test_case(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.test_cases.len() {
            return;
        }
        self.test_cases.remove(index);
        self.output_selection = if self.test_cases.is_empty() {
            OutputSelection::Step(0)
        } else {
            OutputSelection::Case(index.min(self.test_cases.len() - 1))
        };
        cx.notify();
    }

    fn restore_test_case(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(case) = self.test_cases.get(index) else {
            return;
        };
        let Some(default) = case.default.clone() else {
            return;
        };
        let input = case.input.clone();
        let expected = case.expected.clone();
        input.update(cx, |editor, cx| {
            editor.set_text(default.input.as_str(), window, cx)
        });
        expected.update(cx, |editor, cx| {
            editor.set_text(default.output.as_str(), window, cx)
        });
        cx.notify();
    }

    fn open_rematch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| open_rduel(workspace, window, cx))
            .log_err();
    }

    fn render_versus_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let presence = self.presence.as_ref();
        let started_at = presence
            .map(|presence| presence.started_at_second)
            .unwrap_or(0);
        let is_finished = self.room.match_state.room_status == ServerRoomStatus::Finished;
        let elapsed = format_elapsed(started_at);
        let flash = self
            .opponent_flash
            .as_ref()
            .filter(|flash| flash.shown_at.elapsed() <= OPPONENT_FLASH_DURATION)
            .map(|flash| flash.message.clone());

        h_flex()
            .h(px(40.))
            .w_full()
            .flex_none()
            .px_3()
            .gap_3()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().elevated_surface_background)
            .child(self.render_player_presence("你", presence.and_then(|p| p.local.as_ref()), cx))
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .child(
                        Label::new("对局")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(elapsed).size(LabelSize::Default)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .when_some(flash, |this, message| {
                        this.child(
                            h_flex()
                                .h(px(22.))
                                .items_center()
                                .px_2()
                                .rounded_sm()
                                .border_1()
                                .border_color(cx.theme().status().error)
                                .child(
                                    Label::new(message)
                                        .size(LabelSize::Small)
                                        .color(Color::Error),
                                ),
                        )
                    })
                    .when(is_finished, |this| {
                        this.child(
                            Button::new("rduel-rematch", "Again")
                                .size(ButtonSize::Compact)
                                .style(ButtonStyle::Filled)
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.open_rematch(window, cx)
                                    }),
                                ),
                        )
                    })
                    .child(self.render_player_presence(
                        "对手",
                        presence.and_then(|p| p.opponent.as_ref()),
                        cx,
                    )),
            )
    }

    fn render_player_presence(
        &self,
        role: &'static str,
        presence: Option<&PlayerPresence>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let name = presence
            .map(|presence| presence.name.clone())
            .unwrap_or_else(|| "—".to_string());
        let attempts = presence.map(|p| p.activity.attempt_count).unwrap_or(0);
        let verdict = presence.and_then(|p| p.activity.last_verdict.clone());
        let last_epoch = presence.and_then(|p| p.activity.last_submission_epoch);
        let (dot_color, verdict_color) = match verdict.as_deref() {
            Some("AC") => (cx.theme().status().success, Color::Success),
            Some(_) => (cx.theme().status().error, Color::Error),
            None => (cx.theme().colors().border_variant, Color::Muted),
        };

        h_flex()
            .gap_1p5()
            .items_center()
            .child(div().size(px(7.)).rounded_full().bg(dot_color))
            .child(Label::new(role).size(LabelSize::Small).color(Color::Muted))
            .child(Label::new(name).size(LabelSize::Default))
            .when(attempts > 0, |this| {
                this.child(
                    Label::new(format!("{attempts} 次提交"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when_some(verdict, |this, verdict| {
                this.child(
                    Label::new(verdict)
                        .size(LabelSize::Small)
                        .color(verdict_color),
                )
            })
            .when_some(last_epoch, |this, epoch| {
                this.child(
                    Label::new(format_relative(epoch))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
    }

    fn render_command_output_divider(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("rduel-output-divider")
            .relative()
            .h(px(9.))
            .w_full()
            .flex_none()
            .cursor_row_resize()
            .on_drag(DraggedRduelOutputDivider, |divider, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| divider.clone())
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        this.reset_command_output_height(cx);
                    }
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(px(4.))
                    .h_px()
                    .bg(cx.theme().colors().border),
            )
    }

    fn render_split_divider(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("rduel-split-divider")
            .relative()
            .w(px(9.))
            .h_full()
            .flex_none()
            .cursor_col_resize()
            .on_drag(DraggedRduelDivider, |divider, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| divider.clone())
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        this.reset_problem_width(cx);
                    }
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(4.))
                    .w_px()
                    .bg(cx.theme().colors().border),
            )
    }
}

impl Focusable for RduelView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for RduelView {}

impl Item for RduelView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Rduel".into()
    }

    fn is_dirty(&self, cx: &App) -> bool {
        // Consider the item dirty if match is still playing to prevent accidental close
        self.room.match_state.room_status == ServerRoomStatus::Playing
            || self.has_unsaved_solution_buffers(cx)
    }

    fn can_save(&self, _cx: &App) -> bool {
        true
    }

    fn can_autosave(&self, cx: &App) -> bool {
        self.has_unsaved_solution_buffers(cx)
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        if self.main_rs_buffer.read(cx).is_dirty() {
            self.main_rs_buffer.read(cx).project_path(cx)
        } else if self.cargo_toml_buffer.read(cx).is_dirty() {
            self.cargo_toml_buffer.read(cx).project_path(cx)
        } else {
            None
        }
    }

    fn buffer_kind(&self, _cx: &App) -> ItemBufferKind {
        ItemBufferKind::Singleton
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.main_rs_editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx);
        });
        self.cargo_toml_editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx);
        });
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if TypeId::of::<Self>() == type_id {
            Some(self_handle.clone().into())
        } else if TypeId::of::<Editor>() == type_id {
            self.search_target_editor
                .clone()
                .or_else(|| self.active_code_editor())
                .map(Into::into)
        } else {
            None
        }
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        f(
            self.main_rs_buffer.entity_id(),
            self.main_rs_buffer.read(cx),
        );
        f(
            self.cargo_toml_buffer.entity_id(),
            self.cargo_toml_buffer.read(cx),
        );
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Task<anyhow::Result<()>> {
        self.save_solution_editors(options, project, window, cx)
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Task<anyhow::Result<()>> {
        let main_rs_editor = self.main_rs_editor.clone();
        let cargo_toml_editor = self.cargo_toml_editor.clone();

        cx.spawn_in(window, async move |_, cx| {
            let main_rs_reload = main_rs_editor.update_in(cx, |editor, window, cx| {
                editor.reload(project.clone(), window, cx)
            })?;
            main_rs_reload.await?;

            let cargo_toml_reload = cargo_toml_editor
                .update_in(cx, |editor, window, cx| editor.reload(project, window, cx))?;
            cargo_toml_reload.await
        })
    }

    fn on_removed(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            this.update(cx, |this, cx| {
                this.leave_active_match(cx);
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for RduelView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let markdown_style = self.problem_markdown_style(window, cx);
        self.sync_search_target(window, cx);

        v_flex()
            .key_context("Rduel")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(Self::run_samples))
            .on_action(cx.listener(Self::submit_solution))
            .on_action(cx.listener(Self::toggle_layout))
            .on_action(cx.listener(Self::select_main_rs))
            .on_action(cx.listener(Self::select_cargo_toml))
            .on_action(cx.listener(Self::select_opponent_main_rs))
            .on_action(cx.listener(Self::rename_symbol))
            .on_action(cx.listener(Self::confirm_rename))
            .child(self.render_versus_header(cx))
            .child({
                let problem = self
                    .render_problem(markdown_style, window, cx)
                    .into_any_element();
                let editor = self.render_editor(cx).into_any_element();
                let divider = self.render_split_divider(cx).into_any_element();
                h_flex()
                    .flex_1()
                    .overflow_hidden()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .on_drag_move::<DraggedRduelDivider>(cx.listener(Self::resize_problem_width))
                    .on_drag_move::<DraggedRduelOutputDivider>(
                        cx.listener(Self::resize_command_output),
                    )
                    .children(match self.layout_order {
                        LayoutOrder::ProblemLeft => vec![problem, divider, editor],
                        LayoutOrder::CodeLeft => vec![editor, divider, problem],
                    })
            })
    }
}

const STARTER_CODE: &str = r#"fn main() {}
"#;

const STARTER_ACR_PROBLEM_TOML: &str = r#"[package]
name = "rduel"
version = "0.1.0"
edition = "2024"

[package.metadata.acr]
problem_url = "https://atcoder.jp/contests/abc001/tasks/abc001_1"

[dependencies]
num = "0.4.3"
proconio = { version = "0.5.0", features = ["derive"] }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atcoder_submit_url_preselects_language_and_task() {
        assert_eq!(
            rcontest::atcoder_submit_url("https://atcoder.jp/contests/abc073/tasks/abc073_c"),
            Some("https://atcoder.jp/contests/abc073/submit?taskScreenName=abc073_c".to_string())
        );
    }

    #[test]
    fn atcoder_submit_url_handles_invalid_urls() {
        assert_eq!(rcontest::atcoder_submit_url("not a url"), None);
        assert_eq!(rcontest::atcoder_submit_url("https://example.com"), None);
        assert_eq!(
            rcontest::atcoder_submit_url("https://atcoder.jp/contests/abc073"),
            None
        );
    }

    #[test]
    fn parse_local_http_endpoint_with_ipv4() {
        let endpoint = parse_local_http_endpoint("http://127.0.0.1:8787", "/api/join").unwrap();
        assert_eq!(endpoint.host, "127.0.0.1");
        assert_eq!(endpoint.port, 8787);
        assert_eq!(endpoint.path, "/api/join");
        assert_eq!(endpoint.host_header, "127.0.0.1:8787");
    }

    #[test]
    fn parse_local_http_endpoint_with_localhost() {
        let endpoint = parse_local_http_endpoint("http://localhost:8080", "/test").unwrap();
        assert_eq!(endpoint.host, "localhost");
        assert_eq!(endpoint.port, 8080);
        assert_eq!(endpoint.path, "/test");
    }

    #[test]
    fn parse_local_http_endpoint_default_port() {
        let endpoint = parse_local_http_endpoint("http://example.com", "/api").unwrap();
        assert_eq!(endpoint.host, "example.com");
        assert_eq!(endpoint.port, 80);
    }

    #[test]
    fn parse_local_http_endpoint_with_ipv6() {
        let endpoint = parse_local_http_endpoint("http://[::1]:8080", "/api").unwrap();
        assert_eq!(endpoint.host, "::1");
        assert_eq!(endpoint.port, 8080);
    }

    #[test]
    fn parse_local_http_endpoint_rejects_non_http() {
        assert!(parse_local_http_endpoint("https://example.com", "/").is_err());
        assert!(parse_local_http_endpoint("ftp://example.com", "/").is_err());
    }

    #[test]
    fn parse_local_http_endpoint_rejects_empty_host() {
        assert!(parse_local_http_endpoint("http://", "/").is_err());
    }

    #[test]
    fn format_relative_time() {
        // Test recent times
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        assert_eq!(format_relative(now), "0秒前");
        assert_eq!(format_relative(now - 30), "30秒前");
        assert_eq!(format_relative(now - 90), "1分钟前");
        assert_eq!(format_relative(now - 150), "2分钟前");
        assert_eq!(format_relative(now - 3600), "1小时前");
        assert_eq!(format_relative(now - 7200), "2小时前");
        assert_eq!(format_relative(now - 86400), "1天前");
        assert_eq!(format_relative(now - 86400 * 3), "3天前");
        assert_eq!(format_relative(now - 86400 * 7), "1周前");
        assert_eq!(format_relative(now - 86400 * 14), "2周前");
        assert_eq!(format_relative(now - 86400 * 30), "1月前");
        assert_eq!(format_relative(now - 86400 * 60), "2月前");
        assert_eq!(format_relative(now - 86400 * 365), "1年前");
        assert_eq!(format_relative(now - 86400 * 730), "2年前");
    }

    #[test]
    fn history_outcome_shows_correct_status() {
        let server_problem = ServerProblem {
            id: "abc001_a".to_string(),
            title: "Test Problem".to_string(),
            url: "https://atcoder.jp/contests/abc001/tasks/abc001_a".to_string(),
            statement_markdown: "# Problem".to_string(),
            samples: vec![],
        };

        let entry_won = MatchHistoryEntry {
            room_id: "room1".to_string(),
            problem_id: "abc001_a".to_string(),
            problem_title: "Test Problem".to_string(),
            problem: server_problem.clone(),
            started_at_second: 0,
            finished_at_second: Some(100),
            finish_reason: Some("manual_complete".to_string()),
            winner_player_id: Some("player1".to_string()),
            players: [
                MatchHistoryPlayer {
                    player_id: "player1".to_string(),
                    atcoder_user: "user1".to_string(),
                    attempt_count: 1,
                    last_verdict: Some("AC".to_string()),
                    last_submission_epoch: Some(100),
                    submissions: vec![],
                    main_rs: None,
                    cargo_toml: None,
                },
                MatchHistoryPlayer {
                    player_id: "player2".to_string(),
                    atcoder_user: "user2".to_string(),
                    attempt_count: 0,
                    last_verdict: None,
                    last_submission_epoch: None,
                    submissions: vec![],
                    main_rs: None,
                    cargo_toml: None,
                },
            ],
        };

        assert_eq!(history_outcome(&entry_won, Some("user1")), "Win");
        assert_eq!(history_outcome(&entry_won, Some("user2")), "Loss");

        let entry_unfinished = MatchHistoryEntry {
            finished_at_second: None,
            winner_player_id: None,
            ..entry_won.clone()
        };
        assert_eq!(
            history_outcome(&entry_unfinished, Some("user1")),
            "Finished"
        );
    }

    #[test]
    fn rduel_problem_from_server_converts_correctly() {
        let server_problem = ServerProblem {
            id: "abc001_a".to_string(),
            title: "Test Problem".to_string(),
            url: "https://atcoder.jp/contests/abc001/tasks/abc001_a".to_string(),
            statement_markdown: "# Problem".to_string(),
            samples: vec![ServerSample {
                input: "1 2".to_string(),
                output: "3".to_string(),
            }],
        };

        let problem = RduelProblem::from_server(&server_problem);
        assert_eq!(problem.title, "Test Problem");
        assert_eq!(problem.markdown, "# Problem");
        assert_eq!(problem.samples.len(), 1);
        assert_eq!(problem.samples[0].input, "1 2");
        assert_eq!(problem.samples[0].output, "3");
    }
}
