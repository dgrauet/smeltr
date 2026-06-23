//! TUI for smeltr.

pub mod app;
pub mod breakdown;
pub mod live;
pub mod models;
pub mod render;
pub mod replay;
pub mod snapshot;
pub mod state;

pub use app::App;
pub use render::{render, Panel, RenderCtx};
pub use state::UiState;
