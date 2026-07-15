//! Regression for #114: the bus client must survive a daemon restart —
//! reconnect with backoff, resubscribe, and report state transitions —
//! instead of ending silently on socket EOF (which froze the TUI forever).

use smeltr_core::codec::write_frame;
use smeltr_core::event::{Event, Payload, Source};
use smeltr_daemon::client::{subscribe_events_reconnecting, ConnState};
use smeltr_daemon::protocol::{ClientToDaemon, DaemonToClient};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::sync::{mpsc, watch};

fn test_event(seq: u64) -> Event {
    Event {
        ts_mono_ns: seq,
        ts_wall_ns: seq,
        session_id: uuid::Uuid::nil(),
        source: Source::MetalHook,
        pid: None,
        seq,
        payload: Payload::MetalCbScheduled {
            cb_id: seq,
            queue_id: 1,
        },
    }
}

/// Accept one client: full handshake, then send `event`, then drop.
async fn serve_once(listener: &UnixListener, event: Event) {
    let (mut stream, _) = listener.accept().await.expect("accept");
    // Hello -> Welcome
    let _hello: ClientToDaemon = smeltr_daemon::server::read_msg(&mut stream)
        .await
        .expect("read hello")
        .expect("hello frame");
    let mut buf = Vec::new();
    write_frame(
        &mut buf,
        &DaemonToClient::Welcome {
            daemon_version: "test".into(),
            active_session: smeltr_core::session::SessionId::new(),
        },
    )
    .unwrap();
    stream.write_all(&buf).await.unwrap();
    // SubscribeEvents -> Ack
    let _sub: ClientToDaemon = smeltr_daemon::server::read_msg(&mut stream)
        .await
        .expect("read subscribe")
        .expect("subscribe frame");
    let mut buf = Vec::new();
    write_frame(&mut buf, &DaemonToClient::Ack).unwrap();
    stream.write_all(&buf).await.unwrap();
    // One event, then simulate a daemon crash (drop the connection).
    let mut buf = Vec::new();
    write_frame(&mut buf, &DaemonToClient::EventNotification { event }).unwrap();
    stream.write_all(&buf).await.unwrap();
    stream.flush().await.unwrap();
    // Give the client a moment to read before the socket closes.
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(stream);
}

#[tokio::test]
async fn reconnects_after_daemon_restart_and_reports_state() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("smeltrd.sock");
    let listener = UnixListener::bind(&sock).unwrap();

    let (tx, mut rx) = mpsc::channel::<Event>(64);
    let (status_tx, mut status_rx) = watch::channel(ConnState::Reconnecting { attempt: 0 });
    let sock2 = sock.clone();
    let client = tokio::spawn(async move {
        let _ = subscribe_events_reconnecting(&sock2, "test", tx, status_tx).await;
    });

    // First connection lifecycle.
    serve_once(&listener, test_event(1)).await;
    let ev1 = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timely first event")
        .expect("first event");
    assert_eq!(ev1.seq, 1);

    // The client must notice the drop and report Reconnecting.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            status_rx.changed().await.unwrap();
            if matches!(*status_rx.borrow(), ConnState::Reconnecting { .. }) {
                break;
            }
        }
    })
    .await
    .expect("Reconnecting state within 5s");

    // Second lifecycle: the client reconnects on its own.
    serve_once(&listener, test_event(2)).await;
    let ev2 = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("timely second event")
        .expect("second event after reconnect");
    assert_eq!(ev2.seq, 2);
    assert!(matches!(*status_rx.borrow(), ConnState::Connected));

    // Dropping the receiver ends the loop.
    drop(rx);
    let _ = tokio::time::timeout(Duration::from_secs(10), client).await;
}
