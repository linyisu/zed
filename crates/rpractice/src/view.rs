use std::{
    any::TypeId,
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context as _;
use editor::{
    Editor, EditorMode, MultiBuffer, SizingBehavior,
    actions::{ConfirmRename, Rename},
};
use gpui::{
    Action, AnyElement, App, Bounds, ClipboardItem, Context, Div, DragMoveEvent, ElementId, Empty,
    Entity, EntityId, EventEmitter, FocusHandle, Focusable, ImageSource, MouseButton,
    MouseDownEvent, MouseUpEvent, Render, Resource, ScrollHandle, SharedString, SharedUri,
    Stateful, Subscription, UniformListScrollHandle, WeakEntity, Window, div, point, px, size,
    uniform_list,
};
use language::{Buffer, LanguageRegistry};
use markdown::{
    CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownFont,
    MarkdownOptions, MarkdownStyle, WrapButtonVisibility,
};
use project::{Project, ProjectItem, ProjectPath};
use rcontest::output::{
    CaseResult, CommandOutputDetail, CommandOutputDetailSection, CommandOutputItem,
    CommandOutputItemStatus, CommandOutputState, OutputDiff, OutputSelection, sample_test_summary,
    single_command_output_state, status_dot_color,
};
use schemars::JsonSchema;
use search::BufferSearchBar;
use serde::Deserialize;
use settings::SettingsStore;
use smallvec::SmallVec;
use ui::{
    Button, Icon, IconName, IconSize, IndentGuideColors, Label, LabelSize, ListItem,
    ListItemSpacing, RenderedIndentGuide, StickyCandidate, WithScrollbar, prelude::*, v_flex,
};
use util::{ResultExt, rel_path::RelPath};
use workspace::{
    Item, ToolbarItemEvent, ToolbarItemView, Workspace,
    item::{ItemBufferKind, ItemEvent, SaveOptions},
};

use crate::{
    fetcher::{self, Problem, ProblemDetail, Sample},
    storage::{PracticeStorage, default_db_path},
};

const DEFAULT_PROBLEM_WIDTH_FRACTION: f32 = 0.42;
const MIN_PROBLEM_WIDTH_FRACTION: f32 = 0.25;
const MAX_PROBLEM_WIDTH_FRACTION: f32 = 0.75;
const DEFAULT_SIDEBAR_WIDTH: f32 = 340.0;
const MIN_SIDEBAR_WIDTH: f32 = 240.0;
const MAX_SIDEBAR_WIDTH: f32 = 560.0;
const DEFAULT_OUTPUT_HEIGHT: f32 = 280.0;
const MIN_OUTPUT_HEIGHT: f32 = 28.0;
const CODE_AREA_TOP_RESERVE: f32 = 50.0;
const PROBLEM_MARKDOWN_FONT_SCALE: f32 = 1.12;
const SAMPLE_TEST_TIMEOUT: Duration = Duration::from_secs(3);
const SUBMISSION_FETCH_LIMIT: usize = 10;
const STARTER_CARGO_TOML_DEPENDENCIES: &str = r#"[dependencies]
num = "0.4.3"
proconio = { version = "0.5.0", features = ["derive"] }
"#;

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = rpractice)]
pub struct RunSamples;

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = rpractice)]
pub struct SubmitSolution;

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = rpractice)]
pub struct RefreshSubmissions;

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = rpractice)]
pub struct AddTestCase;

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = rpractice)]
pub struct ToggleSidebar;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpracticeProblemFiles {
    root_path: PathBuf,
    source_path: PathBuf,
    source_file_name: String,
    cargo_toml_path: PathBuf,
    cargo_config_path: PathBuf,
    target_path: PathBuf,
    bin_name: String,
}

pub struct RpracticeView {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    language_registry: Arc<LanguageRegistry>,
    storage: Option<PracticeStorage>,
    problems: Vec<Problem>,
    filtered_problem_indices: Vec<usize>,
    selected_problem: Option<Problem>,
    selected_files: Option<RpracticeProblemFiles>,
    problem_markdown: Option<Entity<Markdown>>,
    source_buffer: Option<Entity<Buffer>>,
    source_editor: Option<Entity<Editor>>,
    query_editor: Entity<Editor>,
    expanded_groups: HashSet<String>,
    expanded_contests: HashSet<(String, String)>,
    problem_list_scroll: UniformListScrollHandle,
    problem_scroll_handle: ScrollHandle,
    sidebar_open: bool,
    sidebar_width: f32,
    output_height: f32,
    problem_width_fraction: f32,
    command_status: CommandStatus,
    command_output: CommandOutputState,
    test_cases: Vec<PracticeTestCase>,
    output_selection: OutputSelection,
    is_fetching_submissions: bool,
    submission_fetch_problem_id: Option<String>,
    submission_watch_started_at: Option<i64>,
    submission_watch_problem_id: Option<String>,
    search_target_editor: Option<Entity<Editor>>,
    search_bar_subscriptions: Option<(EntityId, Vec<Subscription>)>,
    status: SharedString,
    is_loading_list: bool,
    is_loading_problem: bool,
    session_id: Option<String>,
    session_started_at: Option<i64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
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

struct PracticeTestCase {
    input: Entity<Editor>,
    expected: Entity<Editor>,
    default: Option<Sample>,
    result: Option<CaseResult>,
}

#[derive(Clone)]
enum ProblemListEntry {
    Group {
        group: String,
        is_expanded: bool,
    },
    Contest {
        group: String,
        segment: String,
        is_expanded: bool,
    },
    Problem {
        problem_index: usize,
    },
}

impl ProblemListEntry {
    fn depth(&self) -> usize {
        match self {
            Self::Group { .. } => 0,
            Self::Contest { .. } => 1,
            Self::Problem { .. } => 2,
        }
    }
}

#[derive(Clone)]
struct ProblemListStickyCandidate {
    index: usize,
    depth: usize,
}

impl StickyCandidate for ProblemListStickyCandidate {
    fn depth(&self) -> usize {
        self.depth
    }
}

#[derive(Clone)]
struct DraggedProblemDivider;

impl Render for DraggedProblemDivider {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

#[derive(Clone)]
struct DraggedSidebarDivider;

impl Render for DraggedSidebarDivider {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

#[derive(Clone)]
struct DraggedOutputDivider;

impl Render for DraggedOutputDivider {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

enum PracticeCommand {
    Test {
        files: RpracticeProblemFiles,
        cases: Vec<(String, String)>,
    },
    Submit {
        files: RpracticeProblemFiles,
        cases: Vec<(String, String)>,
        problem_url: String,
    },
}

struct PracticeCommandOutput {
    success: bool,
    items: Vec<CommandOutputItem>,
    case_results: Vec<(usize, CaseResult)>,
    submit_ready: Option<PracticeSubmitReady>,
}

struct PracticeSubmitReady {
    source_code: String,
    source_path: PathBuf,
    submit_url: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct LocalRduelConfig {
    atcoder_user: Option<String>,
}

impl PracticeCommand {
    async fn run(self) -> anyhow::Result<PracticeCommandOutput> {
        match self {
            Self::Test { files, cases } => run_practice_test(files, cases).await,
            Self::Submit {
                files,
                cases,
                problem_url,
            } => run_practice_submit(files, cases, problem_url).await,
        }
    }
}

async fn run_practice_test(
    files: RpracticeProblemFiles,
    cases: Vec<(String, String)>,
) -> anyhow::Result<PracticeCommandOutput> {
    let solution = rcontest::runner::RustSolution {
        root_path: files.root_path,
        target_path: files.target_path,
        bin_name: Some(files.bin_name),
    };
    let cases = cases
        .into_iter()
        .map(|(input, expected)| rcontest::runner::SampleCase { input, expected })
        .collect();
    let report =
        rcontest::runner::run_rust_sample_tests(solution, cases, SAMPLE_TEST_TIMEOUT).await?;
    Ok(render_practice_test_report(report))
}

async fn run_practice_submit(
    files: RpracticeProblemFiles,
    cases: Vec<(String, String)>,
    problem_url: String,
) -> anyhow::Result<PracticeCommandOutput> {
    let mut output = run_practice_test(files.clone(), cases).await?;
    let submission = rcontest::prepare_atcoder_submission(files.source_path, &problem_url)?;
    output.submit_ready = Some(PracticeSubmitReady {
        source_code: submission.source_code,
        source_path: submission.source_path,
        submit_url: submission.submit_url,
    });
    Ok(output)
}

fn render_practice_test_report(
    report: rcontest::runner::SampleTestReport,
) -> PracticeCommandOutput {
    let code = report
        .build
        .status
        .code()
        .map_or_else(|| "signal".to_string(), |code| code.to_string());
    let heading = format!(
        "{} finished with exit {code}\n{} {}",
        report.build.label,
        report.build.executable.display(),
        report.build.args.join(" ")
    );
    let summary = sample_test_summary(report, heading);
    if !summary.build_succeeded {
        return PracticeCommandOutput {
            success: false,
            items: vec![summary.build_item],
            case_results: Vec::new(),
            submit_ready: None,
        };
    }

    let success = summary.passed_cases == summary.total_cases;
    let mut items = vec![summary.build_item];
    if summary.total_cases == 0 {
        items.push(CommandOutputItem {
            label: "Samples".into(),
            status: CommandOutputItemStatus::Warning,
            detail: Some(CommandOutputDetail::new("No sample cases are available.")),
        });
    }

    PracticeCommandOutput {
        success,
        items,
        case_results: summary.case_results,
        submit_ready: None,
    }
}

fn submissions_output_item(
    atcoder_user: &str,
    problem: &Problem,
    result: anyhow::Result<Vec<fetcher::ProblemSubmission>>,
) -> CommandOutputItem {
    match result {
        Ok(submissions) if submissions.is_empty() => CommandOutputItem {
            label: "Submissions".into(),
            status: CommandOutputItemStatus::Warning,
            detail: Some(CommandOutputDetail::new(format!(
                "No recent submissions found for {atcoder_user} on {}.",
                problem.id
            ))),
        },
        Ok(submissions) => {
            let has_ac = submissions
                .iter()
                .any(|submission| submission.verdict.trim() == "AC");
            CommandOutputItem {
                label: "Submissions".into(),
                status: if has_ac {
                    CommandOutputItemStatus::Passed
                } else {
                    CommandOutputItemStatus::Warning
                },
                detail: Some(CommandOutputDetail::with_sections(
                    format!(
                        "Fetched {} recent submission{} for {atcoder_user} on {}.",
                        submissions.len(),
                        if submissions.len() == 1 { "" } else { "s" },
                        problem.id
                    ),
                    vec![CommandOutputDetailSection {
                        title: "Recent submissions".into(),
                        body: format_submission_rows(&submissions).into(),
                    }],
                )),
            }
        }
        Err(error) => CommandOutputItem {
            label: "Submissions".into(),
            status: CommandOutputItemStatus::Failed,
            detail: Some(CommandOutputDetail::new(format!(
                "Could not fetch submissions:\n{error:#}"
            ))),
        },
    }
}

fn format_submission_rows(submissions: &[fetcher::ProblemSubmission]) -> String {
    submissions
        .iter()
        .map(|submission| {
            let verdict = if submission.verdict.trim().is_empty() {
                "Unknown"
            } else {
                submission.verdict.trim()
            };
            format!(
                "{verdict:<8} {:<8} #{}  {}",
                format_relative(submission.epoch_second),
                submission.id,
                submission.url
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn first_submission_since(
    submissions: &[fetcher::ProblemSubmission],
    from_second: i64,
) -> Option<&fetcher::ProblemSubmission> {
    submissions
        .iter()
        .filter(|submission| submission.epoch_second >= from_second)
        .min_by_key(|submission| (submission.epoch_second, submission.id))
}

fn detected_submission_output_item(submission: &fetcher::ProblemSubmission) -> CommandOutputItem {
    let verdict = if submission.verdict.trim().is_empty() {
        "Unknown"
    } else {
        submission.verdict.trim()
    };
    CommandOutputItem {
        label: "Submissions".into(),
        status: if verdict == "AC" {
            CommandOutputItemStatus::Passed
        } else {
            CommandOutputItemStatus::Warning
        },
        detail: Some(CommandOutputDetail::with_sections(
            format!("Detected AtCoder submission: {verdict}"),
            vec![CommandOutputDetailSection {
                title: "Submission".into(),
                body: format!(
                    "{verdict:<8} {}  #{}  {}",
                    format_relative(submission.epoch_second),
                    submission.id,
                    submission.url
                )
                .into(),
            }],
        )),
    }
}

fn configured_atcoder_user(cx: &App) -> Option<String> {
    if let Ok(atcoder_user) = std::env::var("ATCODER_USER")
        && !atcoder_user.trim().is_empty()
    {
        return Some(atcoder_user.trim().to_string());
    }

    if let Some(atcoder_user) = cx
        .try_global::<SettingsStore>()
        .and_then(|store| store.raw_user_settings())
        .and_then(|settings| settings.content.rduel.as_ref())
        .and_then(|rduel| rduel.atcoder_user.clone())
        .filter(|atcoder_user| !atcoder_user.trim().is_empty())
    {
        return Some(atcoder_user.trim().to_string());
    }

    let path = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)?
        .join(".rduel")
        .join("config.json");
    let config = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<LocalRduelConfig>(&text).ok())?;
    config
        .atcoder_user
        .filter(|atcoder_user| !atcoder_user.trim().is_empty())
        .map(|atcoder_user| atcoder_user.trim().to_string())
}

fn format_relative(epoch_second: i64) -> String {
    let delta = (unix_now() - epoch_second).max(0);
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;

    if delta < MINUTE {
        format!("{}s ago", delta)
    } else if delta < HOUR {
        format!("{}m ago", delta / MINUTE)
    } else if delta < DAY {
        format!("{}h ago", delta / HOUR)
    } else if delta < WEEK {
        format!("{}d ago", delta / DAY)
    } else if delta < MONTH {
        format!("{}w ago", delta / WEEK)
    } else if delta < YEAR {
        format!("{}mo ago", delta / MONTH)
    } else {
        format!("{}y ago", delta / YEAR)
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

impl RpracticeView {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        language_registry: Arc<LanguageRegistry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search title or id", window, cx);
            editor
        });
        cx.subscribe(&query_editor, Self::on_filter_editor_event)
            .detach();

        let mut view = Self {
            focus_handle: cx.focus_handle(),
            workspace,
            project,
            language_registry,
            storage: None,
            problems: Vec::new(),
            filtered_problem_indices: Vec::new(),
            selected_problem: None,
            selected_files: None,
            problem_markdown: None,
            source_buffer: None,
            source_editor: None,
            query_editor,
            expanded_groups: HashSet::default(),
            expanded_contests: HashSet::default(),
            problem_list_scroll: UniformListScrollHandle::new(),
            problem_scroll_handle: ScrollHandle::new(),
            sidebar_open: true,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            output_height: DEFAULT_OUTPUT_HEIGHT,
            problem_width_fraction: DEFAULT_PROBLEM_WIDTH_FRACTION,
            command_status: CommandStatus::Idle,
            command_output: CommandOutputState::empty(),
            test_cases: Vec::new(),
            output_selection: OutputSelection::Step(0),
            is_fetching_submissions: false,
            submission_fetch_problem_id: None,
            submission_watch_started_at: None,
            submission_watch_problem_id: None,
            search_target_editor: None,
            search_bar_subscriptions: None,
            status: "Loading problem list...".into(),
            is_loading_list: true,
            is_loading_problem: false,
            session_id: None,
            session_started_at: None,
        };
        view.load_problem_list(cx);
        view
    }

    fn on_filter_editor_event(
        &mut self,
        _: Entity<Editor>,
        event: &editor::EditorEvent,
        cx: &mut Context<Self>,
    ) {
        if matches!(
            event,
            editor::EditorEvent::BufferEdited | editor::EditorEvent::Edited { .. }
        ) {
            self.refresh_filtered_problems(cx);
        }
    }

    fn load_problem_list(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let storage = PracticeStorage::open(default_db_path()).await;
            this.update(cx, |this, cx| {
                match storage {
                    Ok(storage) => {
                        let cached = storage.load_problem_list().log_err().unwrap_or_default();
                        this.storage = Some(storage.clone());
                        if cached.is_empty() || cached.iter().any(problem_needs_list_refresh) {
                            this.fetch_problem_list(cx);
                        } else {
                            let count = cached.len();
                            this.problems = cached;
                            this.is_loading_list = false;
                            this.status = format!("Loaded {count} cached problems.").into();
                            this.refresh_filtered_problems(cx);
                        }
                    }
                    Err(error) => {
                        this.is_loading_list = false;
                        this.status = format!("Could not open Rpractice storage: {error:#}").into();
                        log::warn!("failed to open Rpractice storage: {error:#}");
                    }
                }
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn fetch_problem_list(&mut self, cx: &mut Context<Self>) {
        self.is_loading_list = true;
        self.status = "Fetching problem index...".into();
        let storage = self.storage.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async { fetcher::fetch_problem_list() })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(problems) => {
                        if let Some(storage) = storage {
                            let problems_to_store = problems.clone();
                            cx.spawn(async move |_, _cx| {
                                storage.save_problem_list(problems_to_store).await?;
                                anyhow::Ok(())
                            })
                            .detach_and_log_err(cx);
                        }
                        let count = problems.len();
                        this.problems = problems;
                        this.is_loading_list = false;
                        this.status = format!("Loaded {count} problems.").into();
                        this.refresh_filtered_problems(cx);
                    }
                    Err(error) => {
                        this.is_loading_list = false;
                        this.status = format!("Could not fetch problem list: {error:#}").into();
                        log::warn!("failed to fetch Rpractice problem list: {error:#}");
                    }
                }
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn refresh_filtered_problems(&mut self, cx: &mut Context<Self>) {
        let query = self.query_editor.read(cx).text(cx).to_lowercase();

        self.filtered_problem_indices = self
            .problems
            .iter()
            .enumerate()
            .filter_map(|(index, problem)| {
                if !query.trim().is_empty() {
                    let haystack = format!(
                        "{} {} {} {} {} {}",
                        problem.contest_group,
                        problem.contest_segment,
                        problem.task_index,
                        problem.id,
                        problem.title,
                        problem.contest_title
                    )
                    .to_lowercase();
                    if !haystack.contains(query.trim()) {
                        return None;
                    }
                }
                Some(index)
            })
            .collect();
        self.filtered_problem_indices.sort_by(|left, right| {
            let left = &self.problems[*left];
            let right = &self.problems[*right];
            (
                left.contest_group.as_str(),
                left.contest_segment.as_str(),
                left.task_index.as_str(),
                left.id.as_str(),
            )
                .cmp(&(
                    right.contest_group.as_str(),
                    right.contest_segment.as_str(),
                    right.task_index.as_str(),
                    right.id.as_str(),
                ))
        });
        cx.notify();
    }

    fn open_problem_at_index(
        &mut self,
        problem_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(problem) = self.problems.get(problem_index).cloned() else {
            return;
        };

        self.is_loading_problem = true;
        self.status = format!("Fetching statement for {}...", problem.id).into();
        self.selected_problem = Some(problem.clone());
        cx.notify();

        let storage = self.storage.clone();
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let prepared = cx
                .background_spawn({
                    let problem = problem.clone();
                    let storage = storage.clone();
                    async move {
                        let files = prepare_problem_files(&problem)?;
                        let detail = match storage
                            .as_ref()
                            .and_then(|storage| storage.load_problem_detail(&problem.id).log_err())
                            .flatten()
                        {
                            Some(cached_detail) => {
                                if cached_detail.normalized
                                    && let Some(storage) = storage.as_ref()
                                {
                                    storage
                                        .normalize_cached_problem_detail(
                                            problem.id.clone(),
                                            cached_detail.detail.statement_markdown.clone(),
                                        )
                                        .await
                                        .log_err();
                                }
                                cached_detail.detail
                            }
                            None => {
                                let detail = fetcher::fetch_problem_detail(&problem.url)?;
                                if let Some(storage) = storage {
                                    storage
                                        .save_problem_detail(problem.clone(), detail.clone())
                                        .await
                                        .log_err();
                                }
                                detail
                            }
                        };
                        anyhow::Ok((problem, files, detail))
                    }
                })
                .await;

            let (problem, files, detail) = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.is_loading_problem = false;
                        this.status = format!("Could not open problem: {error:#}").into();
                        log::warn!("failed to open Rpractice problem: {error:#}");
                        cx.notify();
                    })?;
                    return anyhow::Ok(());
                }
            };

            let Some(source_buffer) =
                open_problem_source_buffer(workspace.clone(), &files, cx).await
            else {
                this.update(cx, |this, cx| {
                    this.is_loading_problem = false;
                    this.status = "Could not open the local source buffer.".into();
                    cx.notify();
                })?;
                return anyhow::Ok(());
            };

            this.update_in(cx, |this, window, cx| {
                this.apply_opened_problem(problem, files, detail, source_buffer, window, cx);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn apply_opened_problem(
        &mut self,
        mut problem: Problem,
        files: RpracticeProblemFiles,
        detail: ProblemDetail,
        source_buffer: Entity<Buffer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(title) = detail
            .title
            .as_ref()
            .filter(|title| !title.trim().is_empty())
        {
            problem.title = title.clone();
        }

        let markdown = cx.new(|cx| {
            Markdown::new_with_options(
                detail.statement_markdown.clone().into(),
                Some(self.language_registry.clone()),
                None,
                MarkdownOptions {
                    parse_html: true,
                    render_math: true,
                    parse_heading_slugs: true,
                    ..Default::default()
                },
                cx,
            )
        });
        let multibuffer = cx.new(|cx| {
            MultiBuffer::singleton(source_buffer.clone(), cx)
                .with_title(files.source_file_name.clone().into())
        });
        let source_editor = cx.new(|cx| {
            let mut editor =
                Editor::for_multibuffer(multibuffer, Some(self.project.clone()), window, cx);
            editor.set_should_serialize_selection_changes(false);
            editor
        });

        self.selected_problem = Some(problem.clone());
        self.selected_files = Some(files);
        self.problem_markdown = Some(markdown);
        self.source_buffer = Some(source_buffer);
        self.source_editor = Some(source_editor.clone());
        self.test_cases = detail
            .samples
            .iter()
            .map(|sample| PracticeTestCase {
                input: Self::new_case_editor(&sample.input, window, cx),
                expected: Self::new_case_editor(&sample.output, window, cx),
                default: Some(sample.clone()),
                result: None,
            })
            .collect();
        self.output_selection = if self.test_cases.is_empty() {
            OutputSelection::Step(0)
        } else {
            OutputSelection::Case(0)
        };
        self.is_fetching_submissions = false;
        self.command_output = CommandOutputState::empty();
        self.command_status = CommandStatus::Idle;
        self.is_loading_problem = false;
        self.status = SharedString::default();
        self.search_target_editor = Some(source_editor);
        self.start_practice_session(problem.id, cx);
        self.fetch_submissions(cx);
        cx.notify();
    }

    fn start_practice_session(&mut self, problem_id: String, cx: &mut Context<Self>) {
        let Some(storage) = self.storage.clone() else {
            return;
        };
        let started_at = unix_now();
        self.session_started_at = Some(started_at);
        cx.spawn(async move |this, cx| {
            let session_id = storage.start_session(problem_id, started_at).await?;
            this.update(cx, |this, cx| {
                this.session_id = Some(session_id);
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn finish_practice_session(&mut self, cx: &mut Context<Self>) {
        let (Some(storage), Some(session_id), Some(started_at)) = (
            self.storage.clone(),
            self.session_id.take(),
            self.session_started_at.take(),
        ) else {
            return;
        };
        let finished_at = unix_now();
        let duration_seconds = finished_at.saturating_sub(started_at);
        cx.spawn(async move |_, _cx| {
            storage
                .finish_session(session_id, finished_at, duration_seconds)
                .await
        })
        .detach_and_log_err(cx);
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
            editor.set_show_gutter(false, cx);
            editor.disable_scrollbars_and_minimap(window, cx);
            editor.set_forbid_vertical_scroll(true);
            editor.set_soft_wrap_mode(language::language_settings::SoftWrap::None, cx);
            editor
        })
    }

    fn test_case_texts(&self, cx: &App) -> Vec<(String, String)> {
        self.test_cases
            .iter()
            .map(|case| {
                (
                    case.input.read(cx).text(cx),
                    case.expected.read(cx).text(cx),
                )
            })
            .collect()
    }

    fn save_source_editor(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Task<anyhow::Result<()>> {
        let Some(source_editor) = self.source_editor.clone() else {
            return gpui::Task::ready(Err(anyhow::anyhow!("no Rpractice source editor is open")));
        };

        cx.spawn_in(window, async move |this, cx| {
            let save = source_editor.update_in(cx, |editor, window, cx| {
                editor.save(options, project, window, cx)
            })?;
            save.await?;
            this.update(cx, |_this, cx| cx.notify())?;
            Ok(())
        })
    }

    fn focus_after_run(&mut self) {
        if let Some(index) = self.test_cases.iter().position(|case| {
            matches!(&case.result, Some(result) if result.status == CommandOutputItemStatus::Failed)
        }) {
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

    fn spawn_practice_command(
        &mut self,
        label: &'static str,
        command: PracticeCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.command_status.is_running() {
            return;
        }

        self.command_status = CommandStatus::Running;
        self.output_selection = OutputSelection::Step(0);
        self.command_output = CommandOutputState::running(label);
        for case in &mut self.test_cases {
            case.result = None;
        }
        cx.notify();

        let save_task =
            self.save_source_editor(SaveOptions::default(), self.project.clone(), window, cx);
        cx.spawn(async move |this, cx| {
            let result = async {
                save_task.await?;
                cx.background_spawn(async move { command.run().await })
                    .await
            }
            .await;

            this.update(cx, |this, cx| {
                match result {
                    Ok(output) => {
                        this.command_status = if output.success {
                            CommandStatus::Succeeded
                        } else {
                            CommandStatus::Failed
                        };
                        for (index, case_result) in output.case_results {
                            if let Some(case) = this.test_cases.get_mut(index) {
                                case.result = Some(case_result);
                            }
                        }
                        if let Some(submit_ready) = output.submit_ready {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                submit_ready.source_code,
                            ));
                            cx.open_url(&submit_ready.submit_url);
                            log::info!(
                                "prepared Rpractice submit for {}",
                                submit_ready.source_path.display()
                            );
                            this.command_output = CommandOutputState {
                                items: output.items,
                            };
                            this.start_submission_watch(cx);
                        } else {
                            this.command_output = CommandOutputState {
                                items: output.items,
                            };
                        }
                    }
                    Err(error) => {
                        this.command_status = CommandStatus::Failed;
                        this.command_output = single_command_output_state(
                            "Command",
                            CommandOutputItemStatus::Failed,
                            format!("Failed before running command:\n{error:#}"),
                        );
                    }
                }
                this.focus_after_run();
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn upsert_submissions_item(
        &mut self,
        item: CommandOutputItem,
        select_item: bool,
        cx: &mut Context<Self>,
    ) {
        let index = match self
            .command_output
            .items
            .iter()
            .position(|existing| existing.label.as_ref() == "Submissions")
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

    fn fetch_submissions(&mut self, cx: &mut Context<Self>) {
        if self.submission_watch_started_at.is_some() {
            return;
        }

        if self.is_fetching_submissions
            && self.selected_problem.as_ref().is_some_and(|problem| {
                self.submission_fetch_problem_id.as_deref() == Some(problem.id.as_str())
            })
        {
            return;
        }

        let Some(problem) = self.selected_problem.clone() else {
            self.upsert_submissions_item(
                CommandOutputItem {
                    label: "Submissions".into(),
                    status: CommandOutputItemStatus::Failed,
                    detail: Some(CommandOutputDetail::new(
                        "Open a problem before fetching submissions.",
                    )),
                },
                true,
                cx,
            );
            return;
        };
        let Some(atcoder_user) = configured_atcoder_user(cx) else {
            self.upsert_submissions_item(
                CommandOutputItem {
                    label: "Submissions".into(),
                    status: CommandOutputItemStatus::Failed,
                    detail: Some(CommandOutputDetail::new(
                        "AtCoder user is not configured. Set it in Rduel first.",
                    )),
                },
                true,
                cx,
            );
            return;
        };

        let fetch_problem_id = problem.id.clone();
        self.is_fetching_submissions = true;
        self.submission_fetch_problem_id = Some(fetch_problem_id.clone());
        self.upsert_submissions_item(
            CommandOutputItem {
                label: "Submissions".into(),
                status: CommandOutputItemStatus::Pending,
                detail: Some(CommandOutputDetail::new("Fetching AtCoder submissions...")),
            },
            true,
            cx,
        );

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let atcoder_user = atcoder_user.clone();
                    let problem = problem.clone();
                    async move {
                        fetcher::fetch_problem_submissions(
                            &atcoder_user,
                            &problem,
                            SUBMISSION_FETCH_LIMIT,
                        )
                    }
                })
                .await;

            this.update(cx, |this, cx| {
                let is_current_fetch =
                    this.submission_fetch_problem_id.as_deref() == Some(fetch_problem_id.as_str());
                if !is_current_fetch {
                    return;
                }
                this.is_fetching_submissions = false;
                this.submission_fetch_problem_id = None;
                if this.submission_watch_started_at.is_some()
                    || !this
                        .selected_problem
                        .as_ref()
                        .is_some_and(|selected_problem| selected_problem.id == fetch_problem_id)
                {
                    cx.notify();
                    return;
                }
                let item = submissions_output_item(&atcoder_user, &problem, result);
                this.upsert_submissions_item(item, true, cx);
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn start_submission_watch(&mut self, cx: &mut Context<Self>) {
        let Some(problem) = self.selected_problem.clone() else {
            return;
        };
        let Some(atcoder_user) = configured_atcoder_user(cx) else {
            self.upsert_submissions_item(
                CommandOutputItem {
                    label: "Submissions".into(),
                    status: CommandOutputItemStatus::Failed,
                    detail: Some(CommandOutputDetail::new(
                        "AtCoder user is not configured. Set it in Rduel first.",
                    )),
                },
                true,
                cx,
            );
            return;
        };

        let from_second = unix_now();
        self.submission_fetch_problem_id = None;
        self.submission_watch_started_at = Some(from_second);
        self.submission_watch_problem_id = Some(problem.id.clone());
        self.is_fetching_submissions = true;
        self.upsert_submissions_item(
            CommandOutputItem {
                label: "Submissions".into(),
                status: CommandOutputItemStatus::Pending,
                detail: Some(CommandOutputDetail::new(
                    "Waiting for the next AtCoder submission...",
                )),
            },
            true,
            cx,
        );
        self.poll_submission_after_delay(atcoder_user, problem, from_second, cx);
    }

    fn poll_submission_after_delay(
        &self,
        atcoder_user: String,
        problem: Problem,
        from_second: i64,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(3)).await;
            let result = cx
                .background_spawn({
                    let atcoder_user = atcoder_user.clone();
                    let problem = problem.clone();
                    async move {
                        fetcher::fetch_problem_submissions(
                            &atcoder_user,
                            &problem,
                            SUBMISSION_FETCH_LIMIT,
                        )
                    }
                })
                .await;

            this.update(cx, |this, cx| {
                let is_current_watch = this.submission_watch_started_at == Some(from_second)
                    && this.submission_watch_problem_id.as_deref() == Some(problem.id.as_str());
                if !is_current_watch {
                    return;
                }
                if !this
                    .selected_problem
                    .as_ref()
                    .is_some_and(|selected_problem| selected_problem.id == problem.id)
                {
                    this.submission_watch_started_at = None;
                    this.submission_watch_problem_id = None;
                    this.is_fetching_submissions = false;
                    cx.notify();
                    return;
                }
                match result {
                    Ok(submissions) => {
                        if let Some(submission) =
                            first_submission_since(&submissions, from_second).cloned()
                        {
                            if !fetcher::is_final_atcoder_verdict(&submission.verdict) {
                                this.poll_submission_after_delay(
                                    atcoder_user,
                                    problem,
                                    from_second,
                                    cx,
                                );
                                return;
                            }
                            let has_ac = submission.verdict.trim() == "AC";
                            this.submission_watch_started_at = None;
                            this.submission_watch_problem_id = None;
                            this.is_fetching_submissions = false;
                            this.upsert_submissions_item(
                                detected_submission_output_item(&submission),
                                true,
                                cx,
                            );
                            if has_ac {
                                this.finish_practice_session(cx);
                            }
                        } else {
                            this.poll_submission_after_delay(
                                atcoder_user,
                                problem,
                                from_second,
                                cx,
                            );
                        }
                    }
                    Err(error) => {
                        this.submission_watch_started_at = None;
                        this.submission_watch_problem_id = None;
                        this.is_fetching_submissions = false;
                        this.upsert_submissions_item(
                            CommandOutputItem {
                                label: "Submissions".into(),
                                status: CommandOutputItemStatus::Failed,
                                detail: Some(CommandOutputDetail::new(format!(
                                    "Could not fetch submissions:\n{error:#}"
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

    fn refresh_submissions(
        &mut self,
        _: &RefreshSubmissions,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.fetch_submissions(cx);
    }

    fn run_samples(&mut self, _: &RunSamples, window: &mut Window, cx: &mut Context<Self>) {
        let Some(files) = self.selected_files.clone() else {
            self.command_status = CommandStatus::Failed;
            self.command_output = single_command_output_state(
                "Problem",
                CommandOutputItemStatus::Failed,
                "Open a problem before running samples.",
            );
            self.output_selection = OutputSelection::Step(0);
            cx.notify();
            return;
        };

        self.spawn_practice_command(
            "Test",
            PracticeCommand::Test {
                files,
                cases: self.test_case_texts(cx),
            },
            window,
            cx,
        );
    }

    fn submit_solution(&mut self, _: &SubmitSolution, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(files), Some(problem)) =
            (self.selected_files.clone(), self.selected_problem.clone())
        else {
            self.command_status = CommandStatus::Failed;
            self.command_output = single_command_output_state(
                "Problem",
                CommandOutputItemStatus::Failed,
                "Open a problem before submitting.",
            );
            self.output_selection = OutputSelection::Step(0);
            cx.notify();
            return;
        };

        self.spawn_practice_command(
            "Submit",
            PracticeCommand::Submit {
                files,
                cases: self.test_case_texts(cx),
                problem_url: problem.url,
            },
            window,
            cx,
        );
    }

    fn add_test_case(&mut self, _: &AddTestCase, window: &mut Window, cx: &mut Context<Self>) {
        self.test_cases.push(PracticeTestCase {
            input: Self::new_case_editor("", window, cx),
            expected: Self::new_case_editor("", window, cx),
            default: None,
            result: None,
        });
        self.output_selection = OutputSelection::Case(self.test_cases.len().saturating_sub(1));
        cx.notify();
    }

    fn toggle_problem_group(&mut self, group: &str, cx: &mut Context<Self>) {
        if !self.expanded_groups.remove(group) {
            self.expanded_groups.insert(group.to_string());
        }
        cx.notify();
    }

    fn toggle_problem_contest(&mut self, group: &str, segment: &str, cx: &mut Context<Self>) {
        let key = (group.to_string(), segment.to_string());
        if !self.expanded_contests.remove(&key) {
            self.expanded_contests.insert(key);
        }
        cx.notify();
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        cx.notify();
    }

    pub fn toggle_sidebar_from_status_bar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        cx.notify();
    }

    pub fn sidebar_open(&self) -> bool {
        self.sidebar_open
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
                    "rpractice-run-samples" => this.run_samples(&RunSamples, window, cx),
                    "rpractice-submit" => this.submit_solution(&SubmitSolution, window, cx),
                    "rpractice-submissions" => {
                        this.refresh_submissions(&RefreshSubmissions, window, cx)
                    }
                    _ => {}
                }
            }))
    }

    fn render_problem_row(
        &self,
        row_index: usize,
        problem_index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(problem) = self.problems.get(problem_index) else {
            return Empty.into_any_element();
        };
        let is_selected = self
            .selected_problem
            .as_ref()
            .is_some_and(|selected| selected.id == problem.id);
        let title = if problem.title.trim().is_empty() {
            problem.id.clone()
        } else {
            problem.title.clone()
        };

        self.render_problem_tree_row(
            ("rpractice-problem-row", row_index),
            2,
            is_selected,
            None,
            None,
            title,
            false,
            cx,
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            this.open_problem_at_index(problem_index, window, cx);
        }))
        .into_any_element()
    }

    fn render_problem_tree_row(
        &self,
        id: impl Into<ElementId>,
        depth: usize,
        is_selected: bool,
        toggle_icon: Option<IconName>,
        item_icon: Option<IconName>,
        text: impl Into<SharedString>,
        muted: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let id = id.into();
        let text = text.into();
        div()
            .id(ElementId::NamedChild(
                Arc::new(id.clone()),
                "container".into(),
            ))
            .bg(cx.theme().colors().panel_background)
            .child(
                ListItem::new(id)
                    .height(px(24.))
                    .indent_level(depth)
                    .indent_step_size(px(16.))
                    .spacing(ListItemSpacing::ExtraDense)
                    .selectable(true)
                    .toggle_state(is_selected)
                    .when_some(toggle_icon, |this, icon| {
                        this.child(Icon::new(icon).size(IconSize::Small))
                    })
                    .when_some(item_icon, |this, icon| {
                        this.child(Icon::new(icon).size(IconSize::Small))
                    })
                    .child(
                        Label::new(text)
                            .size(LabelSize::Default)
                            .when(muted, |this| this.color(Color::Muted))
                            .flex_1()
                            .truncate(),
                    ),
            )
    }

    fn problem_list_entries(&self, expand_all: bool) -> Vec<ProblemListEntry> {
        let mut entries = Vec::new();
        let mut current_group = None::<&str>;
        let mut current_segment = None::<(&str, &str)>;

        for problem_index in &self.filtered_problem_indices {
            let Some(problem) = self.problems.get(*problem_index) else {
                continue;
            };
            let group = problem.contest_group.as_str();
            let segment = problem.contest_segment.as_str();
            if current_group != Some(group) {
                entries.push(ProblemListEntry::Group {
                    group: group.to_string(),
                    is_expanded: expand_all || self.expanded_groups.contains(group),
                });
                current_group = Some(group);
                current_segment = None;
            }
            if !expand_all && !self.expanded_groups.contains(group) {
                continue;
            }
            if current_segment != Some((group, segment)) {
                entries.push(ProblemListEntry::Contest {
                    group: group.to_string(),
                    segment: segment.to_string(),
                    is_expanded: expand_all
                        || self
                            .expanded_contests
                            .contains(&(group.to_string(), segment.to_string())),
                });
                current_segment = Some((group, segment));
            }
            if !expand_all
                && !self
                    .expanded_contests
                    .contains(&(group.to_string(), segment.to_string()))
            {
                continue;
            }
            entries.push(ProblemListEntry::Problem {
                problem_index: *problem_index,
            });
        }

        entries
    }

    fn render_problem_group_row(
        &self,
        row_index: usize,
        group: String,
        is_expanded: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.render_problem_tree_row(
            ("rpractice-problem-group-row", row_index),
            0,
            false,
            Some(if is_expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            }),
            Some(if is_expanded {
                IconName::FolderOpen
            } else {
                IconName::Folder
            }),
            group.clone(),
            false,
            cx,
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            this.toggle_problem_group(&group, cx);
        }))
        .into_any_element()
    }

    fn render_problem_contest_row(
        &self,
        row_index: usize,
        group: String,
        segment: String,
        is_expanded: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.render_problem_tree_row(
            ("rpractice-problem-contest-row", row_index),
            1,
            false,
            Some(if is_expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            }),
            Some(if is_expanded {
                IconName::FolderOpen
            } else {
                IconName::Folder
            }),
            segment.clone(),
            true,
            cx,
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            this.toggle_problem_contest(&group, &segment, cx);
        }))
        .into_any_element()
    }

    fn render_problem_list_entry(
        &self,
        row_index: usize,
        entry: ProblemListEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match entry {
            ProblemListEntry::Group { group, is_expanded } => {
                self.render_problem_group_row(row_index, group, is_expanded, cx)
            }
            ProblemListEntry::Contest {
                group,
                segment,
                is_expanded,
            } => self.render_problem_contest_row(row_index, group, segment, is_expanded, cx),
            ProblemListEntry::Problem { problem_index } => {
                self.render_problem_row(row_index, problem_index, cx)
            }
        }
    }

    fn problem_list_entry_depths(
        &self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) -> SmallVec<[usize; 64]> {
        let expand_all = !self.query_editor.read(cx).text(cx).trim().is_empty();
        let entries = self.problem_list_entries(expand_all);
        range
            .filter_map(|row_index| entries.get(row_index).map(ProblemListEntry::depth))
            .collect()
    }

    fn problem_list_sticky_candidates(
        &self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) -> SmallVec<[ProblemListStickyCandidate; 8]> {
        let expand_all = !self.query_editor.read(cx).text(cx).trim().is_empty();
        let entries = self.problem_list_entries(expand_all);
        range
            .filter_map(|row_index| {
                entries
                    .get(row_index)
                    .map(|entry| ProblemListStickyCandidate {
                        index: row_index,
                        depth: entry.depth(),
                    })
            })
            .collect()
    }

    fn problem_list_sticky_ancestors(
        &self,
        child: ProblemListStickyCandidate,
        cx: &mut Context<Self>,
    ) -> SmallVec<[AnyElement; 8]> {
        let expand_all = !self.query_editor.read(cx).text(cx).trim().is_empty();
        let entries = self.problem_list_entries(expand_all);
        let Some(child_entry) = entries.get(child.index) else {
            return SmallVec::new();
        };

        let mut ancestors = Vec::new();
        match child_entry {
            ProblemListEntry::Group { .. } => {}
            ProblemListEntry::Contest { group, .. } => {
                if let Some(group_entry) = entries[..child.index].iter().rev().find_map(|entry| {
                    if let ProblemListEntry::Group {
                        group: candidate_group,
                        is_expanded,
                    } = entry
                        && candidate_group == group
                    {
                        Some(ProblemListEntry::Group {
                            group: candidate_group.clone(),
                            is_expanded: *is_expanded,
                        })
                    } else {
                        None
                    }
                }) {
                    ancestors.push(group_entry);
                }
            }
            ProblemListEntry::Problem { problem_index } => {
                let Some(problem) = self.problems.get(*problem_index) else {
                    return SmallVec::new();
                };
                let group = problem.contest_group.as_str();
                let segment = problem.contest_segment.as_str();

                if let Some(group_entry) = entries[..child.index].iter().rev().find_map(|entry| {
                    if let ProblemListEntry::Group {
                        group: candidate_group,
                        is_expanded,
                    } = entry
                        && candidate_group == group
                    {
                        Some(ProblemListEntry::Group {
                            group: candidate_group.clone(),
                            is_expanded: *is_expanded,
                        })
                    } else {
                        None
                    }
                }) {
                    ancestors.push(group_entry);
                }
                if let Some(contest_entry) = entries[..child.index].iter().rev().find_map(|entry| {
                    if let ProblemListEntry::Contest {
                        group: candidate_group,
                        segment: candidate_segment,
                        is_expanded,
                    } = entry
                        && candidate_group == group
                        && candidate_segment == segment
                    {
                        Some(ProblemListEntry::Contest {
                            group: candidate_group.clone(),
                            segment: candidate_segment.clone(),
                            is_expanded: *is_expanded,
                        })
                    } else {
                        None
                    }
                }) {
                    ancestors.push(contest_entry);
                }
            }
        }

        let last_index = ancestors.len().saturating_sub(1);
        ancestors
            .into_iter()
            .enumerate()
            .map(|(index, entry)| {
                let mut row = self.render_problem_list_sticky_entry(child.index, index, entry, cx);
                if index == last_index {
                    let shadow_color_top = gpui::hsla(0.0, 0.0, 0.0, 0.10);
                    let shadow_color_bottom = gpui::hsla(0.0, 0.0, 0.0, 0.0);
                    row = div()
                        .relative()
                        .child(row)
                        .child(
                            div()
                                .absolute()
                                .left_0()
                                .bottom_neg_1p5()
                                .h_1p5()
                                .w_full()
                                .bg(gpui::linear_gradient(
                                    0.,
                                    gpui::linear_color_stop(shadow_color_top, 1.),
                                    gpui::linear_color_stop(shadow_color_bottom, 0.),
                                )),
                        )
                        .into_any_element();
                }
                row
            })
            .collect()
    }

    fn render_problem_list_sticky_entry(
        &self,
        child_index: usize,
        ancestor_index: usize,
        entry: ProblemListEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match entry {
            ProblemListEntry::Group { group, is_expanded } => self
                .render_problem_tree_row(
                    (
                        "rpractice-sticky-problem-group-row",
                        child_index + ancestor_index,
                    ),
                    0,
                    false,
                    Some(if is_expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    }),
                    Some(if is_expanded {
                        IconName::FolderOpen
                    } else {
                        IconName::Folder
                    }),
                    group.clone(),
                    false,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_problem_group(&group, cx);
                }))
                .into_any_element(),
            ProblemListEntry::Contest {
                group,
                segment,
                is_expanded,
            } => self
                .render_problem_tree_row(
                    (
                        "rpractice-sticky-problem-contest-row",
                        child_index + ancestor_index,
                    ),
                    1,
                    false,
                    Some(if is_expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    }),
                    Some(if is_expanded {
                        IconName::FolderOpen
                    } else {
                        IconName::Folder
                    }),
                    segment.clone(),
                    true,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_problem_contest(&group, &segment, cx);
                }))
                .into_any_element(),
            ProblemListEntry::Problem { problem_index } => {
                self.render_problem_row(child_index + ancestor_index, problem_index, cx)
            }
        }
    }

    fn render_problem_list_indent_guides(
        params: ui::RenderIndentGuideParams,
    ) -> SmallVec<[RenderedIndentGuide; 12]> {
        const LEFT_OFFSET: gpui::Pixels = px(14.);
        const PADDING_Y: gpui::Pixels = px(4.);

        params
            .indent_guides
            .into_iter()
            .map(|layout| {
                let offset = if layout.continues_offscreen {
                    px(0.)
                } else {
                    PADDING_Y
                };
                RenderedIndentGuide {
                    bounds: Bounds::new(
                        point(
                            layout.offset.x * params.indent_size + LEFT_OFFSET,
                            layout.offset.y * params.item_height + offset,
                        ),
                        size(px(1.), layout.length * params.item_height - offset * 2.),
                    ),
                    layout,
                    is_active: false,
                    hitbox: None,
                }
            })
            .collect()
    }

    fn render_problem_list_items(
        &mut self,
        range: std::ops::Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let expand_all = !self.query_editor.read(cx).text(cx).trim().is_empty();
        let entries = self.problem_list_entries(expand_all);
        range
            .filter_map(|row_index| {
                let entry = entries.get(row_index)?.clone();
                Some(self.render_problem_list_entry(row_index, entry, cx))
            })
            .collect()
    }

    fn render_problem_list_refresh_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let disabled = self.is_loading_list;
        h_flex()
            .id("rpractice-refresh-problem-list")
            .h(px(24.))
            .w(px(24.))
            .items_center()
            .justify_center()
            .rounded_sm()
            .bg(cx.theme().colors().ghost_element_background)
            .when(!disabled, |this| {
                this.hover(|this| this.bg(cx.theme().colors().ghost_element_hover))
                    .cursor_pointer()
            })
            .when(disabled, |this| this.opacity(0.5))
            .child(Icon::new(IconName::RotateCcw).size(IconSize::XSmall))
            .on_click(cx.listener(move |this, _, _, cx| {
                if !disabled {
                    this.fetch_problem_list(cx);
                }
            }))
    }

    fn render_sidebar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let expand_all = !self.query_editor.read(cx).text(cx).trim().is_empty();
        let count = self.problem_list_entries(expand_all).len();

        v_flex()
            .w(px(self.sidebar_width))
            .h_full()
            .flex_none()
            .border_r_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().panel_background)
            .child(
                v_flex()
                    .gap_2()
                    .p_3()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .child(Label::new("Rpractice").size(LabelSize::Default))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        Label::new(format!("{} problems", self.problems.len()))
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .child(self.render_problem_list_refresh_button(cx)),
                            ),
                    )
                    .child(
                        h_flex()
                            .h(px(30.))
                            .items_center()
                            .overflow_hidden()
                            .rounded_sm()
                            .border_1()
                            .border_color(cx.theme().colors().border)
                            .px_2()
                            .child(div().flex_1().min_w_0().child(self.query_editor.clone())),
                    ),
            )
            .child(v_flex().flex_1().min_h_0().px_2().py_1().map(|this| {
                if count == 0 {
                    this.child(div().p_2().child(Label::new(if self.is_loading_list {
                        "Loading problems..."
                    } else {
                        "No problems found."
                    })))
                    .into_any_element()
                } else {
                    this.child(
                        uniform_list(
                            "rpractice-problem-list",
                            count,
                            cx.processor(Self::render_problem_list_items),
                        )
                        .with_decoration(
                            ui::indent_guides(px(16.), IndentGuideColors::panel(cx))
                                .with_compute_indents_fn(cx.entity(), |this, range, _window, cx| {
                                    this.problem_list_entry_depths(range, cx)
                                })
                                .with_render_fn(cx.entity(), |_, params, _, _| {
                                    Self::render_problem_list_indent_guides(params)
                                }),
                        )
                        .with_decoration(
                            ui::sticky_items(
                                cx.entity(),
                                |this, range, _window, cx| {
                                    this.problem_list_sticky_candidates(range, cx)
                                },
                                |this, child, _window, cx| {
                                    this.problem_list_sticky_ancestors(child, cx)
                                },
                            )
                            .with_decoration(
                                ui::indent_guides(px(16.), IndentGuideColors::panel(cx))
                                    .with_render_fn(cx.entity(), |_, params, _, _| {
                                        Self::render_problem_list_indent_guides(params)
                                    }),
                            ),
                        )
                        .track_scroll(&self.problem_list_scroll)
                        .h_full(),
                    )
                    .vertical_scrollbar_for(&self.problem_list_scroll, window, cx)
                    .into_any_element()
                }
            }))
            .when(!self.status.is_empty(), |this| {
                this.child(
                    div()
                        .p_2()
                        .border_t_1()
                        .border_color(cx.theme().colors().border)
                        .child(
                            Label::new(self.status.clone())
                                .size(LabelSize::Default)
                                .color(Color::Muted),
                        ),
                )
            })
    }

    fn render_problem_panel(
        &self,
        markdown_style: MarkdownStyle,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title = self
            .selected_problem
            .as_ref()
            .map(|problem| format_problem_heading(&problem.id, &problem.title))
            .unwrap_or_else(|| "Select a problem".to_string());

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
                    .child(Label::new(title).size(LabelSize::Default).truncate()),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("rpractice-problem")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_y_scroll()
                            .p_4()
                            .track_scroll(&self.problem_scroll_handle)
                            .map(|this| {
                                if let Some(problem_markdown) = self.problem_markdown.as_ref() {
                                    this.child(
                                        MarkdownElement::new(
                                            problem_markdown.clone(),
                                            markdown_style,
                                        )
                                        .code_block_renderer(CodeBlockRenderer::Default {
                                            copy_button_visibility: CopyButtonVisibility::Hidden,
                                            wrap_button_visibility: WrapButtonVisibility::Hidden,
                                            border: false,
                                        })
                                        .image_resolver(|dest_url| {
                                            (dest_url.starts_with("http://")
                                                || dest_url.starts_with("https://"))
                                            .then(|| {
                                                ImageSource::Resource(Resource::Uri(
                                                    SharedUri::from(dest_url.to_string()),
                                                ))
                                            })
                                        })
                                        .scroll_handle(self.problem_scroll_handle.clone()),
                                    )
                                    .into_any_element()
                                } else {
                                    this.child(Label::new("Choose a problem from the list."))
                                        .into_any_element()
                                }
                            }),
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

    fn resize_sidebar_width(
        &mut self,
        event: &DragMoveEvent<DraggedSidebarDivider>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let bounds_left = event.bounds.left().as_f32();
        let pointer_x = event.event.position.x.as_f32();
        let width = (pointer_x - bounds_left).clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);

        if (self.sidebar_width - width).abs() > f32::EPSILON {
            self.sidebar_width = width;
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn reset_sidebar_width(&mut self, cx: &mut Context<Self>) {
        self.sidebar_width = DEFAULT_SIDEBAR_WIDTH;
        cx.notify();
    }

    fn render_sidebar_divider(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("rpractice-sidebar-divider")
            .relative()
            .w(px(9.))
            .h_full()
            .flex_none()
            .cursor_col_resize()
            .on_drag(DraggedSidebarDivider, |divider, _, _, cx| {
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
                        this.reset_sidebar_width(cx);
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

    fn resize_problem_width(
        &mut self,
        event: &DragMoveEvent<DraggedProblemDivider>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let bounds_width = event.bounds.size.width.as_f32();
        if bounds_width <= 0.0 {
            return;
        }

        let pointer_x = event.event.position.x.as_f32();
        let bounds_left = event.bounds.left().as_f32();
        let fraction = ((pointer_x - bounds_left) / bounds_width)
            .clamp(MIN_PROBLEM_WIDTH_FRACTION, MAX_PROBLEM_WIDTH_FRACTION);

        if (self.problem_width_fraction - fraction).abs() > f32::EPSILON {
            self.problem_width_fraction = fraction;
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn reset_problem_width(&mut self, cx: &mut Context<Self>) {
        self.problem_width_fraction = DEFAULT_PROBLEM_WIDTH_FRACTION;
        cx.notify();
    }

    fn render_split_divider(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("rpractice-split-divider")
            .relative()
            .w(px(9.))
            .h_full()
            .flex_none()
            .cursor_col_resize()
            .on_drag(DraggedProblemDivider, |divider, _, _, cx| {
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

    fn resize_output_height(
        &mut self,
        event: &DragMoveEvent<DraggedOutputDivider>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let bounds_bottom = event.bounds.bottom().as_f32();
        let bounds_top = event.bounds.top().as_f32();
        let pointer_y = event.event.position.y.as_f32();
        let available = (bounds_bottom - bounds_top).max(0.0);
        let max_height = (available - CODE_AREA_TOP_RESERVE).max(MIN_OUTPUT_HEIGHT);
        let output_height = (bounds_bottom - pointer_y).clamp(MIN_OUTPUT_HEIGHT, max_height);

        if (self.output_height - output_height).abs() > f32::EPSILON {
            self.output_height = output_height;
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn reset_output_height(&mut self, cx: &mut Context<Self>) {
        self.output_height = DEFAULT_OUTPUT_HEIGHT;
        cx.notify();
    }

    fn render_output_divider(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("rpractice-output-divider")
            .relative()
            .h(px(9.))
            .w_full()
            .flex_none()
            .cursor_row_resize()
            .on_drag(DraggedOutputDivider, |divider, _, _, cx| {
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
                        this.reset_output_height(cx);
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

    fn render_step_chip(
        &self,
        index: usize,
        item: &CommandOutputItem,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = self.output_selection == OutputSelection::Step(index);
        h_flex()
            .id(("rpractice-step-chip", index))
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
            .id(("rpractice-case-chip", index))
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
            .id("rpractice-add-case")
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
            .on_click(
                cx.listener(|this, _, window, cx| this.add_test_case(&AddTestCase, window, cx)),
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

        let mut rows = Vec::new();
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
        case.input.update(cx, |editor, cx| {
            editor.set_text(default.input.as_str(), window, cx)
        });
        case.expected.update(cx, |editor, cx| {
            editor.set_text(default.output.as_str(), window, cx)
        });
        cx.notify();
    }

    fn render_case_detail(
        &self,
        index: usize,
        case: &PracticeTestCase,
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
                                            Button::new(
                                                ("rpractice-case-restore", index),
                                                "Restore",
                                            )
                                            .disabled(!can_restore)
                                            .on_click(
                                                cx.listener(move |this, _, window, cx| {
                                                    this.restore_test_case(index, window, cx)
                                                }),
                                            ),
                                        )
                                    })
                                    .child(
                                        Button::new(("rpractice-case-delete", index), "Delete")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.delete_test_case(index, cx)
                                            })),
                                    ),
                            ),
                    )
                    .child(self.render_case_editor(case.input.clone(), cx)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Label::new("Expected")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(self.render_case_editor(case.expected.clone(), cx)),
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

    fn render_case_editor(
        &self,
        editor: Entity<Editor>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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

    fn search_target_is_editor(
        &self,
        editor: &Entity<Editor>,
        search_bar: &Entity<BufferSearchBar>,
        cx: &App,
    ) -> bool {
        self.search_target_editor.as_ref() == Some(editor) && !search_bar.read(cx).is_dismissed()
    }

    fn render_search_bar_for_editor(
        &self,
        editor: Entity<Editor>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(search_bar) = self.active_pane_search_bar(cx) else {
            return Empty.into_any_element();
        };
        if self.search_target_is_editor(&editor, &search_bar, cx) {
            search_bar.into_any_element()
        } else {
            Empty.into_any_element()
        }
    }

    fn focused_search_editor(&self, window: &Window, cx: &App) -> Option<Entity<Editor>> {
        if let Some(source_editor) = self.source_editor.as_ref()
            && source_editor
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
        {
            return Some(source_editor.clone());
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

        self.search_target_editor
            .clone()
            .or_else(|| self.source_editor.clone())
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

    fn focused_code_editor(&self, window: &Window, cx: &App) -> Option<Entity<Editor>> {
        let source_editor = self.source_editor.as_ref()?;
        if source_editor
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx)
        {
            return Some(source_editor.clone());
        }
        Some(source_editor.clone())
    }

    fn code_editor_with_pending_rename(&self, cx: &App) -> Option<Entity<Editor>> {
        let source_editor = self.source_editor.as_ref()?;
        if source_editor.read(cx).pending_rename().is_some() {
            Some(source_editor.clone())
        } else {
            None
        }
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

    fn render_editor_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .h_full()
            .overflow_hidden()
            .child(
                h_flex()
                    .h(px(40.))
                    .items_center()
                    .px_3()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(self.render_source_tab(cx)),
            )
            .child(div().flex_1().min_h_0().overflow_hidden().map(|this| {
                if let Some(source_editor) = self.source_editor.as_ref() {
                    this.child(self.render_search_bar_for_editor(source_editor.clone(), cx))
                        .child(source_editor.clone())
                        .into_any_element()
                } else {
                    this.child(Label::new("Open a problem to create a source file."))
                        .into_any_element()
                }
            }))
            .child(self.render_output_divider(cx))
            .child(self.render_command_output(cx))
    }

    fn render_source_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let label = self
            .selected_files
            .as_ref()
            .map(|files| files.source_file_name.clone())
            .unwrap_or_else(|| "source.rs".to_string());
        let is_dirty = self
            .source_buffer
            .as_ref()
            .is_some_and(|buffer| buffer.read(cx).is_dirty());
        let label = if is_dirty {
            format!("{label} *").into()
        } else {
            SharedString::from(label)
        };

        h_flex()
            .id("rpractice-source-tab")
            .h(px(28.))
            .items_center()
            .px_2p5()
            .rounded_sm()
            .bg(cx.theme().colors().element_background)
            .hover(|this| this.bg(cx.theme().colors().ghost_element_hover))
            .cursor_pointer()
            .child(Label::new(label).size(LabelSize::Default))
            .on_click(cx.listener(|this, _, window, cx| {
                if let Some(source_editor) = this.source_editor.as_ref() {
                    source_editor.read(cx).focus_handle(cx).focus(window, cx);
                }
            }))
    }

    fn render_command_output(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_command_running = self.command_status.is_running();
        let is_submissions_running = self.is_fetching_submissions;
        let mut chips = Vec::new();
        for (index, item) in self.command_output.items.iter().enumerate() {
            chips.push(self.render_step_chip(index, item, cx).into_any_element());
        }
        for index in 0..self.test_cases.len() {
            chips.push(self.render_case_chip(index, cx).into_any_element());
        }
        chips.push(self.render_add_case_chip(cx).into_any_element());

        v_flex()
            .h(px(self.output_height))
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
                            .id("rpractice-chip-row")
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
                                "rpractice-run-samples",
                                IconName::PlayFilled,
                                "Run",
                                is_command_running,
                                cx,
                            ))
                            .child(self.render_action_button(
                                "rpractice-submit",
                                IconName::Send,
                                "Submit",
                                is_command_running,
                                cx,
                            ))
                            .child(self.render_action_button(
                                "rpractice-submissions",
                                IconName::RotateCw,
                                "Submissions",
                                is_command_running || is_submissions_running,
                                cx,
                            )),
                    ),
            )
            .child(
                v_flex().flex_1().min_h_0().overflow_hidden().p_3().child(
                    div()
                        .id("rpractice-command-output-detail")
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
}

async fn open_problem_source_buffer(
    workspace: WeakEntity<Workspace>,
    files: &RpracticeProblemFiles,
    cx: &mut gpui::AsyncWindowContext,
) -> Option<Entity<Buffer>> {
    let relative_path = RelPath::unix(&files.source_file_name)
        .map(Arc::from)
        .log_err()?;
    let worktree = workspace
        .update(cx, |workspace, cx| {
            workspace.project().update(cx, |project, cx| {
                project.find_or_create_worktree(&files.root_path, false, cx)
            })
        })
        .log_err()?
        .await
        .log_err()?;
    let (worktree, _) = worktree;
    let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());

    workspace
        .update(cx, |workspace, cx| {
            workspace.project().update(cx, |project, cx| {
                project.open_buffer((worktree_id, relative_path), cx)
            })
        })
        .log_err()?
        .await
        .log_err()
}

fn prepare_problem_files(problem: &Problem) -> anyhow::Result<RpracticeProblemFiles> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("could not determine current user home directory"))?;
    let group = sanitize_path_component(&problem.contest_group);
    let segment = sanitize_path_component(&problem.contest_segment);
    let task_slug = sanitize_task_slug(&problem.task_index);
    let root_path = home.join(".rpractice").join(&group).join(&segment);
    let source_file_name = format!("{task_slug}.rs");
    let source_path = root_path.join(&source_file_name);
    let cargo_toml_path = root_path.join("Cargo.toml");
    let cargo_config_path = root_path.join(".cargo").join("config.toml");
    let target_path = home.join(".cache").join("rpractice").join("target");

    std::fs::create_dir_all(&root_path)
        .with_context(|| format!("creating Rpractice directory {}", root_path.display()))?;
    if let Some(parent) = cargo_config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating Cargo config directory {}", parent.display()))?;
    }
    if let Some(parent) = target_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating Rpractice target directory {}", parent.display()))?;
    }
    if !source_path.exists() {
        std::fs::write(&source_path, "fn main() {\n}\n")
            .with_context(|| format!("creating source file {}", source_path.display()))?;
    }

    let bin_name = task_slug;
    ensure_cargo_toml_bin(
        &cargo_toml_path,
        &group,
        &segment,
        &bin_name,
        &source_file_name,
    )?;
    std::fs::write(&cargo_config_path, cargo_config_toml(&target_path))
        .with_context(|| format!("writing Cargo config {}", cargo_config_path.display()))?;

    Ok(RpracticeProblemFiles {
        root_path,
        source_path,
        source_file_name,
        cargo_toml_path,
        cargo_config_path,
        target_path,
        bin_name,
    })
}

fn ensure_cargo_toml_bin(
    cargo_toml_path: &Path,
    group: &str,
    segment: &str,
    bin_name: &str,
    source_file_name: &str,
) -> anyhow::Result<()> {
    if !cargo_toml_path.exists() {
        let package_name = sanitize_package_name(&format!("rpractice-{group}-{segment}"));
        let cargo_toml = format!(
            r#"[package]
name = "{package_name}"
version = "0.1.0"
edition = "2024"

{STARTER_CARGO_TOML_DEPENDENCIES}

[[bin]]
name = "{bin_name}"
path = "{source_file_name}"
"#
        );
        std::fs::write(cargo_toml_path, cargo_toml)
            .with_context(|| format!("writing {}", cargo_toml_path.display()))?;
        return Ok(());
    }

    let mut cargo_toml = std::fs::read_to_string(cargo_toml_path)
        .with_context(|| format!("reading {}", cargo_toml_path.display()))?;
    if cargo_toml_has_bin(&cargo_toml, bin_name) {
        return Ok(());
    }
    if !cargo_toml.ends_with('\n') {
        cargo_toml.push('\n');
    }
    cargo_toml.push_str(&format!(
        r#"
[[bin]]
name = "{bin_name}"
path = "{source_file_name}"
"#
    ));
    std::fs::write(cargo_toml_path, cargo_toml)
        .with_context(|| format!("updating {}", cargo_toml_path.display()))?;
    Ok(())
}

fn cargo_toml_has_bin(cargo_toml: &str, bin_name: &str) -> bool {
    let Ok(value) = cargo_toml.parse::<toml::Value>() else {
        return cargo_toml.contains(&format!("name = \"{bin_name}\""));
    };
    value
        .get("bin")
        .and_then(toml::Value::as_array)
        .is_some_and(|bins| {
            bins.iter().any(|bin| {
                bin.get("name")
                    .and_then(toml::Value::as_str)
                    .is_some_and(|name| name == bin_name)
            })
        })
}

fn cargo_config_toml(target_path: &Path) -> String {
    let target_path = target_path.to_string_lossy().replace('\\', "\\\\");
    format!(
        r#"[build]
target-dir = "{target_path}"
"#
    )
}

fn sanitize_task_slug(task_index: &str) -> String {
    let sanitized = task_index
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "main".to_string()
    } else {
        sanitized
    }
}

fn sanitize_path_component(value: &str) -> String {
    let sanitized = value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "contest".to_string()
    } else {
        sanitized
    }
}

fn sanitize_package_name(value: &str) -> String {
    let mut output = String::new();
    let mut last_was_dash = false;
    for ch in value.trim().to_ascii_lowercase().chars() {
        let next = if ch.is_ascii_alphanumeric() { ch } else { '-' };
        if next == '-' {
            if !last_was_dash && !output.is_empty() {
                output.push(next);
            }
            last_was_dash = true;
        } else {
            output.push(next);
            last_was_dash = false;
        }
    }
    output.trim_matches('-').to_string()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

fn problem_needs_list_refresh(problem: &Problem) -> bool {
    problem.contest_group.trim().is_empty()
        || problem.contest_segment.trim().is_empty()
        || problem.contest_title.trim().is_empty()
}

impl Focusable for RpracticeView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for RpracticeView {}

impl Item for RpracticeView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.selected_problem
            .as_ref()
            .map(|problem| format!("Rpractice {}", problem.id).into())
            .unwrap_or_else(|| "Rpractice".into())
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.source_buffer
            .as_ref()
            .is_some_and(|buffer| buffer.read(cx).is_dirty())
    }

    fn can_save(&self, _cx: &App) -> bool {
        self.source_editor.is_some()
    }

    fn can_autosave(&self, cx: &App) -> bool {
        self.is_dirty(cx)
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.source_buffer
            .as_ref()
            .and_then(|buffer| buffer.read(cx).project_path(cx))
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
        if let Some(source_editor) = self.source_editor.as_ref() {
            source_editor.update(cx, |editor, cx| {
                editor.added_to_workspace(workspace, window, cx);
            });
        }
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
                .or_else(|| self.source_editor.clone())
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
        if let Some(source_buffer) = self.source_buffer.as_ref() {
            f(source_buffer.entity_id(), source_buffer.read(cx));
        }
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Task<anyhow::Result<()>> {
        self.save_source_editor(options, project, window, cx)
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Task<anyhow::Result<()>> {
        let Some(source_editor) = self.source_editor.clone() else {
            return gpui::Task::ready(Ok(()));
        };
        cx.spawn_in(window, async move |_, cx| {
            let reload = source_editor
                .update_in(cx, |editor, window, cx| editor.reload(project, window, cx))?;
            reload.await
        })
    }

    fn on_removed(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            this.update(cx, |this, cx| {
                this.finish_practice_session(cx);
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for RpracticeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let markdown_style = self.problem_markdown_style(window, cx);
        self.sync_search_target(window, cx);

        h_flex()
            .key_context("Rpractice")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(Self::run_samples))
            .on_action(cx.listener(Self::submit_solution))
            .on_action(cx.listener(Self::refresh_submissions))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::rename_symbol))
            .on_action(cx.listener(Self::confirm_rename))
            .overflow_hidden()
            .on_drag_move::<DraggedSidebarDivider>(cx.listener(Self::resize_sidebar_width))
            .on_drag_move::<DraggedProblemDivider>(cx.listener(Self::resize_problem_width))
            .on_drag_move::<DraggedOutputDivider>(cx.listener(Self::resize_output_height))
            .when(self.sidebar_open, |this| {
                this.child(self.render_sidebar(window, cx))
                    .child(self.render_sidebar_divider(cx))
            })
            .child(self.render_problem_panel(markdown_style, cx))
            .child(self.render_split_divider(cx))
            .child(self.render_editor_panel(cx))
    }
}
