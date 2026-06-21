use std::{
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::Arc,
    time::Duration,
};

use editor::{Editor, MultiBuffer};
use gpui::{
    Action, App, ClipboardItem, Context, DismissEvent, DragMoveEvent, Empty, Entity, EventEmitter,
    FocusHandle, Focusable, MouseButton, MouseDownEvent, MouseUpEvent, Render, ScrollHandle,
    SharedString, WeakEntity, Window, div, px,
};
use language::{Buffer, LanguageRegistry};
use markdown::{
    CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownFont,
    MarkdownOptions, MarkdownStyle, WrapButtonVisibility,
};
use menu::{Cancel, Confirm};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use text::{LineEnding, Rope};
use ui::{Button, ButtonSize, ButtonStyle, prelude::*};
use util::{ResultExt, rel_path::RelPath};
use workspace::{Item, ModalView, Workspace, item::ItemEvent, item::SaveOptions};
use zed_actions::rduel::OpenRduel;

const DEFAULT_PROBLEM_WIDTH_FRACTION: f32 = 0.42;
const MIN_PROBLEM_WIDTH_FRACTION: f32 = 0.25;
const MAX_PROBLEM_WIDTH_FRACTION: f32 = 0.75;
const DEFAULT_COMMAND_OUTPUT_HEIGHT: f32 = 156.0;
const MIN_COMMAND_OUTPUT_HEIGHT: f32 = 96.0;
const MAX_COMMAND_OUTPUT_HEIGHT: f32 = 360.0;
const PROBLEM_MARKDOWN_FONT_SCALE: f32 = 1.05;
const RDUEL_SERVER_URL: &str = "http://127.0.0.1:8787";

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

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &OpenRduel, window, cx| {
            open_rduel(workspace, window, cx);
        });
    })
    .detach();
}

fn open_rduel(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let workspace_handle = cx.entity().downgrade();
    workspace.toggle_modal(window, cx, |window, cx| {
        RduelMatchModal::new(workspace_handle, window, cx)
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

            if let (Some(rduel_project), Some(room)) =
                (rduel_project.as_ref(), session.room.as_ref())
                && let Err(error) = write_server_problem_to_project(rduel_project, &room.problem)
            {
                log::error!("failed to write Rduel server problem: {error:#}");
            }

            let session_for_view = session.clone();
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
        let target_path = root_path.join("target");
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
    fs.create_dir(&rduel_project.root_path).await?;
    fs.create_dir(&src_path).await?;
    fs.create_dir(&cargo_config_dir).await?;
    fs.create_dir(&rduel_project.test_path).await?;
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
    room: RoomState,
    problem: RduelProblem,
    rduel_project: Option<RduelProjectFiles>,
    problem_markdown: Entity<Markdown>,
    main_rs_buffer: Entity<Buffer>,
    cargo_toml_buffer: Entity<Buffer>,
    main_rs_editor: Entity<Editor>,
    cargo_toml_editor: Entity<Editor>,
    active_code_tab: ActiveCodeTab,
    layout_order: LayoutOrder,
    problem_width_fraction: f32,
    command_output_height: f32,
    problem_scroll_handle: ScrollHandle,
    command_output_editor: Entity<Editor>,
    command_status: CommandStatus,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActiveCodeTab {
    MainRs,
    CargoToml,
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
    local_user: SharedString,
    remote_user: SharedString,
    match_state: MatchState,
}

struct RduelProblem {
    title: SharedString,
    markdown: SharedString,
}

impl RduelProblem {
    fn from_server(problem: &ServerProblem) -> Self {
        Self {
            title: problem.title.clone().into(),
            markdown: problem.statement_markdown.clone().into(),
        }
    }
}

#[derive(Clone)]
struct RduelSession {
    player_id: String,
    room: Option<ServerRoom>,
    server_url: String,
    atcoder_user: String,
}

#[derive(Clone)]
struct MatchState {
    player_id: Option<String>,
    room_id: Option<String>,
    server_url: String,
}

#[derive(Serialize)]
struct JoinRequest {
    name: String,
    player_id: Option<String>,
}

#[derive(Serialize)]
struct SetAtCoderUserRequest {
    player_id: String,
    atcoder_user: String,
}

#[derive(Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum JoinResponse {
    Waiting { player_id: String },
    Matched { player_id: String, room: ServerRoom },
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
}

#[derive(Clone, Deserialize)]
struct ServerPlayer {
    id: String,
    name: String,
}

#[derive(Clone, Deserialize)]
struct ServerProblem {
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
    SetAtCoderUser {
        server_url: String,
        room_id: String,
        player_id: String,
        atcoder_user: String,
    },
}

enum RduelMatchOutput {
    Waiting { player_id: String },
    Matched { player_id: String, room: ServerRoom },
    RoomStatus { room: ServerRoom },
}

#[derive(Clone, Copy)]
enum CommandStatus {
    Idle,
    Running(&'static str),
    Succeeded,
    Failed,
}

impl CommandStatus {
    fn is_running(self) -> bool {
        matches!(self, Self::Running(_))
    }

    fn label(self) -> SharedString {
        match self {
            Self::Idle => "Idle".into(),
            Self::Running(label) => format!("{label} running").into(),
            Self::Succeeded => "Done".into(),
            Self::Failed => "Failed".into(),
        }
    }
}

struct RduelMatchModal {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    atcoder_user_editor: Entity<Editor>,
    server_url: String,
    player_id: Option<String>,
    status: SharedString,
    is_waiting: bool,
}

impl RduelMatchModal {
    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let atcoder_user_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("AtCoder username", window, cx);
            editor
        });
        window.focus(&atcoder_user_editor.read(cx).focus_handle(cx), cx);

        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            atcoder_user_editor,
            server_url: RDUEL_SERVER_URL.to_string(),
            player_id: None,
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

    fn handle_match_result(
        &mut self,
        result: anyhow::Result<RduelMatchOutput>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(RduelMatchOutput::Waiting { player_id }) => {
                self.player_id = Some(player_id);
                self.status = "已进入队列，等待对手...".into();
                self.poll(window, cx);
            }
            Ok(RduelMatchOutput::Matched { player_id, room }) => {
                self.status = "匹配成功，正在打开 Rduel...".into();
                self.is_waiting = false;
                let atcoder_user = self
                    .atcoder_user_editor
                    .read(cx)
                    .text(cx)
                    .trim()
                    .to_string();
                let room_id = room.id.clone();
                let session = RduelSession {
                    player_id: player_id.clone(),
                    room: Some(room),
                    server_url: self.server_url.clone(),
                    atcoder_user: atcoder_user.clone(),
                };
                let server_url = self.server_url.clone();
                cx.background_spawn(async move {
                    RduelMatchCommand::SetAtCoderUser {
                        server_url,
                        room_id,
                        player_id,
                        atcoder_user,
                    }
                    .run()
                })
                .detach_and_log_err(cx);
                let workspace = self.workspace.clone();
                let window_handle = window.window_handle();
                cx.defer(move |cx| {
                    window_handle
                        .update(cx, |_, window, cx| {
                            workspace
                                .update(cx, |workspace, cx| {
                                    workspace.hide_modal(window, cx);
                                })
                                .log_err();
                            open_rduel_session(workspace, session, window, cx);
                        })
                        .log_err();
                });
            }
            Ok(RduelMatchOutput::RoomStatus { .. }) => {}
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
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .rounded_sm()
                    .px_2()
                    .child(self.atcoder_user_editor.clone()),
            )
            .child(Label::new(self.status.clone()).color(Color::Muted))
            .child(
                h_flex().justify_end().gap_2().child(
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
                this.child(Label::new("等待服务器匹配并准备题面...").size(LabelSize::Small))
            })
            .on_action(cx.listener(|this, _: &Confirm, window, cx| {
                if !this.is_waiting {
                    this.join(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Cancel, window, cx| {
                let workspace = this.workspace.clone();
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
            }))
    }
}

impl Focusable for RduelMatchModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for RduelMatchModal {}
impl ModalView for RduelMatchModal {}

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
        return Ok(RduelCommandOutput {
            success: false,
            rendered: format!(
                "{}\n\nSubmit was stopped because local tests failed.",
                test_output.rendered
            ),
            submit_ready: None,
        });
    }

    let source_path = problem_rs_path;
    let source_code = std::fs::read_to_string(&source_path)?;
    let problem_url = read_problem_url(&cargo_toml_path)
        .ok_or_else(|| anyhow::anyhow!("problem_url was not found in Cargo.toml"))?;
    let submit_url = atcoder_submit_url(&problem_url).unwrap_or_else(|| problem_url.clone());
    Ok(RduelCommandOutput {
        success: true,
        rendered: format!(
            "{}\n\nSubmit: ready\nSource file: {}\nSubmit page: {}\n\nSource code was copied to the system clipboard. Paste it into AtCoder and submit from the browser.",
            test_output.rendered,
            source_path.display(),
            submit_url,
        ),
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

fn atcoder_submit_url(problem_url: &str) -> Option<String> {
    let (prefix, _) = problem_url.split_once("/tasks/")?;
    Some(format!("{prefix}/submit"))
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
                    JoinResponse::Waiting { player_id } => RduelMatchOutput::Waiting { player_id },
                    JoinResponse::Matched { player_id, room } => {
                        RduelMatchOutput::Matched { player_id, room }
                    }
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
                    PlayerStateResponse::Waiting { player_id } => {
                        RduelMatchOutput::Waiting { player_id }
                    }
                    PlayerStateResponse::Matched { player_id, room } => {
                        RduelMatchOutput::Matched { player_id, room }
                    }
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
            Self::SetAtCoderUser {
                server_url,
                room_id,
                player_id,
                atcoder_user,
            } => {
                let path = format!("/rooms/{room_id}/atcoder-user");
                let room: ServerRoom = rduel_http_json(
                    &server_url,
                    "POST",
                    &path,
                    Some(&SetAtCoderUserRequest {
                        player_id,
                        atcoder_user,
                    }),
                )?;
                Ok(RduelMatchOutput::RoomStatus { room })
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
    let endpoint = parse_local_http_endpoint(server_url, path)?;
    let body = match body {
        Some(body) => serde_json::to_string(body)?,
        None => String::new(),
    };
    let request = format!(
        "{method} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        endpoint.path,
        endpoint.host_header,
        body.len(),
        body
    );

    let mut stream = TcpStream::connect((endpoint.host.as_str(), endpoint.port))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    stream.write_all(request.as_bytes())?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("server returned an invalid HTTP response"))?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| anyhow::anyhow!("server returned an invalid HTTP status"))?;
    if !(200..300).contains(&status) {
        return Err(anyhow::anyhow!("server returned HTTP {status}: {body}"));
    }

    Ok(serde_json::from_str(body)?)
}

struct LocalHttpEndpoint {
    host: String,
    port: u16,
    host_header: String,
    path: String,
}

fn parse_local_http_endpoint(server_url: &str, path: &str) -> anyhow::Result<LocalHttpEndpoint> {
    let server_url = server_url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("Rduel server URL must start with http://"))?;
    let server_url = server_url.trim_end_matches('/');
    let (host, port) = match server_url.rsplit_once(':') {
        Some((host, port)) => (host, port.parse()?),
        None => (server_url, 80),
    };
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };

    Ok(LocalHttpEndpoint {
        host: host.to_string(),
        port,
        host_header: format!("{host}:{port}"),
        path,
    })
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
        let index = index + 1;
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
    Ac,
    Wa { actual: String, expected: String },
    Re { stderr: String },
}

async fn run_embedded_sample_tests(
    root_path: PathBuf,
    test_path: PathBuf,
    target_path: PathBuf,
) -> Vec<(usize, EmbeddedAcrTestResult)> {
    let mut results = Vec::new();
    let mut index = 1;

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

    if let Some(mut stdin) = child.stdin.take() {
        use smol::io::AsyncWriteExt;
        if let Err(error) = stdin.write_all(input.as_bytes()).await {
            return EmbeddedAcrTestResult::Re {
                stderr: error.to_string(),
            };
        }
    }

    let output = match child.output().await {
        Ok(output) => output,
        Err(error) => {
            return EmbeddedAcrTestResult::Re {
                stderr: error.to_string(),
            };
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        return EmbeddedAcrTestResult::Re { stderr };
    }

    if stdout.trim_end() == expected.trim_end() {
        EmbeddedAcrTestResult::Ac
    } else {
        EmbeddedAcrTestResult::Wa {
            actual: stdout,
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
            submit_ready: None,
        };
    }

    if results.is_empty() {
        return RduelCommandOutput {
            success,
            rendered: "Build: OK\nTest: no sample cases found.".into(),
            submit_ready: None,
        };
    }

    let passed = results
        .iter()
        .filter(|(_, result)| matches!(result, EmbeddedAcrTestResult::Ac))
        .count();
    let success = success && passed == results.len();

    if success {
        return RduelCommandOutput {
            success,
            rendered: format!("Build: OK\nTest: AC ({passed} cases)"),
            submit_ready: None,
        };
    }

    let mut rendered = String::from("Build: OK\nTest: Failed\n");
    rendered.push_str(&format!("{passed}/{} cases passed\n", results.len()));
    for (index, result) in results {
        match result {
            EmbeddedAcrTestResult::Ac => {}
            EmbeddedAcrTestResult::Wa { actual, expected } => {
                rendered.push_str(&format!(
                    "\nCase {index}: WA\nExpected:\n{}\n\nActual:\n{}\n",
                    expected.trim_end(),
                    actual.trim_end()
                ));
            }
            EmbeddedAcrTestResult::Re { stderr } => {
                rendered.push_str(&format!("\nCase {index}: RE\n{}\n", stderr.trim_end()));
            }
        }
    }

    RduelCommandOutput {
        success,
        rendered,
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
        submit_ready: None,
    }
}

impl RduelView {
    fn new(
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
                title: "Rduel".into(),
                markdown: "No problem was received from the server.".into(),
            });
        let room_id = session.room.as_ref().map(|room| room.id.clone());
        let remote_user = session
            .room
            .as_ref()
            .and_then(|room| {
                room.players
                    .iter()
                    .find(|player| player.id != session.player_id)
                    .map(|player| player.name.clone())
            })
            .unwrap_or_else(|| "waiting".to_string());
        let main_rs_buffer_for_view = main_rs_buffer.clone();
        let cargo_toml_buffer_for_view = cargo_toml_buffer.clone();
        let problem_markdown =
            Self::new_problem_markdown(problem.markdown.clone(), language_registry, cx);

        let main_rs_multibuffer = cx
            .new(|cx| MultiBuffer::singleton(main_rs_buffer, cx).with_title("src/main.rs".into()));
        let main_rs_editor = cx.new(|cx| {
            Editor::for_multibuffer(main_rs_multibuffer, Some(project.clone()), window, cx)
        });
        let cargo_toml_multibuffer = cx.new(|cx| {
            MultiBuffer::singleton(cargo_toml_buffer, cx).with_title("Cargo.toml".into())
        });
        let cargo_toml_editor = cx.new(|cx| {
            Editor::for_multibuffer(cargo_toml_multibuffer, Some(project.clone()), window, cx)
        });
        let command_output_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_text("Test / Submit output will appear here.", window, cx);
            editor.set_read_only(true);
            editor
        });

        let view = Self {
            focus_handle: cx.focus_handle(),
            project,
            room: RoomState {
                local_user: format!("{}：作答中", session.atcoder_user).into(),
                remote_user: format!("{remote_user}：作答中").into(),
                match_state: MatchState {
                    player_id: Some(session.player_id),
                    room_id,
                    server_url: session.server_url,
                },
            },
            problem,
            rduel_project,
            problem_markdown,
            main_rs_buffer: main_rs_buffer_for_view,
            cargo_toml_buffer: cargo_toml_buffer_for_view,
            main_rs_editor,
            cargo_toml_editor,
            active_code_tab: ActiveCodeTab::MainRs,
            layout_order: LayoutOrder::ProblemLeft,
            problem_width_fraction: DEFAULT_PROBLEM_WIDTH_FRACTION,
            command_output_height: DEFAULT_COMMAND_OUTPUT_HEIGHT,
            problem_scroll_handle: ScrollHandle::new(),
            command_output_editor,
            command_status: CommandStatus::Idle,
        };
        view.poll_room_after_delay(cx);
        view
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
                "Rduel project files are not available. Reopen Rduel after checking ~/.rduel.",
                window,
                cx,
            );
            cx.notify();
            return;
        };

        self.spawn_rduel_command(
            "Test",
            rduel_project.clone(),
            RduelCommand::Test {
                root_path: rduel_project.root_path.clone(),
                test_path: rduel_project.test_path.clone(),
                target_path: rduel_project.target_path.clone(),
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

    fn submit_solution(&mut self, _: &SubmitSolution, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rduel_project) = self.rduel_project.clone() else {
            self.command_status = CommandStatus::Failed;
            self.set_command_output(
                "Rduel project files are not available. Reopen Rduel after checking ~/.rduel.",
                window,
                cx,
            );
            cx.notify();
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

        self.command_status = CommandStatus::Running(label);
        self.set_command_output(format!("{label} is running..."), window, cx);
        cx.notify();

        let save_task =
            self.save_solution_editors(SaveOptions::default(), self.project.clone(), window, cx);

        cx.spawn(async move |this, cx| {
            let result = async {
                save_task.await?;
                cx.background_spawn(async move { command.run().await })
                    .await
            }
            .await;
            this.update_in(cx, |this, window, cx| {
                let mut output_text = match result {
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
                            cx.open_url(&submit_ready.submit_url);
                            log::info!(
                                "prepared Rduel submit for {}",
                                submit_ready.source_path.display()
                            );
                        }
                        output.rendered
                    }
                    Err(error) => {
                        this.command_status = CommandStatus::Failed;
                        format!("Failed before running command:\n{error:#}")
                    }
                };

                if !rduel_project.root_path.exists() {
                    output_text.push_str(&format!(
                        "\n\nWarning: Rduel project directory no longer exists: {}",
                        rduel_project.root_path.display()
                    ));
                }

                this.set_command_output(output_text, window, cx);
                cx.notify();
            })
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

            this.update(cx, |this, cx| {
                match result {
                    Ok(RduelMatchOutput::RoomStatus { room }) => {
                        if this.apply_room_status(room) {
                            this.poll_room_after_delay(cx);
                        }
                    }
                    Ok(RduelMatchOutput::Waiting { .. } | RduelMatchOutput::Matched { .. }) => {
                        this.poll_room_after_delay(cx);
                    }
                    Err(error) => {
                        log::warn!("failed to poll Rduel room: {error:#}");
                        this.poll_room_after_delay(cx);
                    }
                }
                cx.notify();
            })?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn apply_room_status(&mut self, room: ServerRoom) -> bool {
        if room.status != ServerRoomStatus::Finished {
            return true;
        }

        let winner = room.winner_player_id.as_deref();
        let local_name = room
            .players
            .iter()
            .find(|player| Some(player.id.as_str()) == self.room.match_state.player_id.as_deref())
            .map(|player| player.name.as_str())
            .unwrap_or("local");
        let remote_name = room
            .players
            .iter()
            .find(|player| Some(player.id.as_str()) != self.room.match_state.player_id.as_deref())
            .map(|player| player.name.as_str())
            .unwrap_or("opponent");
        if winner == self.room.match_state.player_id.as_deref() {
            self.room.local_user = format!("{local_name}：AC").into();
            self.room.remote_user = format!("{remote_name}：结束").into();
        } else if winner.is_some() {
            self.room.local_user = format!("{local_name}：结束").into();
            self.room.remote_user = format!("{remote_name}：AC").into();
        } else {
            self.room.local_user = format!("{local_name}：结束").into();
            self.room.remote_user = format!("{remote_name}：结束").into();
        }
        false
    }

    fn set_command_output(
        &mut self,
        output: impl Into<Arc<str>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.command_output_editor.update(cx, |editor, cx| {
            editor.set_read_only(false);
            editor.set_text(output, window, cx);
            editor.set_read_only(true);
        });
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
        let pointer_y = event.event.position.y.as_f32();
        let command_output_height =
            (bounds_bottom - pointer_y).clamp(MIN_COMMAND_OUTPUT_HEIGHT, MAX_COMMAND_OUTPUT_HEIGHT);

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

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .h(px(40.))
            .px_4()
            .gap_2()
            .items_center()
            .justify_between()
            .overflow_hidden()
            .child(Label::new("Rduel").size(LabelSize::Large))
            .child(
                h_flex()
                    .flex_1()
                    .gap_2()
                    .justify_center()
                    .overflow_hidden()
                    .child(self.render_status_chip(self.room.local_user.clone(), cx))
                    .child(self.render_status_chip(self.room.remote_user.clone(), cx)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("rduel-toggle-layout", "Swap")
                            .size(ButtonSize::Compact)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_layout(&ToggleLayout, window, cx);
                            })),
                    )
                    .child(
                        Button::new("rduel-run-samples", "Test")
                            .size(ButtonSize::Compact)
                            .disabled(self.command_status.is_running())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.run_samples(&RunSamples, window, cx);
                            })),
                    )
                    .child(
                        Button::new("rduel-submit", "Submit")
                            .size(ButtonSize::Compact)
                            .style(ButtonStyle::Filled)
                            .disabled(self.command_status.is_running())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_solution(&SubmitSolution, window, cx);
                            })),
                    ),
            )
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
                v_flex()
                    .h(px(40.))
                    .justify_center()
                    .px_4()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(Label::new(self.problem.title.clone()).size(LabelSize::Small)),
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
        };

        v_flex()
            .flex_1()
            .h_full()
            .overflow_hidden()
            .child(self.render_code_tabs(cx))
            .child(div().flex_1().overflow_hidden().child(active_editor))
            .child(self.render_command_output_divider(cx))
            .child(self.render_command_output(cx))
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
    }

    fn render_code_tab(
        &self,
        label: &'static str,
        tab: ActiveCodeTab,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = format!("rduel-code-tab-{label}");
        let is_dirty = match tab {
            ActiveCodeTab::MainRs => self.main_rs_buffer.read(cx).is_dirty(),
            ActiveCodeTab::CargoToml => self.cargo_toml_buffer.read(cx).is_dirty(),
        };
        let label = if is_dirty {
            format!("{label} *").into()
        } else {
            SharedString::from(label)
        };

        Button::new(id, label)
            .size(ButtonSize::Compact)
            .style(if self.active_code_tab == tab {
                ButtonStyle::Filled
            } else {
                ButtonStyle::Subtle
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                match tab {
                    ActiveCodeTab::MainRs => this.select_main_rs(&SelectMainRs, window, cx),
                    ActiveCodeTab::CargoToml => {
                        this.select_cargo_toml(&SelectCargoToml, window, cx)
                    }
                };
            }))
    }

    fn render_status_chip(&self, text: SharedString, cx: &App) -> impl IntoElement {
        div()
            .px_1p5()
            .py_0p5()
            .rounded_sm()
            .bg(cx.theme().colors().element_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new(text).size(LabelSize::Small).truncate())
    }

    fn render_command_output(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .h(px(self.command_output_height))
            .flex_none()
            .bg(cx.theme().colors().editor_background)
            .child(
                h_flex()
                    .h(px(28.))
                    .items_center()
                    .justify_between()
                    .px_3()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(Label::new("Output").size(LabelSize::Small))
                    .child(self.render_status_chip(self.command_status.label(), cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.command_output_editor.clone()),
            )
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
        self.has_unsaved_solution_buffers(cx)
    }

    fn can_save(&self, _cx: &App) -> bool {
        true
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

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for RduelView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let markdown_style = self.problem_markdown_style(window, cx);

        v_flex()
            .key_context("Rduel")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(Self::run_samples))
            .on_action(cx.listener(Self::submit_solution))
            .on_action(cx.listener(Self::toggle_layout))
            .on_action(cx.listener(Self::select_main_rs))
            .on_action(cx.listener(Self::select_cargo_toml))
            .child(self.render_header(cx))
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

const STARTER_CODE: &str = r#"use std::io::{self, Read};

fn main() {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).unwrap();
    let mut it = input.split_whitespace();

}
"#;

const STARTER_ACR_PROBLEM_TOML: &str = r#"[package]
name = "rduel"
version = "0.1.0"
edition = "2024"

[package.metadata.acr]
problem_url = "https://atcoder.jp/contests/abc001/tasks/abc001_1"

[dependencies]
num = "=0.4.3"
proconio = { version = "=0.5.0", features = ["derive"] }
"#;
