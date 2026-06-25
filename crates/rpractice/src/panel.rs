// Panel module - Problem list sidebar

use anyhow::Result;
use gpui::{prelude::*, actions, AsyncWindowContext, Entity, EventEmitter, FocusHandle, Focusable, Render, WeakEntity};
use ui::prelude::*;
use workspace::{Workspace, dock::{Panel, PanelEvent, DockPosition}};

use crate::fetcher::Problem;

actions!(rpractice, [ToggleFocus]);

const RPRACTICE_PANEL_KEY: &str = "RpracticePanel";

pub struct RpracticePanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    problems: Vec<Problem>,
    filter_difficulty: Option<String>,
}

impl RpracticePanel {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        // TODO: Load problems from AtCoder or cache
        let problems = Self::get_sample_problems();

        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            problems,
            filter_difficulty: None,
        }
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, _window, cx| {
            let workspace_handle = cx.entity().downgrade();
            cx.new(|cx| RpracticePanel::new(workspace_handle, cx))
        })
    }

    fn get_sample_problems() -> Vec<Problem> {
        // TODO: Replace with real data from AtCoder
        vec![
            Problem {
                id: "abc001_a".to_string(),
                contest_id: "abc001".to_string(),
                task_index: "A".to_string(),
                title: "積雪深差".to_string(),
                url: "https://atcoder.jp/contests/abc001/tasks/abc001_a".to_string(),
            },
            Problem {
                id: "abc001_b".to_string(),
                contest_id: "abc001".to_string(),
                task_index: "B".to_string(),
                title: "視程の通報".to_string(),
                url: "https://atcoder.jp/contests/abc001/tasks/abc001_b".to_string(),
            },
            Problem {
                id: "abc002_a".to_string(),
                contest_id: "abc002".to_string(),
                task_index: "A".to_string(),
                title: "正直者".to_string(),
                url: "https://atcoder.jp/contests/abc002/tasks/abc002_a".to_string(),
            },
        ]
    }

    fn render_problem_item(&self, problem: &Problem) -> impl IntoElement {
        h_flex()
            .w_full()
            .p_2()
            .gap_2()
            .hover(|style| style.bg(gpui::rgb(0x2a2a2a)))
            .cursor_pointer()
            .child(
                div()
                    .w_8()
                    .flex_shrink_0()
                    .child(Label::new(problem.task_index.clone()).color(Color::Accent))
            )
            .child(
                v_flex()
                    .flex_1()
                    .child(Label::new(problem.title.clone()))
                    .child(
                        Label::new(problem.contest_id.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                    )
            )
    }
}

impl Focusable for RpracticePanel {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for RpracticePanel {}

impl Panel for RpracticePanel {
    fn persistent_name() -> &'static str {
        "Rpractice Panel"
    }

    fn panel_key() -> &'static str {
        RPRACTICE_PANEL_KEY
    }

    fn position(&self, _window: &gpui::Window, _cx: &gpui::App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, _position: DockPosition, _window: &mut gpui::Window, _cx: &mut Context<Self>) {
        // TODO: Save position to settings
    }

    fn default_size(&self, _window: &gpui::Window, _cx: &gpui::App) -> gpui::Pixels {
        px(360.)
    }

    fn icon(&self, _window: &gpui::Window, _cx: &gpui::App) -> Option<ui::IconName> {
        Some(ui::IconName::Code)
    }

    fn icon_tooltip(&self, _window: &gpui::Window, _cx: &gpui::App) -> Option<&'static str> {
        Some("Rpractice")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        2 // Same as ProjectPanel
    }

    fn set_active(&mut self, _active: bool, _window: &mut gpui::Window, _cx: &mut Context<Self>) {}
}

impl Render for RpracticePanel {
    fn render(&mut self, _window: &mut gpui::Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(gpui::rgb(0x1e1e1e))
            .child(
                // Header
                h_flex()
                    .p_2()
                    .border_b_1()
                    .border_color(gpui::rgb(0x3e3e3e))
                    .child(Headline::new("AtCoder Problems").size(HeadlineSize::Small))
            )
            .child(
                // Problem list
                div()
                    .id("rpractice-problem-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .children(
                        self.problems
                            .iter()
                            .map(|problem| self.render_problem_item(problem))
                    )
            )
    }
}
