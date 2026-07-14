//! TUI event loop. Consumes Events from an mpsc::Receiver, updates UiState,
//! handles keyboard, redraws periodically.

use crate::render::{render, Panel, RenderCtx, RenderOverlay};
use crate::state::UiState;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
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
    pub show_hot_kernels: bool,
    pub show_models: bool,
    pub status: Option<String>,
    pub filter: Option<String>,
    pub filtering: Option<String>,
    pub scrub: Option<crate::scrub::ScrubState>,
}

impl App {
    pub fn new(mode_label: &'static str) -> Self {
        Self {
            state: UiState::default(),
            focus: Panel::Timeline,
            paused: false,
            mode_label,
            quit_requested: false,
            show_hot_kernels: false,
            show_models: false,
            status: None,
            filter: None,
            filtering: None,
            scrub: None,
        }
    }

    /// Installs the replay timeline. If it starts fully played (--speed 0),
    /// fold everything so the UI opens populated instead of blank.
    pub fn set_scrub(&mut self, scrub: crate::scrub::ScrubState) {
        if scrub.at_end() {
            self.state = crate::state::UiState::rebuild(scrub.events());
        }
        self.scrub = Some(scrub);
    }

    /// Gauge state for the replay title; None in live mode.
    fn replay_gauge(&self) -> Option<crate::render::ReplayGauge> {
        self.scrub.as_ref().map(|s| crate::render::ReplayGauge {
            playing: !self.paused,
            at_end: s.at_end(),
            position_ns: s.position_ns(),
            duration_ns: s.duration_ns(),
        })
    }

    fn apply_seek(
        &mut self,
        f: impl FnOnce(&mut crate::scrub::ScrubState) -> crate::scrub::SeekOutcome,
    ) {
        let Some(scrub) = self.scrub.as_mut() else {
            return;
        };
        match f(scrub) {
            crate::scrub::SeekOutcome::Forward(r) => {
                for ev in &scrub.events()[r] {
                    self.state.ingest(ev);
                }
            }
            crate::scrub::SeekOutcome::Rewind(r) => {
                self.state = crate::state::UiState::rebuild(&scrub.events()[r]);
            }
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
        let mut last_tick = Instant::now();
        loop {
            if self.quit_requested {
                return Ok(());
            }
            if event::poll(Duration::from_millis(10))? {
                if let CtEvent::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        self.handle_key(key.code, key.modifiers);
                    }
                }
            }
            if let Some(scrub) = self.scrub.as_mut() {
                let dt = last_tick.elapsed();
                last_tick = Instant::now();
                if !self.paused {
                    let r = scrub.advance(dt);
                    for ev in &scrub.events()[r] {
                        self.state.ingest(ev);
                    }
                }
            } else {
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
            }
            if last_draw.elapsed() >= frame_period {
                let ctx = RenderCtx {
                    focus: self.focus,
                    paused: self.paused,
                    mode_label: self.mode_label,
                    show_hot_kernels: self.show_hot_kernels,
                    show_models: self.show_models,
                };
                let overlay = RenderOverlay {
                    status: self.status.as_deref(),
                    filter: self.filter.as_deref(),
                    filtering: self.filtering.as_deref(),
                    replay: self.replay_gauge(),
                };
                term.draw(|f| render(f, &self.state, ctx, overlay))?;
                last_draw = Instant::now();
            }
        }
    }

    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        self.status = None; // any key dismisses the previous status
        if self.filtering.is_some() {
            match code {
                KeyCode::Char(c) => {
                    if let Some(b) = self.filtering.as_mut() {
                        b.push(c);
                    }
                }
                KeyCode::Backspace => {
                    if let Some(b) = self.filtering.as_mut() {
                        b.pop();
                    }
                }
                KeyCode::Enter => {
                    let q = self.filtering.take().unwrap_or_default();
                    self.filter = if q.is_empty() { None } else { Some(q) };
                }
                KeyCode::Esc => self.filtering = None,
                _ => {}
            }
            return;
        }
        match code {
            KeyCode::Left | KeyCode::Right if self.scrub.is_some() => {
                let step: i64 = if mods.contains(KeyModifiers::SHIFT) {
                    30
                } else {
                    5
                };
                let delta = if code == KeyCode::Left { -step } else { step };
                self.apply_seek(|s| s.seek_by_secs(delta));
            }
            KeyCode::Home if self.scrub.is_some() => {
                self.apply_seek(|s| s.seek_to_ns(0));
            }
            KeyCode::End if self.scrub.is_some() => {
                self.apply_seek(|s| s.seek_to_ns(u64::MAX));
            }
            KeyCode::Char('q') | KeyCode::Esc => self.quit_requested = true,
            KeyCode::Char('/') => self.filtering = Some(String::new()),
            KeyCode::Tab => self.focus = self.focus.next(),
            KeyCode::Char(' ') => self.paused = !self.paused,
            KeyCode::Char('r') => {
                self.state = UiState::default();
            }
            KeyCode::Char('s') => {
                self.status = Some(match crate::snapshot::write_snapshot(&self.state) {
                    Ok(p) => format!("snapshot \u{2192} {}", p.display()),
                    Err(e) => format!("snapshot failed: {e}"),
                });
            }
            KeyCode::Char('k') | KeyCode::Char('K') => {
                self.show_hot_kernels = !self.show_hot_kernels;
            }
            KeyCode::Char('M') => {
                self.show_models = !self.show_models;
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

    fn mk_mark_event(ts_mono_ns: u64, label: &str) -> SmeltrEvent {
        SmeltrEvent {
            ts_mono_ns,
            ts_wall_ns: ts_mono_ns,
            session_id: Uuid::nil(),
            source: Source::Mark,
            pid: None,
            seq: 1,
            payload: Payload::Mark {
                label: label.into(),
                fields: Default::default(),
            },
        }
    }

    fn replay_app() -> App {
        let mut app = App::new("replay");
        let evs: Vec<SmeltrEvent> = (0..10u64)
            .map(|i| mk_mark_event(i * 1_000_000_000, &format!("m{i}")))
            .collect();
        app.scrub = Some(crate::scrub::ScrubState::new(evs, 1.0));
        app
    }

    #[test]
    fn right_key_seeks_forward_and_ingests() {
        let mut app = replay_app();
        app.handle_key(KeyCode::Right, KeyModifiers::NONE); // +5s → events 0..=5s
        assert_eq!(app.scrub.as_ref().unwrap().position_ns(), 5_000_000_000);
        assert_eq!(app.state.log_feed.len(), 6);
    }

    #[test]
    fn left_key_rewinds_and_rebuilds() {
        let mut app = replay_app();
        app.handle_key(KeyCode::Right, KeyModifiers::NONE);
        app.handle_key(KeyCode::Right, KeyModifiers::NONE); // 10s clamped to 9s duration
        let full = app.state.log_feed.len();
        app.handle_key(KeyCode::Left, KeyModifiers::NONE); // -5s
        assert!(app.state.log_feed.len() < full);
        assert_eq!(app.scrub.as_ref().unwrap().position_ns(), 4_000_000_000);
    }

    #[test]
    fn shift_arrows_seek_thirty_seconds() {
        let mut app = replay_app();
        app.handle_key(KeyCode::Right, KeyModifiers::SHIFT); // +30s → clamped to 9s end
        assert!(app.scrub.as_ref().unwrap().at_end());
        app.handle_key(KeyCode::Left, KeyModifiers::SHIFT); // -30s → clamped to 0
        assert_eq!(app.scrub.as_ref().unwrap().position_ns(), 0);
        assert_eq!(app.state.log_feed.len(), 1); // event at t=0 only
    }

    #[test]
    fn home_end_jump_to_bounds() {
        let mut app = replay_app();
        app.handle_key(KeyCode::End, KeyModifiers::NONE);
        assert!(app.scrub.as_ref().unwrap().at_end());
        assert_eq!(app.state.log_feed.len(), 10);
        app.handle_key(KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(app.scrub.as_ref().unwrap().position_ns(), 0);
    }

    #[test]
    fn gauge_reports_end_state_after_end_key() {
        let mut app = replay_app();
        assert!(!app.replay_gauge().unwrap().at_end);
        app.handle_key(KeyCode::End, KeyModifiers::NONE);
        assert!(app.replay_gauge().unwrap().at_end, "End key must surface ■");
        let live = App::new("live");
        assert!(live.replay_gauge().is_none());
    }

    #[test]
    fn speed_zero_launch_opens_fully_populated() {
        let mut app = App::new("replay");
        let evs: Vec<SmeltrEvent> = (0..5u64)
            .map(|i| mk_mark_event(i * 1_000_000_000, &format!("m{i}")))
            .collect();
        app.set_scrub(crate::scrub::ScrubState::new(evs, 0.0));
        assert_eq!(
            app.state.log_feed.len(),
            5,
            "speed-0 launch must open with all events folded"
        );
    }

    #[test]
    fn seek_back_at_start_does_not_wipe_state() {
        let mut app = replay_app();
        app.handle_key(KeyCode::Home, KeyModifiers::NONE); // rebuilds to t=0 -> 1 entry
        assert_eq!(app.state.log_feed.len(), 1);
        app.handle_key(KeyCode::Left, KeyModifiers::NONE); // clamped no-op at 0
        assert_eq!(
            app.state.log_feed.len(),
            1,
            "clamped seek-back must not wipe the t=0 state"
        );
    }

    #[test]
    fn seek_keys_inert_in_live_mode() {
        let mut app = App::new("live");
        app.handle_key(KeyCode::Right, KeyModifiers::NONE);
        app.handle_key(KeyCode::Home, KeyModifiers::NONE);
        assert!(app.scrub.is_none());
        assert_eq!(app.state.log_feed.len(), 0);
    }

    #[test]
    fn handle_key_tab_cycles_focus() {
        let mut app = App::new("test");
        let initial = app.focus;
        app.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_ne!(app.focus, initial);
    }

    #[test]
    fn handle_key_q_requests_quit() {
        let mut app = App::new("test");
        app.handle_key(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(app.quit_requested);
    }

    #[test]
    fn handle_key_esc_requests_quit() {
        let mut app = App::new("test");
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.quit_requested);
    }

    #[test]
    fn handle_key_space_toggles_pause() {
        let mut app = App::new("test");
        assert!(!app.paused);
        app.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(app.paused);
        app.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(!app.paused);
    }

    #[test]
    fn handle_key_k_toggles_hot_kernels_panel() {
        let mut app = App::new("test");
        assert!(!app.show_hot_kernels);
        app.handle_key(KeyCode::Char('k'), KeyModifiers::NONE);
        assert!(app.show_hot_kernels);
        app.handle_key(KeyCode::Char('K'), KeyModifiers::NONE);
        assert!(!app.show_hot_kernels);
    }

    #[test]
    fn handle_key_uppercase_m_toggles_models_view() {
        let mut app = App::new("test");
        assert!(!app.show_models);
        app.handle_key(KeyCode::Char('M'), KeyModifiers::NONE);
        assert!(app.show_models);
        app.handle_key(KeyCode::Char('M'), KeyModifiers::NONE);
        assert!(!app.show_models);
        // Lowercase 'm' does NOT toggle.
        app.handle_key(KeyCode::Char('m'), KeyModifiers::NONE);
        assert!(!app.show_models);
    }

    #[test]
    #[serial_test::serial]
    fn handle_key_s_writes_snapshot_and_sets_status() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let mut app = App::new("test");
        app.handle_key(KeyCode::Char('s'), KeyModifiers::NONE);
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .starts_with("snapshot \u{2192} "),
            "status was {:?}",
            app.status
        );
        let dir = home.path().join("snapshots");
        let count = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(count, 1, "expected one snapshot file");
        std::env::remove_var("SMELTR_HOME");
    }

    #[test]
    fn handle_key_clears_status_on_next_key() {
        let mut app = App::new("test");
        app.status = Some("stale".into());
        app.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        assert!(app.status.is_none());
    }

    #[test]
    fn slash_enters_filter_input_and_builds_query() {
        let mut app = App::new("test");
        app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        assert_eq!(app.filtering.as_deref(), Some(""));
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('b'), KeyModifiers::NONE);
        assert_eq!(app.filtering.as_deref(), Some("ab"));
        app.handle_key(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(app.filtering.as_deref(), Some("a"));
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.filter.as_deref(), Some("a"));
        assert!(app.filtering.is_none());
    }

    #[test]
    fn empty_enter_clears_filter() {
        let mut app = App::new("test");
        app.filter = Some("old".into());
        app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.filter.is_none());
        assert!(app.filtering.is_none());
    }

    #[test]
    fn esc_cancels_input_keeps_prior_filter() {
        let mut app = App::new("test");
        app.filter = Some("keep".into());
        app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('x'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.filtering.is_none());
        assert_eq!(app.filter.as_deref(), Some("keep"));
        assert!(!app.quit_requested, "Esc in filter mode must not quit");
    }

    #[test]
    fn esc_in_normal_mode_still_quits() {
        let mut app = App::new("test");
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.quit_requested);
    }

    #[test]
    fn filtering_swallows_other_keys() {
        let mut app = App::new("test");
        app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('q'), KeyModifiers::NONE); // literal, must not quit
        assert!(!app.quit_requested);
        assert_eq!(app.filtering.as_deref(), Some("q"));
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
            payload: Payload::Mark {
                label: "x".into(),
                fields: Default::default(),
            },
        });
        assert_eq!(app.state.events_total, 1);
        app.handle_key(KeyCode::Char('r'), KeyModifiers::NONE);
        assert_eq!(app.state.events_total, 0);
    }
}
