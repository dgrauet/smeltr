//! TUI for smeltr: ratatui-based panels driven by an event stream.

pub mod render;
pub mod state;

pub use render::{render, Panel, RenderCtx};
pub use state::UiState;
