// Compile the production vendored timer directly, without exposing an internal API.
#[allow(dead_code)]
#[path = "../vendor/webrtc-sctp/src/timer/ack_timer.rs"]
mod ack_timer;

// Offline regressions: Tokio fake clock and an observer mutex; no sockets.
use ack_timer::{AckTimer, AckTimerObserver};
use async_trait::async_trait;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::Mutex;
use tokio::time::{advance, Duration};

struct Observer(Arc<AtomicUsize>);
#[async_trait]
impl AckTimerObserver for Observer {
    async fn on_ack_timeout(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

async fn scheduled() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn natural_expiry_then_two_restarts_each_deliver_one_callback() {
    let callbacks = Arc::new(AtomicUsize::new(0));
    let observer = Arc::new(Mutex::new(Observer(callbacks.clone())));
    let interval = Duration::from_millis(200);
    let mut timer = AckTimer::new(Arc::downgrade(&observer), interval);
    for generation in 0..3 {
        assert!(
            timer.start(),
            "naturally expired timer must restart: generation={generation}"
        );
        scheduled().await;
        advance(interval).await;
        scheduled().await;
        assert_eq!(callbacks.load(Ordering::SeqCst), generation + 1);
    }
}

#[tokio::test(start_paused = true)]
async fn stop_after_expiry_while_observer_locked_prevents_late_callback() {
    let callbacks = Arc::new(AtomicUsize::new(0));
    let observer = Arc::new(Mutex::new(Observer(callbacks.clone())));
    let held = observer.lock().await;
    let interval = Duration::from_millis(200);
    let mut timer = AckTimer::new(Arc::downgrade(&observer), interval);
    assert!(timer.start());
    scheduled().await;
    advance(interval).await;
    scheduled().await; // The expired task is waiting to acquire the observer.
    assert_eq!(callbacks.load(Ordering::SeqCst), 0);
    timer.stop();
    drop(held);
    scheduled().await;
    assert_eq!(
        callbacks.load(Ordering::SeqCst),
        0,
        "stop must suppress a stale waiting callback"
    );
}

#[tokio::test(start_paused = true)]
async fn restart_while_old_expiry_waits_only_fires_new_generation() {
    let callbacks = Arc::new(AtomicUsize::new(0));
    let observer = Arc::new(Mutex::new(Observer(callbacks.clone())));
    let held = observer.lock().await;
    let interval = Duration::from_millis(200);
    let mut timer = AckTimer::new(Arc::downgrade(&observer), interval);
    assert!(timer.start());
    scheduled().await;
    advance(interval).await;
    scheduled().await;
    timer.stop();
    assert!(timer.start());
    scheduled().await; // Register the replacement deadline while the observer is still held.
    drop(held);
    scheduled().await;
    assert_eq!(
        callbacks.load(Ordering::SeqCst),
        0,
        "old generation fired before new deadline"
    );
    advance(interval).await;
    scheduled().await;
    assert_eq!(callbacks.load(Ordering::SeqCst), 1);
    assert!(
        timer.start(),
        "old expiry must not stop or retain the replacement generation"
    );
    scheduled().await;
    advance(interval).await;
    scheduled().await;
    assert_eq!(callbacks.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn repeated_natural_expiry_rearms_without_stop() {
    let callbacks = Arc::new(AtomicUsize::new(0));
    let observer = Arc::new(Mutex::new(Observer(callbacks.clone())));
    let interval = Duration::from_millis(200);
    let mut timer = AckTimer::new(Arc::downgrade(&observer), interval);
    for generation in 0..100 {
        assert!(timer.start());
        assert!(timer.is_running());
        scheduled().await;
        advance(interval).await;
        scheduled().await;
        assert!(!timer.is_running());
        assert_eq!(callbacks.load(Ordering::SeqCst), generation + 1);
    }
}

#[tokio::test(start_paused = true)]
async fn normal_start_stop_and_repeated_start_keep_one_deadline() {
    let callbacks = Arc::new(AtomicUsize::new(0));
    let observer = Arc::new(Mutex::new(Observer(callbacks.clone())));
    let interval = Duration::from_millis(200);
    let mut timer = AckTimer::new(Arc::downgrade(&observer), interval);
    assert!(timer.start());
    assert!(!timer.start());
    scheduled().await;
    timer.stop();
    assert!(!timer.is_running());
    advance(interval).await;
    scheduled().await;
    assert_eq!(callbacks.load(Ordering::SeqCst), 0);
    assert!(timer.start());
    scheduled().await;
    advance(interval).await;
    scheduled().await;
    assert_eq!(callbacks.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn owner_drop_cancels_sleeping_and_expired_waiting_generations() {
    for expired in [false, true] {
        let callbacks = Arc::new(AtomicUsize::new(0));
        let observer = Arc::new(Mutex::new(Observer(callbacks.clone())));
        let held = observer.lock().await;
        let interval = Duration::from_millis(200);
        let mut timer = AckTimer::new(Arc::downgrade(&observer), interval);
        assert!(timer.start());
        scheduled().await;
        if expired {
            advance(interval).await;
            scheduled().await;
        }
        drop(timer);
        drop(held);
        advance(interval).await;
        scheduled().await;
        assert_eq!(callbacks.load(Ordering::SeqCst), 0);
        assert_eq!(
            Arc::strong_count(&observer),
            1,
            "timer task retained its observer"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn finishing_old_callback_does_not_close_replacement_generation() {
    struct WaitingObserver {
        entered: tokio::sync::mpsc::UnboundedSender<()>,
        release: Arc<tokio::sync::Notify>,
        calls: usize,
    }
    #[async_trait]
    impl AckTimerObserver for WaitingObserver {
        async fn on_ack_timeout(&mut self) {
            self.calls += 1;
            self.entered.send(()).unwrap();
            if self.calls == 1 {
                self.release.notified().await;
            }
        }
    }
    let (entered, mut events) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let observer = Arc::new(Mutex::new(WaitingObserver {
        entered,
        release: release.clone(),
        calls: 0,
    }));
    let interval = Duration::from_millis(200);
    let mut timer = AckTimer::new(Arc::downgrade(&observer), interval);
    assert!(timer.start());
    scheduled().await;
    advance(interval).await;
    scheduled().await;
    assert!(events.try_recv().is_ok());
    // Cancellation cannot undo a callback already entered. Its later completion must
    // nevertheless close only its own channel, not the replacement's active channel.
    timer.stop();
    assert!(timer.start());
    scheduled().await;
    release.notify_one();
    scheduled().await;
    assert!(timer.is_running());
    assert!(events.try_recv().is_err());
    advance(interval).await;
    scheduled().await;
    assert!(events.try_recv().is_ok());
    assert!(!timer.is_running());
}

// Keep the fake clock fixed while real spawn_blocking packet marshalling completes.
// The wall-clock deadline only guards a broken test/runtime; it is not an ACK threshold.
async fn without_advancing_clock<F: std::future::Future>(
    stage: &'static str,
    future: F,
) -> F::Output {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    tokio::pin!(future);
    loop {
        tokio::select! {
            biased;
            result = &mut future => return result,
            _ = tokio::task::yield_now() => {}
        }
        assert!(
            std::time::Instant::now() < deadline,
            "SCTP did not progress at the fixed clock time: stage={stage}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn sparse_sctp_data_repeatedly_drains_on_delayed_ack_before_retransmission() {
    use webrtc::sctp::association::{Association, Config};
    use webrtc::sctp::chunk::chunk_payload_data::PayloadProtocolIdentifier;
    use webrtc::util::conn::conn_pipe::pipe;

    // Real SCTP association/chunk/ACK processing over an in-memory packet pipe.
    // Normal ACK policy and production RTO remain unchanged; no sockets are opened.
    let (left, right) = pipe();
    let (client, server) = without_advancing_clock("handshake", async {
        tokio::join!(
            Association::client(Config {
                net_conn: Arc::new(left),
                max_receive_buffer_size: 0,
                max_message_size: 0,
                name: "sparse-client".into(),
            }),
            Association::server(Config {
                net_conn: Arc::new(right),
                max_receive_buffer_size: 0,
                max_message_size: 0,
                name: "sparse-server".into(),
            }),
        )
    })
    .await;
    let (client, server) = (client.unwrap(), server.unwrap());
    let outgoing = without_advancing_clock(
        "open_stream",
        client.open_stream(0, PayloadProtocolIdentifier::Binary),
    )
    .await
    .unwrap();
    let client_before = client.bytes_sent();
    let server_before = server.bytes_sent();
    let started = tokio::time::Instant::now();
    let mut incoming = None;
    for round in 0..3 {
        let expected = bytes::Bytes::from(vec![round as u8]);
        without_advancing_clock("write", outgoing.write(&expected))
            .await
            .unwrap();
        if incoming.is_none() {
            incoming = Some(
                without_advancing_clock("accept_stream", server.accept_stream())
                    .await
                    .unwrap(),
            );
        }
        let mut payload = [0; 1];
        let received =
            without_advancing_clock("read", incoming.as_ref().unwrap().read(&mut payload))
                .await
                .unwrap();
        assert_eq!(received, 1);
        assert!(payload[0] == round as u8, "sparse DATA bytes changed");
        scheduled().await;
        assert_eq!(
            outgoing.buffered_amount(),
            1,
            "Normal ACK unexpectedly became immediate"
        );

        // Advance only the 200 ms delayed-ACK interval. All three rounds together
        // remain below the production minimum T3 RTO (1 second).
        advance(Duration::from_millis(200)).await;
        eprintln!(
            "sparse ACK round={} phase=waiting buffered={} virtual_ms={}",
            round + 1,
            outgoing.buffered_amount(),
            (tokio::time::Instant::now() - started).as_millis(),
        );
        without_advancing_clock("delayed_ack", async {
            while outgoing.buffered_amount() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert_eq!(
            tokio::time::Instant::now() - started,
            Duration::from_millis(200 * (round + 1) as u64)
        );
        // No other traffic exists after the handshake: one 32-byte DATA packet and
        // one 28-byte SACK per round. Extra DATA would expose a retransmission.
        assert_eq!(client.bytes_sent() - client_before, 32 * (round + 1));
        assert_eq!(server.bytes_sent() - server_before, 28 * (round + 1));
        eprintln!(
            "sparse ACK round={} phase=complete buffered={} data_packet_bytes={} sack_packet_bytes={} virtual_ms={}",
            round + 1,
            outgoing.buffered_amount(),
            client.bytes_sent() - client_before,
            server.bytes_sent() - server_before,
            (tokio::time::Instant::now() - started).as_millis(),
        );
    }
    without_advancing_clock("close", async {
        let (client, server) = tokio::join!(client.close(), server.close());
        client.unwrap();
        server.unwrap();
    })
    .await;
}
