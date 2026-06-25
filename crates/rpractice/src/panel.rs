// Panel module - Problem list sidebar

use gpui::{prelude::*, actions, EventEmitter, FocusHandle, Focusable, Render, WeakEntity};
use ui::prelude::*;
use workspace::{Workspace, dock::{Panel, PanelEvent, DockPosition}};

actions!(rpractice, [ToggleFocus]);

const RPRACTICE_PANEL_KEY: &str = "RpracticePanel";

pub struct RpracticePanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
}

impl RpracticePanel {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            workspace,
        }
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
            .child(Label::new("Rpractice Panel - TODO"))
    }
}
