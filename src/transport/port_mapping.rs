//! UU 端口映射 (port mapping) — the controlled ("B") side.
//!
//! The viewer owns the rules (local port → target_host:target_port reachable from this host).
//! For every local TCP connection it opens a stream with Message field 27 `PortMappingFrame
//! {1 session_id, 2 rule_id, 3 stream_id, 4 type, 5 payload}` on FILE_DATA_CHANNEL (the official
//! host rejects frames from other channels). Frame types: SYN=0, FIN=1, DATA=2, SYN_ACK=3,
//! DATA_ACK=4. SYN payload is JSON `{"version", "target_host", "target_port"}`; the host
//! connects and answers SYN_ACK `{"version", "ok", "error"}`. DATA carries raw bytes both ways,
//! FIN closes a stream, DATA_ACK is ignored by both sides (official log strings). Mirrors
//! UURemoteServer `PortMappingHandler` (handleSyn / handleTargetState / flushTargetWrites /
//! closeStream). Payload bytes are never logged. `UU_PORT_MAPPING=0` disables it, like the
//! official controlled-side switch.

use crate::protocol::{decode_wire, WireField, WireValue};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Notify};
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::data_channel::RTCDataChannel;

pub const MESSAGE_FIELD: u32 = 27;
const SYN: u64 = 0;
const FIN: u64 = 1;
const DATA: u64 = 2;
const SYN_ACK: u64 = 3;
const DATA_ACK: u64 = 4;
const PROTOCOL_VERSION: u64 = 1;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_CHUNK: usize = 16 * 1024;
/// Pending writes to one target before the stream is closed ("写缓冲超限").
const WRITE_QUEUE_FRAMES: usize = 256;
/// Pause reading targets while this much is queued on the data channel.
const MAX_BUFFERED: usize = 4 * 1024 * 1024;
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROL_QUEUE_FRAMES: usize = 64;
type Outbound = (Key, u64, Vec<u8>);
static DIAGNOSTIC_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Process-local counters only: no wire identity, target address or payload is retained.
#[derive(Debug)]
struct FlowDiagnostics {
    generation: u64,
    started: Instant,
    identity_reuse: bool,
    duplicate_syn: AtomicBool,
    reason: Mutex<&'static str>,
    rx_data_frames: AtomicU64,
    rx_data_bytes: AtomicU64,
    target_read: AtomicU64,
    target_written: AtomicU64,
    queued_frames: AtomicU64,
    queued_data: AtomicU64,
    sent_frames: AtomicU64,
    sent_data: AtomicU64,
    cancelled_data: AtomicU64,
}
impl FlowDiagnostics {
    fn new(identity_reuse: bool) -> Arc<Self> {
        Arc::new(Self {
            generation: DIAGNOSTIC_GENERATION.fetch_add(1, Ordering::Relaxed),
            started: Instant::now(),
            identity_reuse,
            duplicate_syn: AtomicBool::new(false),
            reason: Mutex::new("active"),
            rx_data_frames: AtomicU64::new(0),
            rx_data_bytes: AtomicU64::new(0),
            target_read: AtomicU64::new(0),
            target_written: AtomicU64::new(0),
            queued_frames: AtomicU64::new(0),
            queued_data: AtomicU64::new(0),
            sent_frames: AtomicU64::new(0),
            sent_data: AtomicU64::new(0),
            cancelled_data: AtomicU64::new(0),
        })
    }
    fn end(&self, reason: &'static str, failed: bool) {
        let mut current = self.reason.lock().unwrap();
        let changed = *current != reason;
        if *current == "active" {
            *current = reason;
        }
        if failed && changed {
            log::warn!(
                "Port mapping stream failed: generation={} reason={}",
                self.generation,
                reason
            );
        }
    }
    fn report(&self, stage: &'static str) {
        log::debug!("Port mapping flow: generation={} stage={} age_ms={} reason={} duplicate_syn={} identity_reuse={} rx_data_frames={} rx_data_bytes={} target_read={} target_written={} queued_frames={} queued_data={} dc_sent_frames={} dc_sent_data={} cancelled_data={}",
            self.generation, stage, self.started.elapsed().as_millis(), *self.reason.lock().unwrap(),
            self.duplicate_syn.load(Ordering::Relaxed), self.identity_reuse,
            self.rx_data_frames.load(Ordering::Relaxed), self.rx_data_bytes.load(Ordering::Relaxed),
            self.target_read.load(Ordering::Relaxed), self.target_written.load(Ordering::Relaxed),
            self.queued_frames.load(Ordering::Relaxed), self.queued_data.load(Ordering::Relaxed),
            self.sent_frames.load(Ordering::Relaxed), self.sent_data.load(Ordering::Relaxed),
            self.cancelled_data.load(Ordering::Relaxed));
    }
    fn queued(&self, kind: u64, length: usize) {
        self.queued_frames.fetch_add(1, Ordering::Relaxed);
        if kind == DATA {
            self.queued_data.fetch_add(length as u64, Ordering::Relaxed);
        }
    }
}
impl Drop for FlowDiagnostics {
    fn drop(&mut self) {
        // Pending frames retain these counters after the target task exits.
        self.report("released");
    }
}

#[derive(Debug, Default)]
struct StreamToken {
    cancelled: AtomicBool,
    fin_pending: AtomicBool,
}
impl std::ops::Deref for StreamToken {
    type Target = AtomicBool;
    fn deref(&self) -> &Self::Target {
        &self.cancelled
    }
}

#[derive(Debug)]
struct PendingFrame {
    frame: Outbound,
    cancelled: Option<Arc<StreamToken>>,
    diagnostics: Option<Arc<FlowDiagnostics>>,
}
// Only test queue fillers may omit cancellation; every production response is bound to a token.
#[cfg(test)]
impl From<Outbound> for PendingFrame {
    fn from(frame: Outbound) -> Self {
        Self {
            frame,
            cancelled: None,
            diagnostics: None,
        }
    }
}
impl PendingFrame {
    fn stream(key: &Key, kind: u64, payload: Vec<u8>, cancelled: &Arc<StreamToken>) -> Self {
        if kind == FIN {
            cancelled.fin_pending.store(true, Ordering::Release);
        }
        Self {
            frame: (key.clone(), kind, payload),
            cancelled: Some(cancelled.clone()),
            diagnostics: None,
        }
    }
    fn tracked(mut self, diagnostics: &Arc<FlowDiagnostics>) -> Self {
        self.diagnostics = Some(diagnostics.clone());
        self
    }
}

pub fn enabled() -> bool {
    std::env::var("UU_PORT_MAPPING").map_or(true, |v| v != "0")
}

/// A frame identity field echoed back exactly as received (varint or bytes).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Id {
    Varint(u64),
    Bytes(Vec<u8>),
    Missing,
}

impl Id {
    fn of(fields: &[WireField], number: u32) -> Self {
        match fields.iter().find(|f| f.number == number).map(|f| &f.value) {
            Some(WireValue::Varint(v)) => Id::Varint(*v),
            Some(WireValue::Bytes(b)) => Id::Bytes(b.to_vec()),
            _ => Id::Missing,
        }
    }
    fn encode(&self, number: u32, out: &mut Vec<u8>) {
        match self {
            Id::Varint(v) => {
                varint(u64::from(number) << 3, out);
                varint(*v, out);
            }
            Id::Bytes(b) => bytes(number, b, out),
            Id::Missing => {}
        }
    }
}

fn varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 128 {
        out.push((v as u8 & 127) | 128);
        v >>= 7;
    }
    out.push(v as u8);
}
fn bytes(number: u32, v: &[u8], out: &mut Vec<u8>) {
    varint(u64::from(number) << 3 | 2, out);
    varint(v.len() as u64, out);
    out.extend_from_slice(v);
}

/// One stream: (session_id, rule_id, stream_id) as sent by the viewer.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    session: Id,
    rule: Id,
    stream: Id,
}

fn frame(key: &Key, kind: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    key.session.encode(1, &mut out);
    key.rule.encode(2, &mut out);
    key.stream.encode(3, &mut out);
    if kind != 0 {
        varint(4 << 3, &mut out);
        varint(kind, &mut out);
    }
    if !payload.is_empty() {
        bytes(5, payload, &mut out);
    }
    out
}

/// SYN payload; accepts the official snake_case keys and camelCase for robustness.
fn parse_syn(payload: &[u8]) -> Result<(String, u16), String> {
    let value: serde_json::Value =
        serde_json::from_slice(payload).map_err(|_| "invalid SYN payload")?;
    let host = value
        .get("target_host")
        .or_else(|| value.get("targetHost"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .ok_or("missing target host")?;
    let port = value
        .get("target_port")
        .or_else(|| value.get("targetPort"))
        .and_then(|v| v.as_u64())
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p != 0)
        .ok_or("invalid target port")?;
    Ok((host.to_owned(), port))
}

fn syn_ack(ok: bool, error: Option<&str>) -> Vec<u8> {
    let mut value = serde_json::json!({ "version": PROTOCOL_VERSION, "ok": ok });
    if let Some(error) = error {
        value["error"] = serde_json::Value::from(error);
    }
    value.to_string().into_bytes()
}

struct Stream {
    writer: mpsc::Sender<Vec<u8>>,
    task: tokio::task::AbortHandle,
    // Local write overflow must send FIN in order, even while the outbound queue is full.
    close: Option<oneshot::Sender<()>>,
    generation: u64,
    cancelled: Arc<StreamToken>,
    diagnostics: Arc<FlowDiagnostics>,
}

#[derive(Default)]
struct State {
    closed: bool,
    generation: u64,
    streams: HashMap<Key, Stream>,
    // Completed tasks and inactive-stream replies can still be waiting in the outbound queue.
    // Weak tokens keep those frames cancellable without retaining every finished identity.
    retired: HashMap<Key, Weak<StreamToken>>,
    outbound_task: Option<tokio::task::AbortHandle>,
    control_queue: VecDeque<PendingFrame>,
    control_task: Option<tokio::task::AbortHandle>,
    diagnostics_task: Option<tokio::task::AbortHandle>,
}
impl State {
    fn inactive_reply(&mut self, key: &Key, kind: u64, payload: Vec<u8>) -> Option<PendingFrame> {
        if self.closed || self.streams.contains_key(key) {
            return None;
        }
        self.retired.retain(|_, token| token.strong_count() != 0);
        let cancelled = self
            .retired
            .get(key)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| Arc::new(StreamToken::default()));
        // One FIN suffices while this incarnation's FIN is still waiting to be sent.
        // This also recognizes the normal target-EOF FIN retained after registration ends.
        if kind == FIN && cancelled.fin_pending.load(Ordering::Acquire) {
            return None;
        }
        self.retired.insert(key.clone(), Arc::downgrade(&cancelled));
        Some(PendingFrame::stream(key, kind, payload, &cancelled))
    }
}

pub struct Session {
    outbound: mpsc::Sender<PendingFrame>,
    state: Mutex<State>,
    writable: Arc<Notify>,
    control_ready: Arc<Notify>,
    control_overflow: Arc<Notify>,
    buffer_limit: usize,
}

// A task never owns its Session. Dropping the last owner cancels pending connects and I/O;
// a late task exit cannot remove a replacement stream with the same wire identity.
struct Registration {
    session: Weak<Session>,
    key: Key,
    generation: u64,
}
impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(session) = self.session.upgrade() {
            let mut state = session.state.lock().unwrap();
            if state
                .streams
                .get(&self.key)
                .is_some_and(|s| s.generation == self.generation)
            {
                let stream = state.streams.remove(&self.key).unwrap();
                state.retired.retain(|_, token| token.strong_count() != 0);
                state
                    .retired
                    .insert(self.key.clone(), Arc::downgrade(&stream.cancelled));
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Session {
    /// `channel` is the peer's FILE_DATA_CHANNEL slot (filled once the viewer opens it).
    pub fn new(
        channel: Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
        sequence: Arc<AtomicU64>,
    ) -> Arc<Self> {
        Self::with_buffer_limit(channel, sequence, MAX_BUFFERED)
    }

    fn with_buffer_limit(
        channel: Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
        sequence: Arc<AtomicU64>,
        buffer_limit: usize,
    ) -> Arc<Self> {
        let (outbound, mut queue) = mpsc::channel::<PendingFrame>(64);
        let session = Arc::new(Self {
            outbound,
            state: Mutex::new(State::default()),
            writable: Arc::new(Notify::new()),
            control_ready: Arc::new(Notify::new()),
            control_overflow: Arc::new(Notify::new()),
            buffer_limit,
        });
        session.start_control_dispatcher(channel.clone());
        let (weak, writable) = (Arc::downgrade(&session), session.writable.clone());
        let task = tokio::spawn(async move {
            while let Some(PendingFrame {
                frame: (key, kind, payload),
                cancelled,
                diagnostics,
            }) = queue.recv().await
            {
                // One deadline covers channel readiness, SCTP backpressure and send itself.
                // buffered_amount is not reset on close (vendor/webrtc); never poll it forever.
                let sent = tokio::time::timeout(SEND_TIMEOUT, async {
                    loop {
                        let ready = writable.notified();
                        if cancelled
                            .as_ref()
                            .is_some_and(|c| c.load(Ordering::Acquire))
                        {
                            if kind == DATA {
                                if let Some(diagnostics) = &diagnostics {
                                    diagnostics
                                        .cancelled_data
                                        .fetch_add(payload.len() as u64, Ordering::Relaxed);
                                }
                            }
                            return true;
                        }
                        let slot = channel.lock().unwrap().clone();
                        if let Some(dc) = slot {
                            match dc.ready_state() {
                                RTCDataChannelState::Closing | RTCDataChannelState::Closed => {
                                    return false
                                }
                                RTCDataChannelState::Open
                                    if dc.buffered_amount().await <= buffer_limit =>
                                {
                                    let timestamp = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs();
                                    let message = crate::initialization::envelope(
                                        sequence.fetch_add(1, Ordering::Relaxed),
                                        timestamp,
                                        MESSAGE_FIELD,
                                        &frame(&key, kind, &payload),
                                    );
                                    let sent = dc.send(&bytes::Bytes::from(message)).await.is_ok();
                                    if sent {
                                        if let Some(diagnostics) = &diagnostics {
                                            diagnostics.sent_frames.fetch_add(1, Ordering::Relaxed);
                                            if kind == DATA {
                                                diagnostics.sent_data.fetch_add(
                                                    payload.len() as u64,
                                                    Ordering::Relaxed,
                                                );
                                            }
                                        }
                                    }
                                    return sent;
                                }
                                _ => {}
                            }
                        }
                        ready.await;
                    }
                })
                .await;
                if sent != Ok(true) {
                    if let Some(diagnostics) = &diagnostics {
                        diagnostics.end(
                            if sent.is_err() {
                                "outbound_send_timeout"
                            } else {
                                "outbound_send_error"
                            },
                            true,
                        );
                        diagnostics.report("outbound_failed");
                    }
                    log::warn!(
                        "Port mapping send unavailable or timed out; closing mapping session"
                    );
                    if let Some(session) = weak.upgrade() {
                        // This worker performs the final close itself; do not self-abort first.
                        session.state.lock().unwrap().outbound_task.take();
                        session.shutdown();
                    }
                    // A failed mapping must be visible to the viewer so it can reconnect.
                    // Only this unusable FILE channel is closed, never another peer.
                    let dc = channel.lock().unwrap().clone();
                    if let Some(dc) = dc {
                        let _ = tokio::time::timeout(Duration::from_secs(2), dc.close()).await;
                    }
                    break;
                }
            }
        });
        session.state.lock().unwrap().outbound_task = Some(task.abort_handle());
        if log::log_enabled!(log::Level::Debug) {
            session.start_diagnostics();
        }
        session
    }

    fn start_diagnostics(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            let period = Duration::from_secs(5);
            let mut interval =
                tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let Some(session) = weak.upgrade() else { break };
                let (flows, control_pending, closed) = {
                    let state = session.state.lock().unwrap();
                    (
                        state
                            .streams
                            .values()
                            .map(|stream| stream.diagnostics.clone())
                            .collect::<Vec<_>>(),
                        state.control_queue.len(),
                        state.closed,
                    )
                };
                let outbound_pending =
                    session.outbound.max_capacity() - session.outbound.capacity();
                drop(session);
                log::debug!("Port mapping snapshot: active={} outbound_pending={} control_pending={} closed={}",
                    flows.len(), outbound_pending, control_pending, closed);
                for flow in flows {
                    flow.report("active");
                }
            }
        });
        self.state.lock().unwrap().diagnostics_task = Some(task.abort_handle());
    }

    fn start_control_dispatcher(
        self: &Arc<Self>,
        channel: Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
    ) {
        let weak = Arc::downgrade(self);
        let outbound = self.outbound.clone();
        let ready = self.control_ready.clone();
        let overflow = self.control_overflow.clone();
        let task = tokio::spawn(async move {
            let dispatch = async {
                loop {
                    let notified = ready.notified();
                    // Do not retain the Session across a queue wait: its Drop must cancel us.
                    let pending = match weak.upgrade() {
                        Some(session) => session.state.lock().unwrap().control_queue.pop_front(),
                        None => return,
                    };
                    match pending {
                        Some(pending) => {
                            if pending
                                .cancelled
                                .as_ref()
                                .is_some_and(|c| c.load(Ordering::Acquire))
                            {
                                continue;
                            }
                            if outbound.send(pending).await.is_err() {
                                return;
                            }
                        }
                        None => notified.await,
                    }
                }
            };
            tokio::select! {
                biased;
                _ = overflow.notified() => {
                    // Saturation is explicit to this viewer; never silently lose its FIN/ACK.
                    let dc = channel.lock().unwrap().clone();
                    if let Some(dc) = dc {
                        let _ = tokio::time::timeout(Duration::from_secs(2), dc.close()).await;
                    }
                }
                _ = dispatch => {}
            }
        });
        self.state.lock().unwrap().control_task = Some(task.abort_handle());
    }

    fn queue_control(&self, pending: PendingFrame) {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return;
        }
        // A new SYN/remote FIN invalidates queued replies for the old incarnation. Pruning
        // here keeps repeated invalid SYNs bounded, even when the ordinary FIFO is full.
        state.control_queue.retain(|frame| {
            !frame
                .cancelled
                .as_ref()
                .is_some_and(|c| c.load(Ordering::Acquire))
        });
        if state.control_queue.len() < CONTROL_QUEUE_FRAMES {
            state.control_queue.push_back(pending);
            drop(state);
            self.control_ready.notify_one();
        } else {
            state.closed = true;
            // The dispatcher performs the final FILE close; shutdown cancels all other work.
            state.control_task.take();
            drop(state);
            log::warn!("Port mapping session failed: reason=control_queue_full");
            self.control_overflow.notify_one();
            self.shutdown();
        }
    }

    /// FILE has one owner of these callbacks; CONTROL/TEXT keep their existing handlers.
    pub async fn attach_channel(self: &Arc<Self>, channel: &Arc<RTCDataChannel>) {
        channel
            .set_buffered_amount_low_threshold(self.buffer_limit / 2)
            .await;
        let writable = self.writable.clone();
        channel
            .on_buffered_amount_low(Box::new(move || {
                writable.notify_one();
                Box::pin(async {})
            }))
            .await;
        let writable = self.writable.clone();
        channel.on_open(Box::new(move || {
            writable.notify_one();
            Box::pin(async {})
        }));
        let session = Arc::downgrade(self);
        channel.on_close(Box::new(move || {
            if let Some(session) = session.upgrade() {
                session.shutdown();
            }
            Box::pin(async {})
        }));
        self.writable.notify_one();
    }

    /// Handles Message field 27 if present; returns true when the message was a port-mapping frame.
    pub async fn handle(self: &Arc<Self>, message: &[WireField<'_>], channel: &str) -> bool {
        let Some(WireValue::Bytes(raw)) = message
            .iter()
            .find(|f| f.number == MESSAGE_FIELD)
            .map(|f| &f.value)
        else {
            return false;
        };
        if channel != "FILE_DATA_CHANNEL" {
            log::warn!("Port mapping frame ignored on {}", channel);
            return true;
        }
        let Ok(fields) = decode_wire(raw) else {
            log::warn!("Port mapping frame undecodable");
            return true;
        };
        let key = Key {
            session: Id::of(&fields, 1),
            rule: Id::of(&fields, 2),
            stream: Id::of(&fields, 3),
        };
        let kind = fields
            .iter()
            .find_map(|f| match (f.number, &f.value) {
                (4, WireValue::Varint(v)) => Some(*v),
                _ => None,
            })
            .unwrap_or(SYN);
        let payload = fields
            .iter()
            .find_map(|f| match (f.number, &f.value) {
                (5, WireValue::Bytes(b)) => Some(*b),
                _ => None,
            })
            .unwrap_or(&[]);
        if self.state.lock().unwrap().closed {
            return true;
        }
        match kind {
            SYN => {
                if let Some(reply) = self.syn(key, payload) {
                    self.queue_control(reply);
                }
            }
            DATA => {
                let writer = self
                    .state
                    .lock()
                    .unwrap()
                    .streams
                    .get(&key)
                    .map(|s| (s.writer.clone(), s.diagnostics.clone()));
                match writer {
                    Some((writer, diagnostics)) => {
                        diagnostics.rx_data_frames.fetch_add(1, Ordering::Relaxed);
                        diagnostics
                            .rx_data_bytes
                            .fetch_add(payload.len() as u64, Ordering::Relaxed);
                        match writer.try_send(payload.to_vec()) {
                            Ok(()) => {}
                            Err(error) => {
                                diagnostics.end(
                                    if matches!(error, mpsc::error::TrySendError::Full(_)) {
                                        "rx_queue_full"
                                    } else {
                                        "target_writer_closed"
                                    },
                                    true,
                                );
                                self.close(&key, true);
                            }
                        }
                    }
                    None => {
                        let reply =
                            self.state
                                .lock()
                                .unwrap()
                                .inactive_reply(&key, FIN, Vec::new());
                        if let Some(reply) = reply {
                            self.queue_control(reply);
                        }
                    }
                }
            }
            FIN => self.close(&key, false),
            DATA_ACK => {}
            other => log::info!("Port mapping unsupported frame type {}", other),
        }
        true
    }

    fn syn(self: &Arc<Self>, key: Key, payload: &[u8]) -> Option<PendingFrame> {
        self.syn_with(key, payload, |host, port| {
            tokio::net::TcpStream::connect((host, port))
        })
    }

    fn syn_with<F, C>(
        self: &Arc<Self>,
        key: Key,
        payload: &[u8],
        connect: F,
    ) -> Option<PendingFrame>
    where
        F: FnOnce(String, u16) -> C,
        C: std::future::Future<Output = std::io::Result<tokio::net::TcpStream>> + Send + 'static,
    {
        let mut state = self.state.lock().unwrap();
        // A duplicate SYN never replaces a live or pending TCP connection. A new SYN is
        // accepted after FIN/cleanup; old task cleanup is generation-checked.
        if state.closed {
            return None;
        }
        if let Some(stream) = state.streams.get(&key) {
            stream
                .diagnostics
                .duplicate_syn
                .store(true, Ordering::Relaxed);
            stream.diagnostics.report("duplicate_syn");
            return None;
        }
        let identity_reuse = state.retired.contains_key(&key);
        state.retired.retain(|_, token| token.strong_count() != 0);
        if let Some(cancelled) = state.retired.remove(&key).and_then(|token| token.upgrade()) {
            cancelled.store(true, Ordering::Release);
            self.writable.notify_one();
        }
        let request = if enabled() {
            parse_syn(payload)
        } else {
            Err("port mapping disabled on this host".into())
        };
        let (host, port) = match request {
            Ok(target) => target,
            Err(error) => {
                let reply = state.inactive_reply(&key, SYN_ACK, syn_ack(false, Some(&error)));
                drop(state);
                log::info!("Port mapping SYN rejected: {}", error);
                return reply;
            }
        };
        state.generation += 1;
        let generation = state.generation;
        let registration = Registration {
            session: Arc::downgrade(self),
            key: key.clone(),
            generation,
        };
        let (writer, writes) = mpsc::channel(WRITE_QUEUE_FRAMES);
        let (close, closing) = oneshot::channel();
        let (outbound, task_key) = (self.outbound.clone(), key.clone());
        let cancelled = Arc::new(StreamToken::default());
        let task_cancelled = cancelled.clone();
        let diagnostics = FlowDiagnostics::new(identity_reuse);
        diagnostics.report("registered");
        let task_diagnostics = diagnostics.clone();
        let connecting = connect(host.clone(), port);
        let task = tokio::spawn(async move {
            let _registration = registration;
            let send_fin = tokio::select! {
                biased;
                _ = closing => { task_diagnostics.end("local_close", false); true },
                result = Self::forward(&outbound, &task_key, &host, port, connecting, writes, &task_cancelled, &task_diagnostics) => result,
            };
            task_diagnostics.report("target_closed");
            // forward's socket halves have been dropped before waiting for queue capacity.
            // Remote FIN and peer shutdown can still abort this tracked task while it waits.
            if send_fin {
                if outbound
                    .send(
                        PendingFrame::stream(&task_key, FIN, Vec::new(), &task_cancelled)
                            .tracked(&task_diagnostics),
                    )
                    .await
                    .is_ok()
                {
                    task_diagnostics.queued(FIN, 0);
                }
            }
        });
        state.streams.insert(
            key,
            Stream {
                writer,
                task: task.abort_handle(),
                close: Some(close),
                generation,
                cancelled,
                diagnostics,
            },
        );
        None
    }

    async fn forward(
        outbound: &mpsc::Sender<PendingFrame>,
        key: &Key,
        host: &str,
        port: u16,
        connecting: impl std::future::Future<Output = std::io::Result<tokio::net::TcpStream>>,
        mut writes: mpsc::Receiver<Vec<u8>>,
        cancelled: &Arc<StreamToken>,
        diagnostics: &Arc<FlowDiagnostics>,
    ) -> bool {
        log::info!(
            "Port mapping connecting: port={} loopback={}",
            port,
            host == "localhost" || host.starts_with("127.") || host == "::1"
        );
        let connected = tokio::time::timeout(CONNECT_TIMEOUT, connecting).await;
        let stream = match connected {
            Ok(Ok(stream)) => stream,
            result => {
                diagnostics.end(
                    if result.is_err() {
                        "connect_timeout"
                    } else {
                        "connect_error"
                    },
                    true,
                );
                let error = match result {
                    Ok(Err(e)) => format!("connect failed: {}", e.kind()),
                    _ => "connect timeout".into(),
                };
                log::info!("Port mapping connect failed: {}", error);
                if outbound
                    .send(
                        PendingFrame::stream(key, SYN_ACK, syn_ack(false, Some(&error)), cancelled)
                            .tracked(diagnostics),
                    )
                    .await
                    .is_ok()
                {
                    diagnostics.queued(SYN_ACK, 0);
                }
                return false;
            }
        };
        let _ = stream.set_nodelay(true);
        let (mut read, mut write) = stream.into_split();
        // The stream was registered before connecting; FIN during connect/ACK cancels it.
        // SYN_ACK and all target DATA share one ordered queue.
        if outbound
            .send(
                PendingFrame::stream(key, SYN_ACK, syn_ack(true, None), cancelled)
                    .tracked(diagnostics),
            )
            .await
            .is_err()
        {
            diagnostics.end("outbound_queue_closed", true);
            return false;
        }
        diagnostics.queued(SYN_ACK, 0);
        log::info!("Port mapping stream open: port={}", port);
        tokio::select! {
            result = async {
                let mut buf = vec![0u8; READ_CHUNK];
                loop {
                    match read.read(&mut buf).await {
                        Ok(0) => { diagnostics.end("target_eof", false); return true; },
                        Err(_) => { diagnostics.end("target_read_error", true); return true; },
                        Ok(n) => {
                            diagnostics.target_read.fetch_add(n as u64, Ordering::Relaxed);
                            if outbound.send(PendingFrame::stream(key, DATA, buf[..n].to_vec(), cancelled).tracked(diagnostics)).await.is_err() {
                                diagnostics.end("outbound_queue_closed", true);
                                return false;
                            }
                            diagnostics.queued(DATA, n);
                        },
                    }
                }
            } => result,
            _ = async {
                while let Some(data) = writes.recv().await {
                    let mut offset = 0;
                    while offset < data.len() {
                        match write.write(&data[offset..]).await {
                            Ok(0) | Err(_) => { diagnostics.end("target_write_error", true); return; },
                            Ok(n) => {
                                offset += n;
                                diagnostics.target_written.fetch_add(n as u64, Ordering::Relaxed);
                            }
                        }
                    }
                }
                diagnostics.end("target_writer_closed", false);
            } => true,
        }
    }

    fn close(&self, key: &Key, send_fin: bool) {
        let mut state = self.state.lock().unwrap();
        if send_fin {
            if let Some(stream) = state.streams.get_mut(key) {
                if let Some(close) = stream.close.take() {
                    let _ = close.send(());
                }
            }
        } else {
            if let Some(stream) = state.streams.remove(key) {
                stream.diagnostics.end("peer_fin", false);
                stream.diagnostics.report("peer_fin");
                // Frames still queued for this incarnation must not reach a reused wire identity.
                stream.cancelled.store(true, Ordering::Release);
                self.writable.notify_one();
                stream.task.abort();
            }
            if let Some(cancelled) = state.retired.remove(key).and_then(|token| token.upgrade()) {
                cancelled.store(true, Ordering::Release);
                self.writable.notify_one();
            }
        }
    }

    /// Permanent and idempotent: no new stream can register after this lock is released.
    pub fn shutdown(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        if let Some(task) = state.outbound_task.take() {
            task.abort();
        }
        if let Some(task) = state.control_task.take() {
            task.abort();
        }
        if let Some(task) = state.diagnostics_task.take() {
            task.abort();
        }
        state.control_queue.clear();
        if !state.streams.is_empty() {
            log::info!(
                "Port mapping closed {} stream(s) with the connection",
                state.streams.len()
            );
        }
        for (_, stream) in state.streams.drain() {
            stream.diagnostics.end("shutdown", false);
            stream.diagnostics.report("shutdown");
            stream.task.abort();
        }
        state.retired.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syn_payload_uses_official_snake_case_keys() {
        assert_eq!(
            parse_syn(br#"{"version":1,"target_host":"127.0.0.1","target_port":22}"#).unwrap(),
            ("127.0.0.1".into(), 22)
        );
        assert_eq!(
            parse_syn(br#"{"targetHost":"nas.local","targetPort":8080}"#).unwrap(),
            ("nas.local".into(), 8080)
        );
        assert!(parse_syn(br#"{"target_host":"","target_port":22}"#).is_err());
        assert!(parse_syn(br#"{"target_host":"h","target_port":70000}"#).is_err());
        assert!(parse_syn(b"not json").is_err());
    }

    #[test]
    fn syn_ack_reports_ok_and_error() {
        let ok: serde_json::Value = serde_json::from_slice(&syn_ack(true, None)).unwrap();
        assert_eq!(ok, serde_json::json!({"version": 1, "ok": true}));
        let failed: serde_json::Value =
            serde_json::from_slice(&syn_ack(false, Some("connect timeout"))).unwrap();
        assert_eq!(failed["ok"], false);
        assert_eq!(failed["error"], "connect timeout");
    }

    #[test]
    fn frames_echo_identity_fields_with_their_wire_types() {
        let key = Key {
            session: Id::Bytes(b"s1".to_vec()),
            rule: Id::Varint(7),
            stream: Id::Varint(300),
        };
        let encoded = frame(&key, DATA, b"abc");
        let fields = decode_wire(&encoded).unwrap();
        assert_eq!(Id::of(&fields, 1), Id::Bytes(b"s1".to_vec()));
        assert_eq!(Id::of(&fields, 2), Id::Varint(7));
        assert_eq!(Id::of(&fields, 3), Id::Varint(300));
        assert!(fields
            .iter()
            .any(|f| f.number == 4 && f.value == WireValue::Varint(DATA)));
        assert!(fields
            .iter()
            .any(|f| f.number == 5 && f.value == WireValue::Bytes(b"abc")));
        // SYN is the proto3 default (0) and is omitted on the wire.
        let syn = frame(&key, SYN, b"");
        assert!(!decode_wire(&syn)
            .unwrap()
            .iter()
            .any(|f| f.number == 4 || f.number == 5));
    }
}

#[cfg(test)]
#[path = "port_mapping_tests.rs"]
mod lifecycle_tests;

#[cfg(test)]
#[path = "port_mapping_concurrency_tests.rs"]
mod concurrency_tests;
