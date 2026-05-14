//! TUI event loop. Consumes Events from an mpsc::Receiver, updates UiState,
//! handles keyboard, redraws periodically.

use crate::render::{render, Panel, RenderCtx};
use crate::state::UiState;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use smeltr_core::event::Event as SmeltrEvent;
use std::io::stdout;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub struct App {
    pub state: UiState,
    pub focus: Panel,
    pub paused: bool,
    pub mode_label: &'static str,
    pub quit_requested: bool,
}

impl App {
    pub fn new(mode_label: &'static str) -> Self {
        Self {
            state: UiState::default(),
            focus: Panel::Timeline,
            paused: false,
            mode_label,
            quit_requested: false,
        }
    }

    /// Runs the TUI until the user quits. Consumes events from `rx`.
    pub async fn run(mut self, mut rx: mpsc::Receiver<SmeltrEvent>) -> std::io::Result<()> {
        enable_raw_mode()?;
        let mut out = stdout();
        execute!(out, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(out);
        let mut term = Terminal::new(backend)?;
        let result = self.event_loop(&mut term, &mut rx).await;
        disable_raw_mode()?;
        execute!(term.backend_mut(), LeaveAlternateScreen)?;
        let _ = term.show_cursor();
        result
    }

    async fn event_loop(
        &mut self,
        term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
        rx: &mut mpsc::Receiver<SmeltrEvent>,
    ) -> std::io::Result<()> {
        let frame_period = Duration::from_millis(33);
        let mut last_draw = Instant::now() - frame_period;
        loop {
            if self.quit_requested {
                return Ok(());
            }
            if event::poll(Duration::from_millis(10))? {
                if let CtEvent::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        self.handle_key(key.code);
                    }
                }
            }
            loop {
                match rx.try_recv() {
                    Ok(ev) => {
                        if !self.paused {
                            self.state.ingest(&ev);
                        }
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => break,
                }
            }
            if last_draw.elapsed() >= frame_period {
                let ctx = RenderCtx {
                    focus: self.focus,
                    paused: self.paused,
                    mode_label: self.mode_label,
                };
                term.draw(|f| render(f, &self.state, ctx))?;
                last_draw = Instant::now();
            }
        }
    }

    pub fn handle_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit_requested = true,
            KeyCode::Tab => self.focus = self.focus.next(),
            KeyCode::Char(' ') => self.paused = !self.paused,
            KeyCode::Char('r') => {
                self.state = UiState::default();
            }
            KeyCode::Char('s') => {
                // Reserved: snapshot. No-op v1.
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Payload, Source};
    use uuid::Uuid;

    #[test]
    fn handle_key_tab_cycles_focus() {
        let mut app = App::new("test");
        let initial = app.focus;
        app.handle_key(KeyCode::Tab);
        assert_ne!(app.focus, initial);
    }

    #[test]
    fn handle_key_q_requests_quit() {
        let mut app = App::new("test");
        app.handle_key(KeyCode::Char('q'));
        assert!(app.quit_requested);
    }

    #[test]
    fn handle_key_esc_requests_quit() {
        let mut app = App::new("test");
        app.handle_key(KeyCode::Esc);
        assert!(app.quit_requested);
    }

    #[test]
    fn handle_key_space_toggles_pause() {
        let mut app = App::new("test");
        assert!(!app.paused);
        app.handle_key(KeyCode::Char(' '));
        assert!(app.paused);
        app.handle_key(KeyCode::Char(' '));
        assert!(!app.paused);
    }

    #[test]
    fn handle_key_r_resets_state() {
        let mut app = App::new("test");
        app.state.ingest(&SmeltrEvent {
            ts_mono_ns: 1,
            ts_wall_ns: 1,
            session_id: Uuid::nil(),
            source: Source::Mark,
            pid: None,
            seq: 1,
            payload: Payload::Mark { label: "x".into() },
        });
        assert_eq!(app.state.events_total, 1);
        app.handle_key(KeyCode::Char('r'));
        assert_eq!(app.state.events_total, 0);
    }
}
