//! Official UU port-mapping peer. Media negotiation is retained as inert compatibility
//! scaffolding; this module never captures, encodes, decodes or injects desktop input.
use crate::{
    session_ice::SessionIce,
    signaling::{SignalingClient, SoacMessage},
};
use base64::Engine;
use std::{
    io::Read,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::Duration,
};
use tokio::sync::Mutex;
use webrtc::{
    api::{media_engine::MediaEngine, APIBuilder},
    data_channel::{data_channel_state::RTCDataChannelState, RTCDataChannel},
    dtls_transport::dtls_role::DTLSRole,
    ice_transport::ice_candidate::RTCIceCandidateInit,
    peer_connection::{
        peer_connection_state::RTCPeerConnectionState as State,
        sdp::session_description::RTCSessionDescription, RTCPeerConnection,
    },
    rtp_transceiver::rtp_codec::{
        RTCRtpCodecCapability, RTCRtpHeaderExtensionCapability, RTPCodecType,
    },
    track::track_local::track_local_static_rtp::TrackLocalStaticRTP,
};

#[derive(Default, Clone)]
pub struct SessionContext {
    pub client_id: Option<String>,
    pub ice_id: Option<serde_json::Value>,
    pub app_control_id: Option<serde_json::Value>,
    pub bundle_media: Option<(String, u16)>,
}
pub struct WebRtcSession {
    pub pc: Arc<RTCPeerConnection>,
    pub context: Arc<Mutex<SessionContext>>,
    mapping: Arc<crate::port_mapping::Session>,
    tasks: Arc<StdMutex<Vec<tokio::task::AbortHandle>>>,
    closed: tokio::sync::watch::Sender<bool>,
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn stop(tasks: &StdMutex<Vec<tokio::task::AbortHandle>>) {
    for task in tasks.lock().unwrap().drain(..) {
        task.abort();
    }
}
impl Drop for WebRtcSession {
    fn drop(&mut self) {
        self.mapping.shutdown();
        self.closed.send_replace(true);
        stop(&self.tasks);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let pc = self.pc.clone();
            runtime.spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(5), pc.close()).await;
            });
        }
    }
}
impl WebRtcSession {
    pub async fn new(
        signaling: Arc<SignalingClient>,
        transport: SessionIce,
    ) -> Result<Self, String> {
        let mut media = MediaEngine::default();
        media
            .register_default_codecs()
            .map_err(|_| "Register codecs failed")?;
        // Preserve the official offer's metadata extensions without creating an encoder.
        for uri in [
            "http://www.webrtc.org/experiments/rtp-hdrext/video-capture-index",
            "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-is-new-frame",
        ] {
            media
                .register_header_extension(
                    RTCRtpHeaderExtensionCapability { uri: uri.into() },
                    RTPCodecType::Video,
                    None,
                )
                .map_err(|_| "Register metadata extension failed")?;
        }
        let registry = webrtc::api::interceptor_registry::register_default_interceptors(
            Default::default(),
            &mut media,
        )
        .map_err(|_| "Register interceptors failed")?;
        let mut settings = webrtc::api::setting_engine::SettingEngine::default();
        #[cfg(test)]
        if let Some(setting) = &transport.setting_engine {
            settings = setting.clone();
        }
        settings
            .set_answering_dtls_role(DTLSRole::Server)
            .map_err(|_| "Configure DTLS role failed")?;
        settings.set_ice_multicast_dns_mode(webrtc::ice::mdns::MulticastDnsMode::Disabled);
        settings.set_interface_filter(Box::new(ice_interface_allowed));
        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_setting_engine(settings)
            .with_interceptor_registry(registry)
            .build();
        let pc = Arc::new(
            api.new_peer_connection(transport.configuration()?)
                .await
                .map_err(|_| "Create peer failed")?,
        );
        let context = Arc::new(Mutex::new(SessionContext::default()));
        let tasks = Arc::new(StdMutex::new(Vec::new()));
        let (closed, _) = tokio::sync::watch::channel(false);
        let sequence = Arc::new(AtomicU64::new(1));
        let file_channel = Arc::new(StdMutex::new(None));
        let mapping = crate::port_mapping::Session::new(file_channel.clone(), sequence.clone());
        crate::network_diagnostics::start(Arc::downgrade(&pc));

        let ctx = context.clone();
        let signal = signaling.clone();
        pc.on_ice_candidate(Box::new(move |candidate| {
            let (ctx, signal) = (ctx.clone(), signal.clone());
            Box::pin(async move {
                let Some(candidate) = candidate else { return; };
                let Ok(candidate) = candidate.to_json() else { return; };
                let ctx = ctx.lock().await.clone();
                let (Some(client), Some((mid, index))) = (ctx.client_id, ctx.bundle_media) else { return; };
                let message = SoacMessage { msg_type: "candidate".into(), sdp: None, gzip_sdp: None,
                    candidate: Some(serde_json::json!({"candidate":candidate.candidate,"sdpMid":mid,"sdpMLineIndex":index})),
                    ice_id: ctx.ice_id, client_id: Some(client), app_control_id: ctx.app_control_id, sdp_mid: None, sdp_mline_index: None };
                if !matches!(tokio::time::timeout(Duration::from_secs(5), signal.emit_soac(message)).await, Ok(Ok(_))) {
                    log::warn!("ICE signaling delivery failed or timed out");
                }
            })
        }));
        let ctx = context.clone();
        let ending = Arc::downgrade(&mapping);
        let ended = closed.clone();
        let end_tasks = tasks.clone();
        let notified = AtomicBool::new(false);
        pc.on_peer_connection_state_change(Box::new(move |state| {
            log::info!("PeerConnection State changed: {:?}", state);
            if matches!(state, State::Closed | State::Failed) {
                ended.send_replace(true);
                stop(&end_tasks);
                if let Some(mapping) = ending.upgrade() {
                    mapping.shutdown();
                }
            }
            let first = state == State::Connected && !notified.swap(true, Ordering::AcqRel);
            let (ctx, signal) = (ctx.clone(), signaling.clone());
            if first {
                // Do not hold the peer state callback behind a congested cloud socket.
                let mut tasks = end_tasks.lock().unwrap();
                if !*ended.borrow() {
                    let task = tokio::spawn(async move {
                        let ctx = ctx.lock().await.clone();
                        if let (Some(client), Some(ice)) = (
                            ctx.client_id.as_deref(),
                            ctx.ice_id.as_ref().and_then(|v| v.as_str()),
                        ) {
                            for payload in crate::compat::legacy_settings(client, ice) {
                                let _ = tokio::time::timeout(
                                    Duration::from_secs(5),
                                    signal.emit_event("forward_setting", payload),
                                )
                                .await;
                            }
                        }
                        if let (Some(ice), Some(control)) = (
                            ctx.ice_id.as_ref().and_then(|v| v.as_str()),
                            ctx.app_control_id.as_ref().and_then(|v| v.as_str()),
                        ) {
                            // Observed official telemetry, not a cloud ACK or a media capability claim.
                            let _ = tokio::time::timeout(
                                Duration::from_secs(5),
                                signal.emit_event(
                                    "ice_finished_log",
                                    serde_json::json!({"ice_id":ice,"app_control_id":control}),
                                ),
                            )
                            .await;
                        }
                    });
                    tasks.push(task.abort_handle());
                }
            }
            Box::pin(async {})
        }));

        // The upstream headless tool peer binds one H264 track, with no audio sender.
        // Keep that SDP behavior; datachannel-only negotiation needs separate client acceptance.
        let video = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: "video/H264".into(),
                clock_rate: 90_000,
                ..Default::default()
            },
            "video_0".into(),
            "video_0".into(),
        ));
        let sender = pc
            .add_track(video)
            .await
            .map_err(|_| "Bind compatibility track failed")?;
        let feedback = tokio::spawn(async move {
            let mut buffer = [0u8; 1500];
            while sender.read(&mut buffer).await.is_ok() {}
        });
        tasks.lock().unwrap().push(feedback.abort_handle());

        let peer_mapping = mapping.clone();
        let peer_closed = closed.clone();
        pc.on_data_channel(Box::new(move |channel: Arc<RTCDataChannel>| {
            let (mapping, sequence, slot, closed) = (peer_mapping.clone(), sequence.clone(), file_channel.clone(), peer_closed.clone());
            Box::pin(async move {
                let label = channel.label().to_owned();
                if !matches!(label.as_str(), "FILE_DATA_CHANNEL" | "CONTROL_DATA_CHANNEL" | "TEXT_DATA_CHANNEL") {
                    let _ = channel.close().await; return;
                }
                log::info!("Tool DataChannel received: {}", label);
                if label == "FILE_DATA_CHANNEL" {
                    *slot.lock().unwrap() = Some(channel.clone());
                    mapping.attach_channel(&channel).await;
                } else {
                    let weak = Arc::downgrade(&channel); let seq = sequence.clone(); let mut ended = closed.subscribe();
                    channel.on_open(Box::new(move || Box::pin(async move {
                        let Some(channel) = weak.upgrade() else { return; };
                        let messages = if channel.label() == "CONTROL_DATA_CHANNEL" {
                            crate::compat::initial(|| seq.fetch_add(1, Ordering::Relaxed), now())
                        } else { vec![crate::compat::metrics(seq.fetch_add(1, Ordering::Relaxed), now())] };
                        for message in messages {
                            let data = bytes::Bytes::from(message);
                            tokio::select! {
                                _ = ended.wait_for(|v| *v) => return,
                                result = tokio::time::timeout(Duration::from_secs(5), channel.send(&data)) => if !matches!(result, Ok(Ok(_))) { return; },
                            }
                        }
                    })));
                }
                let weak = Arc::downgrade(&channel);
                channel.on_message(Box::new(move |message| {
                    let (mapping, seq, weak, label) = (mapping.clone(), sequence.clone(), weak.clone(), label.clone());
                    Box::pin(async move {
                        let Ok(fields) = crate::protocol::decode_wire(&message.data) else { return; };
                        if mapping.handle(&fields, &label).await { return; }
                        if !matches!(label.as_str(), "CONTROL_DATA_CHANNEL" | "TEXT_DATA_CHANNEL") { return; }
                        let Some(channel) = weak.upgrade() else { return; };
                        for response in crate::compat::replies(&message.data, || seq.fetch_add(1, Ordering::Relaxed), now()) {
                            if channel.ready_state() != RTCDataChannelState::Open { break; }
                            if !matches!(tokio::time::timeout(Duration::from_secs(5), channel.send(&response.into())).await, Ok(Ok(_))) { break; }
                        }
                    })
                }));
            })
        }));
        Ok(Self {
            pc,
            context,
            mapping,
            tasks,
            closed,
        })
    }
    pub async fn close(&self) {
        self.mapping.shutdown();
        self.closed.send_replace(true);
        stop(&self.tasks);
        let _ = tokio::time::timeout(Duration::from_secs(5), self.pc.close()).await;
    }
    pub async fn handle_offer(&self, soac: &SoacMessage) -> Result<String, String> {
        {
            let mut ctx = self.context.lock().await;
            ctx.client_id = soac.client_id.clone();
            ctx.ice_id = soac.ice_id.clone();
            ctx.app_control_id = soac.app_control_id.clone();
        }
        let sdp = offer_sdp(soac)?;
        let offer = RTCSessionDescription::offer(sdp).map_err(|_| "Invalid offer")?;
        self.pc
            .set_remote_description(offer)
            .await
            .map_err(|_| "Set remote description failed")?;
        let answer = self
            .pc
            .create_answer(None)
            .await
            .map_err(|_| "Create answer failed")?;
        self.context.lock().await.bundle_media =
            Some(negotiated_bundle_media(&answer.sdp).ok_or("No active BUNDLE transport")?);
        self.pc
            .set_local_description(answer.clone())
            .await
            .map_err(|_| "Set local description failed")?;
        Ok(with_max_message_size(&answer.sdp))
    }
    pub async fn add_candidate(&self, soac: &SoacMessage) -> Result<(), String> {
        let Some(value) = &soac.candidate else {
            return Ok(());
        };
        let candidate = value
            .as_str()
            .or_else(|| value.get("candidate").and_then(|v| v.as_str()))
            .unwrap_or("");
        if candidate.is_empty() {
            return Ok(());
        }
        let mid = value
            .get("sdpMid")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .or_else(|| soac.sdp_mid.clone())
            .or(Some("0".into()));
        let index = value
            .get("sdpMLineIndex")
            .and_then(|v| v.as_u64())
            .and_then(|v| u16::try_from(v).ok())
            .or_else(|| soac.sdp_mline_index.and_then(|v| u16::try_from(v).ok()))
            .or(Some(0));
        self.pc
            .add_ice_candidate(RTCIceCandidateInit {
                candidate: candidate.into(),
                sdp_mid: mid,
                sdp_mline_index: index,
                ..Default::default()
            })
            .await
            .map_err(|_| "Add candidate failed".into())
    }
}
fn offer_sdp(soac: &SoacMessage) -> Result<String, String> {
    const LIMIT: usize = 2 * 1024 * 1024;
    let mut sdp = soac.sdp.clone().unwrap_or_default();
    if sdp.is_empty() {
        if let Some(encoded) = &soac.gzip_sdp {
            if encoded.len() > LIMIT {
                return Err("Compressed SDP too large".into());
            }
            let data = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| "Invalid compressed SDP")?;
            flate2::read::GzDecoder::new(data.as_slice())
                .take((LIMIT + 1) as u64)
                .read_to_string(&mut sdp)
                .map_err(|_| "Invalid compressed SDP")?;
        }
    }
    if sdp.is_empty() || sdp.len() > LIMIT {
        return Err("Offer SDP empty or too large".into());
    }
    Ok(sdp)
}
fn with_max_message_size(sdp: &str) -> String {
    if sdp.contains("a=max-message-size:") {
        return sdp.into();
    }
    let eol = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::new();
    for line in sdp.split_inclusive('\n') {
        out.push_str(line);
        if line.trim_end().starts_with("a=sctp-port:") {
            out.push_str(&format!("a=max-message-size:262144{eol}"));
        }
    }
    out
}
fn negotiated_bundle_media(sdp: &str) -> Option<(String, u16)> {
    let preferred: Vec<&str> = sdp
        .lines()
        .find_map(|l| l.strip_prefix("a=group:BUNDLE "))?
        .split_whitespace()
        .collect();
    let mut sections = Vec::<(bool, Option<String>)>::new();
    for line in sdp.lines() {
        if let Some(media) = line.strip_prefix("m=") {
            sections.push((media.split_whitespace().nth(1)? != "0", None));
        } else if let Some(mid) = line.strip_prefix("a=mid:") {
            if let Some(section) = sections.last_mut() {
                section.1 = Some(mid.into());
            }
        }
    }
    for mid in preferred {
        for (index, (active, section)) in sections.iter().enumerate() {
            if *active && section.as_deref() == Some(mid) {
                return Some((mid.into(), index.try_into().ok()?));
            }
        }
    }
    None
}
fn ice_interface_allowed(name: &str) -> bool {
    !["zt", "docker", "br-", "veth", "virbr", "lo"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

#[cfg(test)]
#[path = "peer_tests.rs"]
mod tests;
