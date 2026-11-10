//! Loopback TCP lifecycle tests, independent of cloud accounts and timing of Internet routes.
use super::*;
use tokio::net::{TcpListener, TcpStream};

fn session(capacity: usize) -> (Arc<Session>, mpsc::Receiver<PendingFrame>) {
    let (outbound, queue) = mpsc::channel(capacity);
    (
        Arc::new(Session {
            outbound,
            state: Mutex::new(State::default()),
            writable: Arc::new(Notify::new()),
            buffer_limit: MAX_BUFFERED,
        }),
        queue,
    )
}
fn key() -> Key {
    Key {
        session: Id::Bytes(b"session".to_vec()),
        rule: Id::Varint(7),
        stream: Id::Varint(9),
    }
}
fn syn_payload(listener: &TcpListener) -> Vec<u8> {
    serde_json::json!({"target_host":"127.0.0.1", "target_port":listener.local_addr().unwrap().port()}).to_string().into_bytes()
}
async fn next(queue: &mut mpsc::Receiver<PendingFrame>) -> Outbound {
    tokio::time::timeout(Duration::from_secs(3), queue.recv())
        .await
        .unwrap()
        .unwrap()
        .frame
}
async fn finished(task: &tokio::task::AbortHandle) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("mapping task survived cancellation");
}
async fn open(
    session: &Arc<Session>,
    queue: &mut mpsc::Receiver<PendingFrame>,
) -> (TcpStream, tokio::task::AbortHandle) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    assert!(session.syn(key(), &syn_payload(&listener)).is_none());
    let task = session.state.lock().unwrap().streams[&key()].task.clone();
    let (target, _) = listener.accept().await.unwrap();
    let (_, kind, ack) = next(queue).await;
    assert_eq!(kind, SYN_ACK);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&ack).unwrap()["ok"],
        true
    );
    (target, task)
}
async fn receive(session: &Arc<Session>, kind: u64, payload: &[u8]) {
    let message =
        crate::initialization::envelope(1, 0, MESSAGE_FIELD, &frame(&key(), kind, payload));
    assert!(
        session
            .handle(&decode_wire(&message).unwrap(), "FILE_DATA_CHANNEL")
            .await
    );
}

#[tokio::test]
async fn tcp_round_trip_preserves_bytes_and_ack_data_fin_order() {
    let (session, mut queue) = session(2);
    let (mut target, task) = open(&session, &mut queue).await;
    let payload: Vec<u8> = (0..192 * 1024).map(|i| (i % 251) as u8).collect();
    receive(&session, DATA, &payload).await; // Includes a frame larger than 64 KiB.
    let mut received = vec![0; payload.len()];
    target.read_exact(&mut received).await.unwrap();
    assert_eq!(received, payload);
    let copy = payload.clone();
    let target = tokio::spawn(async move {
        target.write_all(&copy).await.unwrap();
        target.shutdown().await.unwrap();
    });
    let mut downloaded = Vec::new();
    loop {
        let (id, kind, bytes) = next(&mut queue).await;
        assert_eq!(id, key());
        if kind == FIN {
            assert!(bytes.is_empty());
            break;
        }
        assert_eq!(kind, DATA);
        downloaded.extend(bytes);
    }
    assert_eq!(downloaded, payload);
    target.await.unwrap();
    finished(&task).await;
    assert!(session.state.lock().unwrap().streams.is_empty());
}

#[tokio::test]
async fn fin_before_connect_and_shutdown_prevent_late_registration() {
    let (session, _queue) = session(1);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let syn = syn_payload(&listener);
    session.syn(key(), &syn);
    let task = session.state.lock().unwrap().streams[&key()].task.clone();
    // No await between SYN and FIN: cancel before the scheduled connect can run.
    session.close(&key(), false);
    finished(&task).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
    session.shutdown();
    session.shutdown();
    session.syn(key(), &syn);
    assert!(session.state.lock().unwrap().streams.is_empty());
}

#[tokio::test]
async fn duplicate_syn_keeps_one_target_and_old_cleanup_cannot_remove_reused_key() {
    let (session, mut queue) = session(4);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let syn = syn_payload(&listener);
    session.syn(key(), &syn);
    session.syn(key(), &syn); // Pending duplicate.
    let (mut first, _) = listener.accept().await.unwrap();
    assert_eq!(next(&mut queue).await.1, SYN_ACK);
    session.syn(key(), &syn); // Open duplicate.
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
    let old = session.state.lock().unwrap().streams[&key()].task.clone();
    session.close(&key(), false);
    session.syn(key(), &syn); // Register replacement before the old task runs its destructor.
    let (mut second, _) = listener.accept().await.unwrap();
    assert_eq!(next(&mut queue).await.1, SYN_ACK);
    finished(&old).await;
    assert_eq!(session.state.lock().unwrap().streams.len(), 1);
    assert_eq!(first.read(&mut [0]).await.unwrap(), 0);
    receive(&session, DATA, b"replacement").await;
    let mut bytes = [0; 11];
    second.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"replacement");
    session.shutdown();
}

#[tokio::test]
async fn shutdown_cancels_ack_blocked_on_full_outbound_queue() {
    let (session, _queue) = session(1);
    session
        .outbound
        .try_send((key(), DATA_ACK, vec![]).into())
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    session.syn(key(), &syn_payload(&listener));
    let task = session.state.lock().unwrap().streams[&key()].task.clone();
    let (mut target, _) = listener.accept().await.unwrap();
    session.shutdown();
    finished(&task).await;
    assert_eq!(target.read(&mut [0]).await.unwrap(), 0);
    assert!(session.state.lock().unwrap().streams.is_empty());
}

#[tokio::test]
async fn full_writer_queue_closes_target_and_fin_waits_for_outbound_capacity() {
    let (session, mut queue) = session(1);
    let (mut target, task) = open(&session, &mut queue).await;
    session
        .outbound
        .try_send((key(), DATA_ACK, vec![]).into())
        .unwrap();
    // The current-thread runtime cannot drain writes during these immediate handlers.
    for _ in 0..=WRITE_QUEUE_FRAMES {
        receive(&session, DATA, &[42]).await;
    }
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), target.read(&mut [0]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(!task.is_finished()); // FIN is tracked while waiting for queue capacity.
    assert_eq!(next(&mut queue).await.1, DATA_ACK);
    assert_eq!(next(&mut queue).await.1, FIN);
    finished(&task).await;
}

#[tokio::test]
async fn remote_fin_cancels_local_fin_waiting_on_full_queue() {
    let (session, mut queue) = session(1);
    let (mut target, task) = open(&session, &mut queue).await;
    session
        .outbound
        .try_send((key(), DATA_ACK, vec![]).into())
        .unwrap();
    session.close(&key(), true);
    assert_eq!(target.read(&mut [0]).await.unwrap(), 0);
    session.close(&key(), false);
    finished(&task).await;
    assert_eq!(next(&mut queue).await.1, DATA_ACK);
    assert!(queue.try_recv().is_err());
}

#[tokio::test]
async fn dropping_session_cancels_blocked_target_write_and_read() {
    let (session, mut queue) = session(4);
    let (mut target, task) = open(&session, &mut queue).await;
    // A target which does not read eventually blocks write_all, independent of read().
    let writer = session.state.lock().unwrap().streams[&key()].writer.clone();
    for _ in 0..128 {
        writer.try_send(vec![7; 128 * 1024]).unwrap();
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(writer.capacity() < WRITE_QUEUE_FRAMES);
    assert!(!task.is_finished());
    let weak = Arc::downgrade(&session);
    drop(session);
    assert!(weak.upgrade().is_none());
    finished(&task).await;
    // Drain only bytes already accepted by TCP; cancellation must then close both halves.
    tokio::time::timeout(
        Duration::from_secs(3),
        tokio::io::copy(&mut target, &mut tokio::io::sink()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(writer.is_closed());
}

#[tokio::test]
async fn thousand_reconnects_release_tasks_and_stream_registrations() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (session, mut queue) = session(4);
        // Warm the runtime's TCP reactor before measuring descriptors in an isolated run.
        let (target, warm) = open(&session, &mut queue).await;
        session.close(&key(), false);
        finished(&warm).await;
        drop(target);
        #[cfg(target_os = "linux")]
        let before = std::env::var_os("UU_RECONNECT_RESOURCE_CHECK").map(|_| {
            (
                std::fs::read_dir("/proc/self/fd").unwrap().count(),
                std::fs::read_dir("/proc/self/task").unwrap().count(),
            )
        });
        for _ in 0..1000 {
            let (mut target, task) = open(&session, &mut queue).await;
            receive(&session, DATA, b"ping").await;
            let mut payload = [0; 4];
            target.read_exact(&mut payload).await.unwrap();
            assert_eq!(&payload, b"ping");
            session.close(&key(), false);
            finished(&task).await;
            assert_eq!(target.read(&mut [0]).await.unwrap(), 0);
            assert!(session.state.lock().unwrap().streams.is_empty());
        }
        #[cfg(target_os = "linux")]
        if let Some(before) = before {
            let after = (
                std::fs::read_dir("/proc/self/fd").unwrap().count(),
                std::fs::read_dir("/proc/self/task").unwrap().count(),
            );
            eprintln!("1000 reconnects: (FDs, threads) before={before:?} after={after:?}");
            assert_eq!(after, before);
        }
    })
    .await
    .expect("reconnect endurance timed out");
}

#[tokio::test(start_paused = true)]
async fn unavailable_channel_times_out_and_shutdown_aborts_outbound_worker() {
    let slot = Arc::new(Mutex::new(None));
    let session = Session::new(slot, Arc::new(AtomicU64::new(1)));
    let task = session.state.lock().unwrap().outbound_task.clone().unwrap();
    session
        .outbound
        .send((key(), FIN, vec![]).into())
        .await
        .unwrap();
    tokio::task::yield_now().await;
    tokio::time::advance(SEND_TIMEOUT + Duration::from_secs(1)).await;
    finished(&task).await;
    assert!(session.state.lock().unwrap().closed);
    assert!(session.outbound.is_closed());
}

#[tokio::test(start_paused = true)]
async fn pending_connect_is_cancelled_and_timeout_reports_failure_once() {
    struct Dropped(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    for remote_fin in [true, false] {
        let (session, mut queue) = session(2);
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (entered, ready) = oneshot::channel();
        let guard = Dropped(dropped.clone());
        session.syn_with(
            key(),
            br#"{"target_host":"127.0.0.1","target_port":22}"#,
            move |_, _| async move {
                let _guard = guard;
                let _ = entered.send(());
                std::future::pending().await
            },
        );
        ready.await.unwrap(); // The connect future has actually started, not merely been queued.
        let task = session.state.lock().unwrap().streams[&key()].task.clone();
        if remote_fin {
            session.close(&key(), false);
        } else {
            session.shutdown();
        }
        finished(&task).await;
        assert!(dropped.load(Ordering::SeqCst));
        assert!(queue.try_recv().is_err());
        assert!(session.state.lock().unwrap().streams.is_empty());
    }
    let (session, mut queue) = session(2);
    session.syn_with(
        key(),
        br#"{"target_host":"127.0.0.1","target_port":22}"#,
        |_, _| std::future::pending(),
    );
    let task = session.state.lock().unwrap().streams[&key()].task.clone();
    tokio::task::yield_now().await;
    tokio::time::advance(CONNECT_TIMEOUT).await;
    let (_, kind, payload) = next(&mut queue).await;
    assert_eq!(kind, SYN_ACK);
    let ack: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(ack["ok"], false);
    assert_eq!(ack["error"], "connect timeout");
    finished(&task).await;
    assert!(queue.try_recv().is_err());
}

#[tokio::test]
async fn remote_fin_invalidates_queued_frames_before_identity_reuse() {
    let (session, mut queue) = session(4);
    let (mut target, task) = open(&session, &mut queue).await;
    target.write_all(b"old stream bytes").await.unwrap();
    let pending = tokio::time::timeout(Duration::from_secs(3), queue.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.frame.1, DATA);
    let cancelled = pending.cancelled.unwrap();
    assert!(!cancelled.load(Ordering::Acquire));
    session.close(&key(), false);
    let (_replacement, replacement) = open(&session, &mut queue).await;
    finished(&task).await;
    assert!(cancelled.load(Ordering::Acquire));
    assert!(!session.state.lock().unwrap().streams[&key()]
        .cancelled
        .load(Ordering::Acquire));
    session.shutdown();
    finished(&replacement).await;
}

#[tokio::test]
async fn completed_stream_frames_are_cancelled_by_fin_or_same_key_syn() {
    // Leave completed generations in the outbound queue, exactly as under SCTP backpressure.
    for refused in [false, true] {
        for remote_fin in [false, true] {
            let (session, mut queue) = session(4);
            let expected = if refused {
                session.syn_with(
                    key(),
                    br#"{"target_host":"127.0.0.1","target_port":22}"#,
                    |_, _| async { Err(std::io::ErrorKind::ConnectionRefused.into()) },
                );
                let task = session.state.lock().unwrap().streams[&key()].task.clone();
                finished(&task).await;
                vec![SYN_ACK]
            } else {
                let (mut target, task) = open(&session, &mut queue).await;
                target.write_all(b"finished stream bytes").await.unwrap();
                target.shutdown().await.unwrap();
                finished(&task).await;
                vec![DATA, FIN]
            };
            assert!(session.state.lock().unwrap().streams.is_empty());
            let retired = session.state.lock().unwrap().retired[&key()].clone();
            assert!(!retired.upgrade().unwrap().load(Ordering::Acquire));
            if remote_fin {
                receive(&session, FIN, &[]).await;
                assert!(retired.upgrade().unwrap().load(Ordering::Acquire));
            }
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            session.syn(key(), &syn_payload(&listener));
            let replacement = session.state.lock().unwrap().streams[&key()].task.clone();
            let (mut target, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
                .await
                .unwrap()
                .unwrap();
            assert!(retired.upgrade().unwrap().load(Ordering::Acquire));
            for kind in expected {
                let pending = queue.recv().await.unwrap();
                assert_eq!(pending.frame.1, kind);
                assert!(pending.cancelled.unwrap().load(Ordering::Acquire));
            }
            let pending = queue.recv().await.unwrap();
            assert_eq!(pending.frame.1, SYN_ACK);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&pending.frame.2).unwrap()["ok"],
                true
            );
            assert!(!pending.cancelled.unwrap().load(Ordering::Acquire));
            assert!(retired.upgrade().is_none());
            receive(&session, DATA, b"replacement").await;
            let mut bytes = [0; 11];
            tokio::time::timeout(Duration::from_secs(3), target.read_exact(&mut bytes))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&bytes, b"replacement");
            session.shutdown();
            finished(&replacement).await;
        }
    }
}

#[tokio::test]
async fn completed_stream_tokens_do_not_accumulate_for_distinct_identities() {
    let (session, mut queue) = session(1);
    for stream_id in 0..1000 {
        let mut id = key();
        id.stream = Id::Varint(stream_id);
        session.syn_with(
            id.clone(),
            br#"{"target_host":"127.0.0.1","target_port":22}"#,
            |_, _| async { Err(std::io::ErrorKind::ConnectionRefused.into()) },
        );
        let task = session.state.lock().unwrap().streams[&id].task.clone();
        assert_eq!(next(&mut queue).await.1, SYN_ACK);
        finished(&task).await;
        let state = session.state.lock().unwrap();
        assert!(state.streams.is_empty());
        assert!(state.retired.len() <= 1);
        assert!(state
            .retired
            .values()
            .all(|token| token.upgrade().is_none()));
    }
    session.shutdown();
    assert!(session.state.lock().unwrap().retired.is_empty());
}

// Exercise the actual vendored SCTP buffered-amount callback with packet loss and recovery.
#[tokio::test]
async fn sctp_backpressure_resumes_on_low_water_and_shutdown_does_not_wait_for_drain() {
    use webrtc::api::APIBuilder;
    use webrtc::peer_connection::{configuration::RTCConfiguration, RTCPeerConnection};
    let run = async {
        let (left, right, router) = crate::test_network::virtual_network().await;
        let mut peers = Vec::<Arc<RTCPeerConnection>>::new();
        for setting in [left, right] {
            peers.push(Arc::new(
                APIBuilder::new()
                    .with_setting_engine(setting)
                    .build()
                    .new_peer_connection(RTCConfiguration::default())
                    .await
                    .unwrap(),
            ));
        }
        let (tx, mut received) = mpsc::unbounded_channel();
        peers[1].on_data_channel(Box::new(move |dc| {
            let tx = tx.clone();
            Box::pin(async move {
                dc.on_message(Box::new(move |message| {
                    let _ = tx.send(message.data);
                    Box::pin(async {})
                }));
            })
        }));
        let dc = peers[0]
            .create_data_channel("FILE_DATA_CHANNEL", None)
            .await
            .unwrap();
        // SCTP's own pending queue blocks send() at 128 KiB before the production
        // 4 MiB guard is reached. Lower only this test's limit to exercise the callback.
        let limit = 32 * 1024;
        let session = Session::with_buffer_limit(
            Arc::new(Mutex::new(Some(dc.clone()))),
            Arc::new(AtomicU64::new(1)),
            limit,
        );
        session.attach_channel(&dc).await;
        let mut gathered = peers[0].gathering_complete_promise().await;
        peers[0]
            .set_local_description(peers[0].create_offer(None).await.unwrap())
            .await
            .unwrap();
        gathered.recv().await;
        peers[1]
            .set_remote_description(peers[0].local_description().await.unwrap())
            .await
            .unwrap();
        let mut gathered = peers[1].gathering_complete_promise().await;
        peers[1]
            .set_local_description(peers[1].create_answer(None).await.unwrap())
            .await
            .unwrap();
        gathered.recv().await;
        peers[0]
            .set_remote_description(peers[1].local_description().await.unwrap())
            .await
            .unwrap();
        while dc.ready_state() != RTCDataChannelState::Open {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let drop_packets = Arc::new(AtomicBool::new(true));
        let drop_filter = drop_packets.clone();
        router
            .lock()
            .await
            .add_chunk_filter(Box::new(move |_| !drop_filter.load(Ordering::Acquire)))
            .await;
        let enqueue = session.outbound.clone();
        let producer = tokio::spawn(async move {
            for i in 0..384 {
                enqueue
                    .send((key(), DATA, vec![(i % 251) as u8; READ_CHUNK]).into())
                    .await
                    .unwrap();
            }
        });
        while dc.buffered_amount().await <= limit {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!producer.is_finished());
        drop_packets.store(false, Ordering::Release);
        for i in 0..384 {
            let message = received.recv().await.unwrap();
            let fields = decode_wire(&message).unwrap();
            let raw = fields
                .iter()
                .find_map(|f| match (f.number, &f.value) {
                    (MESSAGE_FIELD, WireValue::Bytes(b)) => Some(*b),
                    _ => None,
                })
                .unwrap();
            let fields = decode_wire(raw).unwrap();
            let data = fields
                .iter()
                .find_map(|f| match (f.number, &f.value) {
                    (5, WireValue::Bytes(b)) => Some(*b),
                    _ => None,
                })
                .unwrap();
            assert_eq!(data, vec![(i % 251) as u8; READ_CHUNK]);
        }
        producer.await.unwrap();
        // Fill SCTP again, then close without allowing buffered_amount to drain to zero.
        drop_packets.store(true, Ordering::Release);
        let enqueue = session.outbound.clone();
        let producer = tokio::spawn(async move {
            for _ in 0..384 {
                if enqueue
                    .send((key(), DATA, vec![7; READ_CHUNK]).into())
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        while dc.buffered_amount().await <= limit {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let worker = session.state.lock().unwrap().outbound_task.clone().unwrap();
        session.shutdown();
        finished(&worker).await;
        producer.await.unwrap();
        assert!(dc.buffered_amount().await > 0);
        // With the production limit, SCTP's smaller pending queue can block send()
        // itself. Cancellation must release that future too, not only our low-water wait.
        let session = Session::new(
            Arc::new(Mutex::new(Some(dc.clone()))),
            Arc::new(AtomicU64::new(1000)),
        );
        session.attach_channel(&dc).await;
        let enqueue = session.outbound.clone();
        let producer = tokio::spawn(async move {
            for _ in 0..384 {
                if enqueue
                    .send((key(), DATA, vec![9; READ_CHUNK]).into())
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        while session.outbound.capacity() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let worker = session.state.lock().unwrap().outbound_task.clone().unwrap();
        assert!(!producer.is_finished());
        session.shutdown();
        finished(&worker).await;
        producer.await.unwrap();
        assert!(dc.buffered_amount().await > 0);
        for peer in peers {
            peer.close().await.unwrap();
        }
        router.lock().await.stop().await.unwrap();
    };
    tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .expect("SCTP backpressure recovery timed out");
}
