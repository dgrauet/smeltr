//! Unix-socket server. Reads framed `ClientToDaemon` messages, writes framed
//! `DaemonToClient` responses, holds a reference to the session router and the
//! broadcast bus.

use crate::bus::Bus;
use crate::probes::ProbeRuntime;
use crate::protocol::{ClientToDaemon, DaemonToClient};
use crate::session_router::SessionRouter;
use smeltr_core::reader::{find_session_dir, list_sessions, read_events, read_metadata};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};

pub fn socket_path() -> std::path::PathBuf {
    // An empty variable counts as unset, as in the Python sidecar: an empty
    // XDG_RUNTIME_DIR used to bind a relative `smeltr.sock` (#245).
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    if let Some(p) = var("SMELTR_SOCKET") {
        return p.into();
    }
    var("XDG_RUNTIME_DIR")
        .or_else(|| var("TMPDIR"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "/tmp".into())
        .join("smeltr.sock")
}

/// Whether a live process accepts connections on `path`.
pub fn socket_served(path: &std::path::Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

pub struct Server {
    listener: UnixListener,
    router: Arc<SessionRouter>,
    bus: Bus,
    probe_runtime: Arc<ProbeRuntime>,
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl Server {
    pub fn bind(
        router: Arc<SessionRouter>,
        bus: Bus,
        probe_runtime: Arc<ProbeRuntime>,
        shutdown: tokio::sync::watch::Sender<bool>,
    ) -> std::io::Result<Self> {
        let path = socket_path();
        // Only a stale socket may be replaced (#267): unlinking one another
        // daemon still serves (a second daemon with a different SMELTR_HOME
        // has its own pid file) stole it, and stranded the first daemon
        // once the second stopped and removed the socket.
        if socket_served(&path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!("another smeltrd is serving {}", path.display()),
            ));
        }
        let _ = std::fs::remove_file(&path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener = UnixListener::bind(&path)?;
        Ok(Self {
            listener,
            router,
            bus,
            probe_runtime,
            shutdown,
        })
    }

    pub async fn run(self) -> std::io::Result<()> {
        let mut rx = self.shutdown.subscribe();
        let mut accept_errors: u64 = 0;
        loop {
            tokio::select! {
                accept = self.listener.accept() => {
                    // Never leave the loop on an accept error (#267): with
                    // `accept?`, one EMFILE ended it for good while the
                    // process stayed alive — launchd never restarted it, and
                    // every later connection was refused. Errors here are
                    // transient (fd limit, aborted connection): log, back
                    // off briefly so EMFILE does not spin, and keep serving.
                    let (stream, _) = match accept {
                        Ok(conn) => conn,
                        Err(e) => {
                            accept_errors += 1;
                            if accept_errors == 1 || accept_errors.is_multiple_of(1000) {
                                tracing::warn!(error = %e, count = accept_errors, "accept failed; still serving");
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                            continue;
                        }
                    };
                    let router = self.router.clone();
                    let bus = self.bus.clone();
                    let probe_runtime = self.probe_runtime.clone();
                    let shutdown_tx = self.shutdown.clone();
                    tokio::spawn(handle_connection(stream, router, bus, probe_runtime, shutdown_tx));
                }
                _ = rx.changed() => {
                    if *rx.borrow() { break; }
                }
            }
        }
        Ok(())
    }
}

/// Pids attached via this connection and not yet detached. The record
/// client holds one connection for the whole recording, so a connection
/// dying with leftovers means the client was killed (#143): the daemon
/// auto-detaches, otherwise the scoped session stays open forever and
/// shadows every later recording via `--last`.
#[derive(Default)]
struct ConnAttachments {
    scoped: Vec<u32>,
    hooks: Vec<u32>,
}

async fn handle_connection(
    mut stream: UnixStream,
    router: Arc<SessionRouter>,
    bus: Bus,
    probe_runtime: Arc<ProbeRuntime>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
) {
    let mut attached = ConnAttachments::default();
    if let Err(e) = handle_connection_inner(
        &mut stream,
        &router,
        &bus,
        &probe_runtime,
        &shutdown_tx,
        &mut attached,
    )
    .await
    {
        tracing::warn!(error = %e, "connection ended with error");
    }
    for pid in attached.hooks {
        tracing::warn!(
            pid,
            "client disconnected without DetachMetalHook; auto-detaching"
        );
        probe_runtime.detach_metal_hook(pid).await;
    }
    for pid in attached.scoped {
        tracing::warn!(
            pid,
            "client disconnected without DetachScopedProbes; finalizing scoped session"
        );
        probe_runtime.detach_scoped(pid).await;
        let _ = router.detach_scoped(pid, None, None);
    }
}

async fn handle_connection_inner(
    stream: &mut UnixStream,
    router: &Arc<SessionRouter>,
    bus: &Bus,
    probe_runtime: &Arc<ProbeRuntime>,
    shutdown_tx: &tokio::sync::watch::Sender<bool>,
    attached: &mut ConnAttachments,
) -> std::io::Result<()> {
    loop {
        let msg = match read_msg::<ClientToDaemon>(stream).await? {
            Some(m) => m,
            None => return Ok(()),
        };
        if matches!(msg, ClientToDaemon::SubscribeEvents) {
            write_msg(stream, &DaemonToClient::Ack).await?;
            stream_events(stream, bus, shutdown_tx).await?;
            return Ok(());
        }
        match &msg {
            ClientToDaemon::AttachScopedProbes { pid, .. } => attached.scoped.push(*pid),
            ClientToDaemon::DetachScopedProbes { pid, .. } => attached.scoped.retain(|p| p != pid),
            ClientToDaemon::AttachMetalHook { pid, .. } => attached.hooks.push(*pid),
            ClientToDaemon::DetachMetalHook { pid } => attached.hooks.retain(|p| p != pid),
            _ => {}
        }
        let resp = handle_msg(msg, router, bus, probe_runtime, shutdown_tx).await;
        write_msg(stream, &resp).await?;
    }
}

async fn stream_events(
    stream: &mut UnixStream,
    bus: &Bus,
    shutdown_tx: &tokio::sync::watch::Sender<bool>,
) -> std::io::Result<()> {
    let mut bus_rx = bus.subscribe();
    let mut shutdown_rx = shutdown_tx.subscribe();
    loop {
        tokio::select! {
            biased;
            ev = bus_rx.recv() => {
                match ev {
                    Ok(event) => {
                        let notif = DaemonToClient::EventNotification { event };
                        if write_msg(stream, &notif).await.is_err() {
                            return Ok(());
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "subscriber lagged behind bus");
                    }
                    Err(_) => return Ok(()),
                }
            }
            r = stream.readable() => {
                if r.is_err() {
                    return Ok(());
                }
                let mut tmp = [0u8; 16];
                match stream.try_read(&mut tmp) {
                    Ok(0) => return Ok(()),
                    Ok(_) => continue,
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(_) => return Ok(()),
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() { return Ok(()); }
            }
        }
    }
}

async fn handle_msg(
    msg: ClientToDaemon,
    router: &Arc<SessionRouter>,
    _bus: &Bus,
    probe_runtime: &Arc<ProbeRuntime>,
    shutdown_tx: &tokio::sync::watch::Sender<bool>,
) -> DaemonToClient {
    match msg {
        ClientToDaemon::Hello {
            client,
            scope_token,
        } => {
            tracing::info!(client = %client, "client connected");
            let active_session_ref = scope_token
                .as_deref()
                .and_then(|t| router.session_for_token(t))
                .unwrap_or_else(|| router.ambient_id())
                .short();
            DaemonToClient::Welcome {
                daemon_version: env!("CARGO_PKG_VERSION").to_string(),
                active_session: router.ambient_id(),
                active_session_ref,
            }
        }
        ClientToDaemon::Emit {
            source,
            pid,
            scope_token,
            payload,
        } => match router.append(source, pid, scope_token.as_deref(), payload) {
            Ok(()) => DaemonToClient::Ack,
            Err(e) => DaemonToClient::Error {
                message: e.to_string(),
            },
        },
        ClientToDaemon::ListSessions => match list_sessions() {
            Ok(paths) => {
                let dirs = paths
                    .iter()
                    .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                    .collect();
                DaemonToClient::SessionList { dirs }
            }
            Err(e) => DaemonToClient::Error {
                message: e.to_string(),
            },
        },
        // Decoding a whole session is blocking work: keep it off the
        // runtime's workers (#267). `write_msg` turns a response too large
        // for the socket into an error.
        ClientToDaemon::GetSession { id } => {
            tokio::task::spawn_blocking(move || match find_session_dir(id) {
                Ok(Some(dir)) => match (read_events(&dir), read_metadata(&dir)) {
                    (Ok(events), Ok(metadata)) => {
                        DaemonToClient::SessionEvents { events, metadata }
                    }
                    (Err(e), _) | (_, Err(e)) => DaemonToClient::Error {
                        message: e.to_string(),
                    },
                },
                Ok(None) => DaemonToClient::Error {
                    message: format!("session {id} not found"),
                },
                Err(e) => DaemonToClient::Error {
                    message: e.to_string(),
                },
            })
            .await
            .unwrap_or_else(|e| DaemonToClient::Error {
                message: format!("session read failed: {e}"),
            })
        }
        ClientToDaemon::Shutdown => {
            let _ = shutdown_tx.send(true);
            DaemonToClient::Ack
        }
        ClientToDaemon::AttachScopedProbes {
            pid,
            argv,
            scope_token,
            name,
            chunked,
            gputrace_path,
        } => {
            // Register the scoped session with the router BEFORE starting any
            // probe. `SessionRouter::route` falls back to the ambient session
            // for a pid it doesn't recognize yet, and probes (e.g.
            // `FootprintProbe`, whose `tokio::time::interval` fires its first
            // tick immediately) can emit within the first poll of their task
            // — before the daemon would otherwise get around to telling the
            // router about this pid. Doing it in this order closes that
            // race: the router already knows the pid by the time any probe
            // task is first polled, so no scoped sample can leak into the
            // ambient session.
            match router.attach_scoped(pid, argv, scope_token, name, chunked, gputrace_path) {
                Ok(_) => {
                    probe_runtime.attach_scoped(pid).await;
                    DaemonToClient::Ack
                }
                Err(e) => {
                    // The session failed to open, so there is nowhere
                    // correct to route this pid's events. Leave the probes
                    // off, and say so: answering Ack let `record` carry on
                    // as if the run were recorded while everything landed
                    // in the ambient session (#244).
                    tracing::warn!(error = %e, pid = pid, "failed to open scoped session");
                    DaemonToClient::Error {
                        message: format!("could not open the scoped session: {e}"),
                    }
                }
            }
        }
        ClientToDaemon::DetachScopedProbes {
            pid,
            exit_code,
            term_signal,
        } => {
            probe_runtime.detach_scoped(pid).await;
            let _ = router.detach_scoped(pid, exit_code, term_signal);
            DaemonToClient::Ack
        }
        ClientToDaemon::AttachMetalHook { pid, ring_path } => {
            probe_runtime
                .attach_metal_hook(pid, std::path::PathBuf::from(ring_path))
                .await;
            DaemonToClient::Ack
        }
        ClientToDaemon::DetachMetalHook { pid } => {
            probe_runtime.detach_metal_hook(pid).await;
            DaemonToClient::Ack
        }
        ClientToDaemon::SubscribeEvents => {
            unreachable!("SubscribeEvents handled by handle_connection_inner directly")
        }
    }
}

/// How long a frame's body may take to arrive once its length is read.
/// Idle connections between frames are normal (sidecars, subscribers);
/// a frame started and never finished is not.
const FRAME_BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub async fn read_msg<T: serde::de::DeserializeOwned>(
    stream: &mut UnixStream,
) -> std::io::Result<Option<T>> {
    read_msg_within(stream, FRAME_BODY_TIMEOUT).await
}

/// [`read_msg`] with an explicit body timeout (#267: a client announcing a
/// frame and never sending it held its descriptor forever).
pub async fn read_msg_within<T: serde::de::DeserializeOwned>(
    stream: &mut UnixStream,
    body_timeout: std::time::Duration,
) -> std::io::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    // Grown as bytes arrive rather than sized up front from the announced
    // length, so a stalled frame does not pin 16 MiB.
    let mut buf = Vec::new();
    let mut body = (&mut *stream).take(len as u64);
    match tokio::time::timeout(body_timeout, body.read_to_end(&mut buf)).await {
        Ok(r) => {
            r?;
        }
        Err(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "frame body not received in time",
            ))
        }
    }
    if buf.len() != len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "connection closed mid-frame",
        ));
    }
    let v = ciborium::from_reader(&buf[..])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(Some(v))
}

/// Largest frame either side reads (see [`read_msg_within`]).
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

async fn write_msg(stream: &mut UnixStream, value: &DaemonToClient) -> std::io::Result<()> {
    let encode = |v: &DaemonToClient| -> std::io::Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(256);
        ciborium::into_writer(v, &mut buf)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        Ok(buf)
    };
    let mut buf = encode(value)?;
    // A response no client can read (and whose length would not even fit
    // the u32 prefix past 4 GiB) becomes an error it can act on (#267).
    if buf.len() > MAX_FRAME_BYTES {
        buf = encode(&DaemonToClient::Error {
            message: format!(
                "response too large for the socket ({} bytes); read the session from disk",
                buf.len()
            ),
        })?;
    }
    write_bytes(stream, &buf).await
}

async fn write_bytes(stream: &mut UnixStream, buf: &[u8]) -> std::io::Result<()> {
    let len = (buf.len() as u32).to_le_bytes();
    stream.write_all(&len).await?;
    stream.write_all(buf).await?;
    stream.flush().await
}

/// Unbounded frame writer for tests that play the client side.
#[cfg(test)]
async fn write_raw<T: serde::Serialize>(stream: &mut UnixStream, value: &T) -> std::io::Result<()> {
    let mut buf = Vec::new();
    ciborium::into_writer(value, &mut buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    write_bytes(stream, &buf).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probes::{DaemonSink, ProbeRuntime};
    use crate::session_router::SessionRouter;
    use crate::sessions::ActiveSession;
    use serial_test::serial;
    use smeltr_core::event::{Payload, Source};

    /// #267: `GetSession` answered with the whole session in one frame — 116
    /// MB for a 100k-event ambient session, beyond the 16 MiB every reader
    /// accepts, and its length was cast to u32 unchecked. An oversized
    /// response must become an error the client can act on.
    #[tokio::test]
    async fn an_oversized_response_is_replaced_by_an_error() {
        let (mut daemon_end, mut client_end) = UnixStream::pair().unwrap();
        let meta = smeltr_core::session::SessionMetadata::now_starting(
            smeltr_core::session::SessionId::new(),
        );
        let big = "x".repeat(1 << 20);
        let events = (0..20)
            .map(|i| smeltr_core::event::Event {
                seq: i,
                ts_mono_ns: i,
                ts_wall_ns: i,
                source: Source::Mark,
                pid: None,
                session_id: meta.session_id.0,
                payload: Payload::Mark {
                    label: big.clone(),
                    fields: Default::default(),
                },
            })
            .collect();
        let writer = tokio::spawn(async move {
            write_msg(
                &mut daemon_end,
                &DaemonToClient::SessionEvents {
                    events,
                    metadata: meta,
                },
            )
            .await
        });
        let got: DaemonToClient = read_msg(&mut client_end).await.unwrap().unwrap();
        writer.await.unwrap().unwrap();
        assert!(
            matches!(&got, DaemonToClient::Error { message } if message.contains("too large")),
            "got {:?}",
            std::mem::discriminant(&got)
        );
    }

    /// #267: a client that announces a frame and never sends its body held
    /// its descriptor (and a buffer sized for the announced length) forever;
    /// enough of them exhaust the daemon's descriptors.
    #[tokio::test]
    async fn a_frame_body_that_never_arrives_times_out() {
        let (mut daemon_end, mut client_end) = UnixStream::pair().unwrap();
        client_end.write_all(&100u32.to_le_bytes()).await.unwrap();
        let r = read_msg_within::<ClientToDaemon>(
            &mut daemon_end,
            std::time::Duration::from_millis(100),
        )
        .await;
        assert_eq!(
            r.err().map(|e| e.kind()),
            Some(std::io::ErrorKind::TimedOut)
        );
    }

    async fn connect() -> UnixStream {
        UnixStream::connect(socket_path()).await.unwrap()
    }

    fn temp_env() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", d.path());
        std::env::set_var("SMELTR_SOCKET", d.path().join("smeltr.sock"));
        d
    }

    /// #244: when the scoped session cannot be opened, `record` must hear
    /// it — the daemon answered Ack, so the run went on believing it was
    /// recorded while every event it sent landed in the ambient session.
    /// #245: `smeltr.export()` exported the ambient session — the Welcome
    /// named it whatever the client — and as a raw 16-byte UUID. A client
    /// that sends its scope token now learns its own recording, by a ref
    /// every CLI command resolves.
    /// #245: an empty `XDG_RUNTIME_DIR` made the daemon bind a relative
    /// `smeltr.sock` in its cwd, while the Python sidecar (which skips empty
    /// values) looked in $TMPDIR — and never connected.
    #[test]
    #[serial]
    fn empty_env_values_are_treated_as_unset() {
        let saved: Vec<_> = ["SMELTR_SOCKET", "XDG_RUNTIME_DIR", "TMPDIR"]
            .iter()
            .map(|k| (k, std::env::var_os(k)))
            .collect();
        std::env::set_var("SMELTR_SOCKET", "");
        std::env::set_var("XDG_RUNTIME_DIR", "");
        std::env::set_var("TMPDIR", "/t");
        let p = socket_path();
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        assert_eq!(p, std::path::PathBuf::from("/t/smeltr.sock"));
    }

    #[tokio::test]
    #[serial]
    async fn welcome_names_the_recording_of_the_clients_scope_token() {
        let _home = temp_env();
        let ambient = Arc::new(ActiveSession::open_new().unwrap());
        let router = Arc::new(SessionRouter::new(ambient.clone(), None, None));
        let sink = Arc::new(DaemonSink {
            router: router.clone(),
        });
        let probe_runtime = Arc::new(ProbeRuntime::start_global(sink));
        let (tx, _rx) = tokio::sync::watch::channel(false);
        let scoped = router
            .attach_scoped(
                4242,
                vec!["x".into()],
                Some("tok".into()),
                None,
                false,
                None,
            )
            .unwrap();

        let hello = |token: Option<&str>| ClientToDaemon::Hello {
            client: "t".into(),
            scope_token: token.map(str::to_string),
        };
        let reply = |r: DaemonToClient| match r {
            DaemonToClient::Welcome {
                active_session_ref, ..
            } => active_session_ref,
            other => panic!("{other:?}"),
        };
        let bus = Bus::new();
        let mine = handle_msg(hello(Some("tok")), &router, &bus, &probe_runtime, &tx).await;
        let anon = handle_msg(hello(None), &router, &bus, &probe_runtime, &tx).await;
        probe_runtime.shutdown().await;
        assert_eq!(reply(mine), scoped.short());
        assert_eq!(reply(anon), ambient.id().short());
    }

    #[tokio::test]
    #[serial]
    async fn attach_reports_a_session_that_failed_to_open() {
        use std::os::unix::fs::PermissionsExt;
        let home = temp_env();
        let ambient = Arc::new(ActiveSession::open_new().unwrap());
        let router = Arc::new(SessionRouter::new(ambient, None, None));
        let sink = Arc::new(DaemonSink {
            router: router.clone(),
        });
        let probe_runtime = Arc::new(ProbeRuntime::start_global(sink));
        let (tx, _rx) = tokio::sync::watch::channel(false);
        // No new session directory can be created.
        let sessions = home.path().join("sessions");
        std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o555)).unwrap();

        let resp = handle_msg(
            ClientToDaemon::AttachScopedProbes {
                pid: std::process::id(),
                argv: vec!["x".into()],
                scope_token: None,
                name: None,
                chunked: false,
                gputrace_path: None,
            },
            &router,
            &Bus::new(),
            &probe_runtime,
            &tx,
        )
        .await;
        std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o755)).unwrap();
        probe_runtime.shutdown().await;
        assert!(matches!(resp, DaemonToClient::Error { .. }), "{resp:?}");
    }

    /// #267: a second daemon (e.g. `smeltr daemon start` with only
    /// SMELTR_HOME overridden) unlinked the live daemon's socket and bound its
    /// own; when it stopped, it removed the socket and the first daemon was
    /// alive but unreachable.
    #[tokio::test]
    #[serial]
    async fn bind_refuses_a_socket_another_daemon_serves() {
        let _home = temp_env();
        let live = std::os::unix::net::UnixListener::bind(socket_path()).unwrap();
        let ambient = Arc::new(ActiveSession::open_new().unwrap());
        let router = Arc::new(SessionRouter::new(ambient, None, None));
        let sink = Arc::new(DaemonSink {
            router: router.clone(),
        });
        let probe_runtime = Arc::new(ProbeRuntime::start_global(sink));
        let (tx, _rx) = tokio::sync::watch::channel(false);
        let err = Server::bind(router, Bus::new(), probe_runtime, tx)
            .err()
            .expect("bind must refuse a served socket");
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
        assert!(
            std::os::unix::net::UnixStream::connect(socket_path()).is_ok(),
            "the live daemon's socket must be left in place"
        );
        drop(live);
    }

    /// A socket file left by a dead daemon is still replaced.
    #[tokio::test]
    #[serial]
    async fn bind_replaces_a_stale_socket() {
        let _home = temp_env();
        drop(std::os::unix::net::UnixListener::bind(socket_path()).unwrap());
        assert!(socket_path().exists(), "stale socket file");
        let ambient = Arc::new(ActiveSession::open_new().unwrap());
        let router = Arc::new(SessionRouter::new(ambient, None, None));
        let sink = Arc::new(DaemonSink {
            router: router.clone(),
        });
        let probe_runtime = Arc::new(ProbeRuntime::start_global(sink));
        let (tx, _rx) = tokio::sync::watch::channel(false);
        assert!(Server::bind(router, Bus::new(), probe_runtime, tx).is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn hello_round_trip() {
        let _home = temp_env();
        let ambient = Arc::new(ActiveSession::open_new().unwrap());
        let bus = Bus::new();
        let router = Arc::new(SessionRouter::new(ambient.clone(), None, None));
        let sink = Arc::new(DaemonSink {
            router: router.clone(),
        });
        let probe_runtime = Arc::new(ProbeRuntime::start_global(sink));
        let (tx, _rx) = tokio::sync::watch::channel(false);
        let server = Server::bind(router.clone(), bus, probe_runtime.clone(), tx.clone()).unwrap();
        tokio::spawn(server.run());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut s = connect().await;
        write_raw(
            &mut s,
            &ClientToDaemon::Hello {
                client: "test".into(),
                scope_token: None,
            },
        )
        .await
        .unwrap();
        let resp: DaemonToClient = read_msg(&mut s).await.unwrap().unwrap();
        assert!(matches!(resp, DaemonToClient::Welcome { .. }));

        write_raw(
            &mut s,
            &ClientToDaemon::Emit {
                source: Source::Mark,
                pid: None,
                scope_token: None,
                payload: Payload::Mark {
                    label: "from-test".into(),
                    fields: Default::default(),
                },
            },
        )
        .await
        .unwrap();
        let resp: DaemonToClient = read_msg(&mut s).await.unwrap().unwrap();
        assert!(matches!(resp, DaemonToClient::Ack));

        let _ = tx.send(true);
        probe_runtime.shutdown().await;
    }

    /// Regression test for the ambient-routing race: `FootprintProbe`'s
    /// `tokio::time::interval` fires its first tick as soon as the probe
    /// task is polled, which can be within the first millisecond of the
    /// probe being spawned. If the router doesn't know the pid yet at that
    /// point, `SessionRouter::route` falls back to the ambient session and
    /// the sample is misrouted (observed as 20 stray `ProcFootprint` events
    /// in ambient under full parallel test load).
    ///
    /// This drives `handle_msg` — the real production code path — with an
    /// `AttachScopedProbes` message using our own pid (always resolvable by
    /// the footprint probe) and asserts every `ProcFootprint` sample that
    /// arrives before detach lands in the scoped session, never ambient.
    ///
    /// Requires a `multi_thread` runtime: on a `current_thread` runtime the
    /// task spawned by `ProbeRuntime::attach_scoped` cannot get its first
    /// poll until the calling task yields, so `router.attach_scoped` (fully
    /// synchronous, no `.await` inside) always finishes first regardless of
    /// call order — the pre-fix bug would not reproduce there. Verified by
    /// temporarily reverting the fix: with `multi_thread`, the reverted
    /// code fails this test 5/5 runs (a probe thread can get scheduled
    /// before the router-registration line runs); the fixed code passes
    /// 6/6 runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[serial]
    async fn attach_scoped_probes_registers_router_before_probes_can_emit() {
        let _home = temp_env();
        let ambient = Arc::new(ActiveSession::open_new().unwrap());
        let bus = Bus::new();
        let router = Arc::new(SessionRouter::new(ambient.clone(), None, None));
        let sink = Arc::new(DaemonSink {
            router: router.clone(),
        });
        let probe_runtime = Arc::new(ProbeRuntime::start_global(sink));
        let (tx, _rx) = tokio::sync::watch::channel(false);

        let pid = std::process::id();
        let resp = handle_msg(
            ClientToDaemon::AttachScopedProbes {
                pid,
                argv: vec!["self".into()],
                scope_token: None,
                name: None,
                chunked: false,
                gputrace_path: None,
            },
            &router,
            &bus,
            &probe_runtime,
            &tx,
        )
        .await;
        assert!(matches!(resp, DaemonToClient::Ack));

        // Give the footprint probe's first tick a wide margin to fire and
        // reach the router before we tear everything down.
        tokio::time::sleep(std::time::Duration::from_millis(3000)).await;

        probe_runtime.detach_scoped(pid).await;
        router.detach_scoped(pid, Some(0), None);
        let ambient_id = ambient.id();
        ambient.finalize(Some(0), None, "test").unwrap();

        let dirs = smeltr_core::reader::list_sessions().unwrap();
        let mut ambient_footprints = 0;
        let mut scoped_footprints = 0;
        for d in &dirs {
            let meta = smeltr_core::reader::read_metadata(d).unwrap();
            let evs = smeltr_core::reader::read_events(d).unwrap();
            let n = evs
                .iter()
                .filter(|e| matches!(&e.payload, smeltr_core::event::Payload::ProcFootprint { pid: p, .. } if *p == pid))
                .count();
            if meta.session_id == ambient_id {
                ambient_footprints += n;
            } else {
                scoped_footprints += n;
            }
        }
        assert_eq!(
            ambient_footprints, 0,
            "footprint samples for the scoped pid leaked into the ambient session"
        );
        assert!(
            scoped_footprints > 0,
            "expected at least one footprint sample in the scoped session (probe never emitted \
             within the wait margin — test may need a longer sleep, not a correctness signal)"
        );

        let _ = tx.send(true);
        probe_runtime.shutdown().await;
    }
}
