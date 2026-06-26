// Rpractice - Solo practice mode for AtCoder problems
//
// File structure:
// - rpractice.rs: Main entry point, exports public API
// - panel.rs: Problem list sidebar panel
// - view.rs: Practice view (copied from rduel, simplified)
// - fetcher.rs: Problem fetching from AtCoder
// - storage.rs: SQLite database for progress tracking

use gpui::{App, Context, Window};
use workspace::Workspace;
use zed_actions::rpractice::OpenRpractice;

mod panel;
// mod view;  // TODO: Fix view.rs (copied from rduel, needs simplification)
mod fetcher;
mod storage;

pub use panel::RpracticePanel;
// pub use view::RpracticeView;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, cx| {
        workspace.register_action(|workspace, _: &OpenRpractice, window, cx| {
            open_rpractice(workspace, window, cx);
        });
    })
    .detach();
}

fn open_rpractice(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    // TODO: Check if atcoder_user is set in settings
    // TODO: If not set, show input dialog
    // TODO: If set, open RpracticePanel

    // For now, just open the panel
    workspace.toggle_panel_focus::<RpracticePanel>(window, cx);
}
