//! Concurrent mappings share one real SCTP FILE channel; all targets are local fixtures.
use super::*;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use webrtc::api::APIBuilder;
use webrtc::peer_connection::RTCPeerConnection;

struct EchoService {
    address: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
}
impl EchoService {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.unwrap();
                        connections.spawn(Self::echo(stream));
                    }
                    Some(result) = connections.join_next(), if !connections.is_empty() => {
                        result.unwrap();
                    }
                }
            }
        });
        Self { address, task }
    }

    async fn echo(mut stream: TcpStream) {
        let _ = stream.set_nodelay(true);
        let mut header = [0; 4];
        if stream.read_exact(&mut header).await.is_err() {
            return;
        }
        let mut remaining = u32::from_be_bytes(header) as usize;
        // Both directions include the length prefix, so every forwarded byte is checked.
        if stream.write_all(&header).await.is_err() {
            return;
        }
        let mut buffer = [0; 4096];
        while remaining != 0 {
            let requested = remaining.min(buffer.len());
            match stream.read(&mut buffer[..requested]).await {
                Ok(0) | Err(_) => return, // An intentionally aborted flow.
                Ok(length) => {
                    if stream.write_all(&buffer[..length]).await.is_err() {
                        return;
                    }
                    remaining -= length;
                }
            }
        }
        let _ = stream.shutdown().await;
    }

    fn syn(&self) -> Vec<u8> {
        serde_json::json!({"version":1, "target_host":"127.0.0.1", "target_port":self.address.port()})
            .to_string().into_bytes()
    }
}
impl Drop for EchoService {
    fn drop(&mut self) {
        // Aborting the acceptor also drops its JoinSet and cancels every accepted target.
        self.task.abort();
    }
}

struct Harness {
    session: Arc<Session>,
    peers: Vec<RTCPeerConnection>,
    file: Arc<RTCDataChannel>,
    incoming: mpsc::UnboundedReceiver<Outbound>,
    router: Arc<tokio::sync::Mutex<webrtc::util::vnet::router::Router>>,
}
impl Harness {
    async fn connect() -> Self {
        let (left, right, router) = crate::test_network::virtual_network().await;
        let mut peers = Vec::new();
        for setting in [left, right] {
            peers.push(
                APIBuilder::new()
                    .with_setting_engine(setting)
                    .build()
                    .new_peer_connection(Default::default())
                    .await
                    .unwrap(),
            );
        }
        let slot = Arc::new(Mutex::new(None));
        let session = Session::new(slot.clone(), Arc::new(AtomicU64::new(1)));
        let weak = Arc::downgrade(&session);
        peers[0].on_data_channel(Box::new(move |dc| {
            let (slot, weak) = (slot.clone(), weak.clone());
            Box::pin(async move {
                assert_eq!(dc.label(), "FILE_DATA_CHANNEL");
                *slot.lock().unwrap() = Some(dc.clone());
                let session = weak.upgrade().unwrap();
                session.attach_channel(&dc).await;
                dc.on_message(Box::new(move |message| {
                    let weak = weak.clone();
                    Box::pin(async move {
                        if let Some(session) = weak.upgrade() {
                            session
                                .handle(&decode_wire(&message.data).unwrap(), "FILE_DATA_CHANNEL")
                                .await;
                        }
                    })
                }));
            })
        }));
        let file = peers[1]
            .create_data_channel("FILE_DATA_CHANNEL", None)
            .await
            .unwrap();
        let (tx, incoming) = mpsc::unbounded_channel();
        file.on_message(Box::new(move |message| {
            let envelope = decode_wire(&message.data).unwrap();
            let raw = envelope
                .iter()
                .find_map(|field| match (field.number, &field.value) {
                    (MESSAGE_FIELD, WireValue::Bytes(raw)) => Some(*raw),
                    _ => None,
                })
                .unwrap();
            let fields = decode_wire(raw).unwrap();
            let key = Key {
                session: Id::of(&fields, 1),
                rule: Id::of(&fields, 2),
                stream: Id::of(&fields, 3),
            };
            let kind = fields
                .iter()
                .find_map(|field| match (field.number, &field.value) {
                    (4, WireValue::Varint(kind)) => Some(*kind),
                    _ => None,
                })
                .unwrap_or(SYN);
            let payload = fields
                .iter()
                .find_map(|field| match (field.number, &field.value) {
                    (5, WireValue::Bytes(payload)) => Some(payload.to_vec()),
                    _ => None,
                })
                .unwrap_or_default();
            let _ = tx.send((key, kind, payload));
            Box::pin(async {})
        }));
        let mut gathered = peers[1].gathering_complete_promise().await;
        peers[1]
            .set_local_description(peers[1].create_offer(None).await.unwrap())
            .await
            .unwrap();
        gathered.recv().await;
        peers[0]
            .set_remote_description(peers[1].local_description().await.unwrap())
            .await
            .unwrap();
        let mut gathered = peers[0].gathering_complete_promise().await;
        peers[0]
            .set_local_description(peers[0].create_answer(None).await.unwrap())
            .await
            .unwrap();
        gathered.recv().await;
        peers[1]
            .set_remote_description(peers[0].local_description().await.unwrap())
            .await
            .unwrap();
        while file.ready_state() != RTCDataChannelState::Open {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Self {
            session,
            peers,
            file,
            incoming,
            router,
        }
    }

    async fn next(&mut self) -> Outbound {
        tokio::time::timeout(Duration::from_secs(5), self.incoming.recv())
            .await
            .expect("mapping response stalled")
            .expect("FILE receiver closed")
    }

    async fn open(&mut self, service: &EchoService, count: usize, discard_old: bool) {
        let syn = service.syn();
        for index in 0..count {
            send(&self.file, index, SYN, &syn).await;
        }
        let mut acknowledged = vec![false; count];
        let mut remaining = count;
        while remaining != 0 {
            let (key, kind, payload) = self.next().await;
            let index = index_of(&key, count);
            if kind != SYN_ACK {
                // Already-transmitted old DATA can precede the replacement's SYN_ACK.
                // Ordered FILE delivery makes the new ACK the observable generation boundary.
                assert!(
                    discard_old && !acknowledged[index],
                    "old frame crossed replacement ACK"
                );
                continue;
            }
            assert!(!acknowledged[index], "duplicate SYN_ACK");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&payload).unwrap()["ok"],
                true
            );
            acknowledged[index] = true;
            remaining -= 1;
        }
        assert_eq!(self.session.state.lock().unwrap().streams.len(), count);
    }

    async fn round_trip(&mut self, count: usize, body_size: usize) {
        let payloads: Vec<_> = (0..count).map(|index| content(index, body_size)).collect();
        let file = self.file.clone();
        let copies = payloads.clone();
        let mut sender = JoinSet::new();
        sender.spawn(async move {
            // Round-robin chunks require all streams to share the same real FILE transport.
            for offset in (0..body_size + 4).step_by(8192) {
                for (index, payload) in copies.iter().enumerate() {
                    let end = (offset + 8192).min(payload.len());
                    send(&file, index, DATA, &payload[offset..end]).await;
                }
            }
        });
        let mut offsets = vec![0; count];
        let mut finished = vec![false; count];
        let mut remaining = count;
        while remaining != 0 {
            let (key, kind, payload) = self.next().await;
            let index = index_of(&key, count);
            assert!(!finished[index], "frame arrived after FIN");
            match kind {
                DATA => {
                    let offset = offsets[index];
                    let end = offset + payload.len();
                    assert!(
                        end <= payloads[index].len(),
                        "echo exceeded expected length"
                    );
                    assert!(
                        payload == payloads[index][offset..end],
                        "echo bytes differed at local flow {index}, offset {offset}"
                    );
                    offsets[index] = end;
                }
                FIN => {
                    assert_eq!(
                        offsets[index],
                        payloads[index].len(),
                        "FIN preceded complete echo"
                    );
                    finished[index] = true;
                    remaining -= 1;
                }
                _ => panic!("unexpected mapping response type {kind}"),
            }
        }
        sender.join_next().await.unwrap().unwrap();
        while !self.session.state.lock().unwrap().streams.is_empty() {
            tokio::task::yield_now().await;
        }
    }

    async fn close(self) {
        self.session.shutdown();
        for peer in self.peers {
            peer.close().await.unwrap();
        }
        self.router.lock().await.stop().await.unwrap();
    }
}

fn key_at(index: usize) -> Key {
    Key {
        session: Id::Bytes(b"concurrent-fixture".to_vec()),
        rule: Id::Varint(1),
        stream: Id::Varint(index as u64),
    }
}
fn index_of(key: &Key, count: usize) -> usize {
    let Id::Varint(index) = key.stream else {
        panic!("unexpected stream identity type")
    };
    assert!(index < count as u64, "unexpected stream identity");
    assert_eq!(*key, key_at(index as usize));
    index as usize
}
fn content(index: usize, body_size: usize) -> Vec<u8> {
    let mut data = (body_size as u32).to_be_bytes().to_vec();
    data.extend((0..body_size).map(|offset| ((index * 17 + offset) % 251) as u8));
    data
}
async fn send(file: &RTCDataChannel, index: usize, kind: u64, payload: &[u8]) {
    let message =
        crate::initialization::envelope(1, 0, MESSAGE_FIELD, &frame(&key_at(index), kind, payload));
    file.send(&bytes::Bytes::from(message)).await.unwrap();
}

#[tokio::test]
async fn sixty_four_sctp_streams_preserve_bytes_after_abort_reuse_and_peer_reconnect() {
    let run = async {
        let service = EchoService::start().await;
        let mut peer = Harness::connect().await;
        peer.open(&service, 64, false).await;
        peer.round_trip(64, 128 * 1024).await;

        peer.open(&service, 8, false).await;
        let partial = content(0, 1024 * 1024);
        for index in 0..8 {
            send(&peer.file, index, DATA, &partial[..16 * 1024]).await;
        }
        let mut observed = [false; 8];
        while observed.iter().any(|seen| !seen) {
            let (key, kind, _) = peer.next().await;
            assert_eq!(kind, DATA);
            observed[index_of(&key, 8)] = true;
        }
        for index in 0..8 {
            send(&peer.file, index, FIN, &[]).await;
        }
        // Reuse immediately; do not wait for the old host task's destructor.
        peer.open(&service, 8, true).await;
        peer.round_trip(8, 63).await;
        peer.close().await;

        let mut replacement = Harness::connect().await;
        replacement.open(&service, 64, false).await;
        replacement.round_trip(64, 32 * 1024).await;
        replacement.close().await;
    };
    tokio::time::timeout(Duration::from_secs(60), run)
        .await
        .expect("64-stream SCTP integrity/recovery test exceeded its deadline");
}
