// Rpractice - Solo practice mode for AtCoder problems
//
// File structure:
// - rpractice.rs: Main entry point, exports public API
// - panel.rs: Problem list sidebar panel
// - view.rs: Practice view (copied from rduel, simplified)
// - fetcher.rs: Problem fetching from AtCoder
// - storage.rs: SQLite database for progress tracking

mod panel;
// mod view;  // TODO: Fix view.rs (copied from rduel, needs simplification)
mod fetcher;
mod storage;

pub use panel::RpracticePanel;
// pub use view::RpracticeView;

pub fn init(cx: &mut gpui::App) {
    // TODO: Register actions and settings
}
