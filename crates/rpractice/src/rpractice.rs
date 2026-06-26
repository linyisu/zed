use gpui::{App, AppContext, Context, Empty, IntoElement, Render, WeakEntity, Window};
use ui::{ButtonCommon, Clickable, Color, IconButton, IconName, IconSize, Toggleable, Tooltip};
use util::ResultExt;
use workspace::{HideStatusItem, ItemHandle, StatusItemView, Workspace};
use zed_actions::rpractice::OpenRpractice;

mod fetcher;
mod storage;
mod view;

pub use view::RpracticeView;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &OpenRpractice, window, cx| {
            open_rpractice(workspace, window, cx);
        });
        let Some(window) = window else {
            return;
        };
        let sidebar_toggle = cx.new(|_| RpracticeSidebarToggle::new());
        workspace.status_bar().update(cx, |status_bar, cx| {
            status_bar.add_left_item(sidebar_toggle, window, cx);
        });
    })
    .detach();
}

fn open_rpractice(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let existing = {
        let active_pane = workspace.active_pane().read(cx);
        active_pane
            .items()
            .find_map(|item| item.downcast::<RpracticeView>())
    };
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }

    let workspace_handle = cx.entity().downgrade();
    let project = workspace.project().clone();
    let language_registry = project.read(cx).languages().clone();
    let rpractice =
        cx.new(|cx| RpracticeView::new(workspace_handle, project, language_registry, window, cx));
    workspace.add_item_to_active_pane(Box::new(rpractice), None, true, window, cx);
}

struct RpracticeSidebarToggle {
    active_rpractice: Option<WeakEntity<RpracticeView>>,
}

impl RpracticeSidebarToggle {
    fn new() -> Self {
        Self {
            active_rpractice: None,
        }
    }
}

impl Render for RpracticeSidebarToggle {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let Some(active_rpractice) = self.active_rpractice.clone() else {
            return Empty.into_any_element();
        };
        let sidebar_open = active_rpractice
            .read_with(_cx, |rpractice, _| rpractice.sidebar_open())
            .unwrap_or(false);

        IconButton::new("rpractice-toggle-sidebar-status", IconName::ListTree)
            .icon_size(IconSize::Small)
            .toggle_state(sidebar_open)
            .selected_icon_color(Color::Accent)
            .tooltip(Tooltip::text("Toggle Rpractice problem list"))
            .on_click(move |_, _, cx| {
                active_rpractice
                    .update(cx, |rpractice, cx| {
                        rpractice.toggle_sidebar_from_status_bar(cx);
                    })
                    .log_err();
            })
            .into_any_element()
    }
}

impl StatusItemView for RpracticeSidebarToggle {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_rpractice = active_pane_item
            .and_then(|item| item.downcast::<RpracticeView>())
            .map(|rpractice| rpractice.downgrade());
        cx.notify();
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}
