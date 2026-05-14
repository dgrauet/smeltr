//! TUI for smeltr.

pub mod app;
pub mod live;
pub mod render;
pub mod replay;
pub mod state;

pub use app::App;
pub use render::{render, Panel, RenderCtx};
pub use state::UiState;
