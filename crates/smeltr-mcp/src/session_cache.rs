//! The last session decoded by a tool, kept briefly for the next call.
//!
//! An agent drills into one session with several tools in a row (summary,
//! breakdown, op summary, memory…), and each used to decode the whole
//! session again: 9-20 s per call on 3.3 M events (#271). The last decoded
//! session is kept with the damage its read reported (#268), keyed on its
//! events file's and metadata's size and mtime so a live session that grew
//! is decoded again, and dropped after [`IDLE_TTL`] so a large session
//! (~1 GB decoded for 3.3 M events) is not pinned in a long-lived server on
//! a machine whose memory is the thing being watched.

use smeltr_core::event::Event;
use smeltr_core::reader::Damage;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant, SystemTime};

/// How long an unused entry is kept.
pub const IDLE_TTL: Duration = Duration::from_secs(60);

/// A session's readable events and what could not be read.
pub type Checked = (Arc<Vec<Event>>, Option<Damage>);

#[derive(PartialEq)]
struct Key {
    dir: PathBuf,
    len: u64,
    mtime: SystemTime,
    /// The damage report also reads the metadata (written event count,
    /// dropped events).
    meta: Option<(u64, SystemTime)>,
}

struct Entry {
    key: Key,
    events: Arc<Vec<Event>>,
    damage: Option<Damage>,
    last_used: Instant,
}

static CACHE: Mutex<Option<Entry>> = Mutex::new(None);
static REAPER: Once = Once::new();

fn key(dir: &Path) -> Option<Key> {
    let m = std::fs::metadata(smeltr_core::session::events_path_for_read(dir)).ok()?;
    let meta = std::fs::metadata(smeltr_core::session::metadata_path(dir))
        .ok()
        .and_then(|m| Some((m.len(), m.modified().ok()?)));
    Some(Key {
        dir: dir.to_path_buf(),
        len: m.len(),
        mtime: m.modified().ok()?,
        meta,
    })
}

/// The session's events and damage, as
/// [`smeltr_core::reader::read_events_checked`] reports them, decoded once
/// for successive calls on it.
pub fn checked(dir: &Path) -> std::io::Result<Checked> {
    let Some(k) = key(dir) else {
        let (events, damage) = smeltr_core::reader::read_events_checked(dir)?;
        return Ok((Arc::new(events), damage));
    };
    if let Ok(mut slot) = CACHE.lock() {
        if let Some(e) = slot.as_mut().filter(|e| e.key == k) {
            e.last_used = Instant::now();
            return Ok((e.events.clone(), e.damage.clone()));
        }
        // Release the previous session before decoding the next one, not
        // after: holding both doubled the peak.
        *slot = None;
    }
    // Decoded without the lock: another session's call need not wait.
    let (events, damage) = smeltr_core::reader::read_events_checked(dir)?;
    let events = Arc::new(events);
    if let Ok(mut slot) = CACHE.lock() {
        *slot = Some(Entry {
            key: k,
            events: events.clone(),
            damage: damage.clone(),
            last_used: Instant::now(),
        });
    }
    REAPER.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("smeltr-mcp-cache-reaper".into())
            .spawn(|| loop {
                std::thread::sleep(IDLE_TTL / 4);
                evict_idle(IDLE_TTL);
            });
    });
    Ok((events, damage))
}

/// [`checked`] for tools that only want the events; damage is logged, as
/// [`smeltr_core::reader::read_events`] does.
pub fn events(dir: &Path) -> std::io::Result<Arc<Vec<Event>>> {
    let (events, damage) = checked(dir)?;
    if let Some(d) = damage {
        tracing::warn!(dir = ?dir, "session data is incomplete: {d}");
    }
    Ok(events)
}

/// The session if it is the one cached and still current; never decodes.
pub fn peek(dir: &Path) -> Option<Checked> {
    let k = key(dir)?;
    let mut slot = CACHE.lock().ok()?;
    let e = slot.as_mut().filter(|e| e.key == k)?;
    e.last_used = Instant::now();
    Some((e.events.clone(), e.damage.clone()))
}

/// [`smeltr_core::reader::for_each_event`], walking the cached session
/// instead when there is one: a tool that only needs a few events streams
/// them from disk rather than decoding a session into the cache, but
/// should not decode it again right after another tool did. Returns the
/// damage: of what was read when streaming, of the whole session when
/// cached.
pub fn visit<F>(
    dir: &Path,
    filter: Option<&smeltr_core::EventFilter>,
    mut f: F,
) -> std::io::Result<Option<Damage>>
where
    F: FnMut(&Event) -> std::ops::ControlFlow<()>,
{
    let Some((cached, damage)) = peek(dir) else {
        return smeltr_core::reader::for_each_event(dir, filter, |e| f(&e));
    };
    for e in cached.iter() {
        if filter.is_none_or(|flt| flt.matches(e)) && f(e).is_break() {
            break;
        }
    }
    Ok(damage)
}

#[cfg(test)]
fn is_cached(dir: &Path) -> bool {
    CACHE
        .lock()
        .map(|s| s.as_ref().is_some_and(|e| e.key.dir == dir))
        .unwrap_or(false)
}

/// Drops the entry when unused for at least `ttl`.
pub fn evict_idle(ttl: Duration) {
    if let Ok(mut slot) = CACHE.lock() {
        if slot.as_ref().is_some_and(|e| e.last_used.elapsed() >= ttl) {
            *slot = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Payload, Source};
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;

    fn session(n: u64) -> std::path::PathBuf {
        let mut w = SessionWriter::create(SessionMetadata::now_starting(SessionId::new())).unwrap();
        for seq in 0..n {
            w.write_event(&Event {
                ts_mono_ns: seq,
                ts_wall_ns: seq,
                session_id: uuid::Uuid::nil(),
                source: Source::Mark,
                pid: None,
                seq,
                payload: Payload::Mark {
                    label: "m".into(),
                    fields: Default::default(),
                },
            })
            .unwrap();
        }
        let dir = w.dir().to_path_buf();
        w.finalize(Some(0), "x".into()).unwrap();
        dir
    }

    /// #271: every tool decoded the whole session on every call (9-20 s on
    /// 3.3 M events); a second call on the same session reuses the first
    /// decode.
    #[test]
    #[serial_test::serial]
    fn a_second_call_on_the_same_session_reuses_the_decode() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let dir = session(3);
        let a = events(&dir).unwrap();
        let b = events(&dir).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(b.len(), 3);
    }

    /// Keyed on the events file's size and mtime: a session that grew (a
    /// live one) is decoded again.
    #[test]
    #[serial_test::serial]
    fn a_changed_session_is_decoded_again() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let dir = session(3);
        let a = events(&dir).unwrap();
        let other = session(5);
        std::fs::copy(other.join("events.cbor.zst"), dir.join("events.cbor.zst")).unwrap();
        let b = events(&dir).unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(b.len(), 5);
    }

    /// One session only: the cache never holds more than the last one.
    #[test]
    #[serial_test::serial]
    fn only_the_last_session_is_kept() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let (d1, d2) = (session(1), session(2));
        let a = events(&d1).unwrap();
        events(&d2).unwrap();
        assert!(!Arc::ptr_eq(&a, &events(&d1).unwrap()));
    }

    /// The tools that need every event of a session go through the cache.
    #[test]
    #[serial_test::serial]
    fn whole_session_tools_share_the_cache() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let dir = session(3);
        let short = dir.file_name().unwrap().to_str().unwrap().to_string();
        let calls: Vec<(&str, serde_json::Value)> = vec![
            ("get_session_summary", serde_json::json!({"session": short})),
            (
                "get_inference_breakdown",
                serde_json::json!({"session": short}),
            ),
            ("get_op_summary", serde_json::json!({"session": short})),
            (
                "get_memory_breakdown",
                serde_json::json!({"session": short}),
            ),
            (
                "get_dispatch_origins",
                serde_json::json!({"session": short}),
            ),
            ("get_model_loads", serde_json::json!({"session": short})),
            (
                "compare_sessions",
                serde_json::json!({"session_a": short, "session_b": short}),
            ),
        ];
        for (tool, args) in calls {
            evict_idle(Duration::ZERO);
            let _ = crate::server::dispatch_call(tool, args);
            assert!(is_cached(&dir), "{tool}");
        }
    }

    /// #268: the damage a read reported is served with the cached events,
    /// never dropped by a cache hit.
    #[test]
    #[serial_test::serial]
    fn damage_is_served_with_the_cached_events() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let dir = session(3);
        let mut meta = smeltr_core::reader::read_metadata(&dir).unwrap();
        meta.dropped_events = Some(42);
        smeltr_core::session::write_metadata(&dir, &meta).unwrap();
        evict_idle(Duration::ZERO);
        let (a, first) = checked(&dir).unwrap();
        let (b, again) = checked(&dir).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(first.as_ref().is_some_and(|d| d.to_string().contains("42")));
        assert_eq!(again, first);

        // Damage recorded after the decode (metadata rewritten) is seen.
        meta.dropped_events = Some(7);
        std::thread::sleep(Duration::from_millis(10));
        smeltr_core::session::write_metadata(&dir, &meta).unwrap();
        let (_, later) = checked(&dir).unwrap();
        assert!(later.is_some_and(|d| d.to_string().contains('7')));
        // Walking the cached session reports it too.
        let walked = visit(&dir, None, |_| std::ops::ControlFlow::Continue(())).unwrap();
        assert!(walked.is_some_and(|d| d.to_string().contains('7')));
    }

    /// `peek` never decodes: it hands out the cached session only while it
    /// is still the one on disk.
    #[test]
    #[serial_test::serial]
    fn peek_returns_only_a_current_cached_session() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let dir = session(3);
        evict_idle(Duration::ZERO);
        assert!(peek(&dir).is_none());
        let a = events(&dir).unwrap();
        assert!(Arc::ptr_eq(&a, &peek(&dir).unwrap().0));
        std::fs::copy(
            session(5).join("events.cbor.zst"),
            dir.join("events.cbor.zst"),
        )
        .unwrap();
        assert!(peek(&dir).is_none(), "stale");
    }

    /// `visit` streams from disk or walks the cached session: same events,
    /// same filter, same early stop.
    #[test]
    #[serial_test::serial]
    fn visit_is_the_same_cached_or_not() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let dir = session(10);
        let filter = smeltr_core::EventFilter {
            from_ts: Some(2),
            ..Default::default()
        };
        let run = || {
            let mut seen = vec![];
            visit(&dir, Some(&filter), |e| {
                seen.push(e.seq);
                if seen.len() == 4 {
                    std::ops::ControlFlow::Break(())
                } else {
                    std::ops::ControlFlow::Continue(())
                }
            })
            .unwrap();
            seen
        };
        evict_idle(Duration::ZERO);
        let streamed = run();
        events(&dir).unwrap();
        assert_eq!(run(), streamed);
        assert_eq!(streamed, vec![2, 3, 4, 5]);
    }

    /// An idle entry is dropped: a large session must not stay pinned in a
    /// long-lived server process.
    #[test]
    #[serial_test::serial]
    fn an_idle_entry_is_evicted() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let dir = session(2);
        let a = events(&dir).unwrap();
        evict_idle(std::time::Duration::from_secs(3600));
        assert!(Arc::ptr_eq(&a, &events(&dir).unwrap()), "not idle yet");
        evict_idle(std::time::Duration::ZERO);
        assert!(!Arc::ptr_eq(&a, &events(&dir).unwrap()));
    }
}
