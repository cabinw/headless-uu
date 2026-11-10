//! Account room lifecycle. Independent mapping peers survive a signaling reconnect.
use crate::transport::peer::WebRtcSession;
use crate::{
    auth::{Credentials, RoomError, UUAuthClient},
    config, login,
    session_ice::SessionIce,
    signaling::{SignalingClient, SignalingEvent, SoacMessage},
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{mpsc, watch};
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState as State;

type Peers = HashMap<String, WebRtcSession>;
fn ice_key(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}
fn release_ids<T>(peers: &HashMap<String, T>, value: Option<&serde_json::Value>) -> Vec<String> {
    let key = value.map(ice_key);
    peers
        .keys()
        .filter(|id| key.as_ref().is_none_or(|key| key == *id))
        .cloned()
        .collect()
}
async fn close(peer: WebRtcSession) {
    if tokio::time::timeout(Duration::from_secs(5), peer.close())
        .await
        .is_err()
    {
        log::warn!("Peer close timed out");
    }
}
async fn reap(peers: &mut Peers) {
    let ended: Vec<_> = peers
        .iter()
        .filter(|(_, p)| matches!(p.pc.connection_state(), State::Closed | State::Failed))
        .map(|(k, _)| k.clone())
        .collect();
    for id in ended {
        if let Some(peer) = peers.remove(&id) {
            close(peer).await;
        }
    }
}
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|v| *v).await;
}

pub async fn run(mut stop: watch::Receiver<bool>) -> Result<(), String> {
    if *stop.borrow() {
        return Ok(());
    }
    let _lock = super::instance::HostLock::acquire(&config::path())?;
    let mut peers = Peers::new();
    // Cancelling the room loop still reaches deterministic peer cleanup below.
    let result = tokio::select! {
        biased;
        _ = stopped(&mut stop) => Ok(()),
        result = serve(&mut peers) => result,
    };
    let mut closing = tokio::task::JoinSet::new();
    for (_, peer) in peers.drain() {
        closing.spawn(close(peer));
    }
    while closing.join_next().await.is_some() {}
    result
}
async fn serve(peers: &mut Peers) -> Result<(), String> {
    let creds = match config::read_fresh().and_then(|f| Credentials::load(f.as_ref())) {
        Ok(creds) => creds,
        Err(_) => {
            log::warn!("No valid local login; run uuremote-server login in a private terminal");
            login::wait_for_credentials(None).await
        }
    };
    let mut auth = Arc::new(UUAuthClient::new(creds));
    loop {
        reap(peers).await;
        let room = match auth.create_room().await {
            Ok(room) => room,
            Err(RoomError::LoginExpired) => {
                log::warn!("Login expired; waiting for refreshed credentials from CLI login");
                // Compare with the credentials that failed, not a potentially refreshed disk token.
                let stale = Some(auth.creds.token.clone());
                auth = Arc::new(UUAuthClient::new(login::wait_for_credentials(stale).await));
                continue;
            }
            Err(_) => {
                log::warn!("Room request failed; retrying in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        let (events, mut incoming) = mpsc::channel(64);
        let signal =
            match SignalingClient::connect(&room.signaling_server, &room.token, events).await {
                Ok(client) => Arc::new(client),
                Err(_) => {
                    log::warn!("Signaling connect failed; retrying in 5s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
        let mut pending = HashMap::<String, (u64, SessionIce)>::new();
        let mut next = None;
        let mut maintenance = tokio::time::interval(Duration::from_secs(10));
        let renewal = tokio::time::sleep(Duration::from_secs(900));
        tokio::pin!(renewal);
        loop {
            let event = tokio::select! {
                event = incoming.recv() => event,
                _ = maintenance.tick() => { reap(peers).await; continue; },
                _ = &mut renewal => {
                    if peers.values().any(|p| p.pc.connection_state() == State::Connected) {
                        renewal.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(60));
                        continue;
                    }
                    log::info!("Renewing idle signaling room"); break;
                }
            };
            let Some(event) = event else {
                break;
            };
            match event {
                SignalingEvent::Connected => log::info!("Signaling handshake established"),
                SignalingEvent::RoomInfoAck(_) | SignalingEvent::BeControlled(_) => {}
                SignalingEvent::DeviceInfoChanged(value) => {
                    if let Some(data) = value.get("data") {
                        log::info!(
                            "Device status synchronized: status={:?}, availability={:?}",
                            data.get("status").and_then(|v| v.as_str()),
                            data.get("publisher_availability_status")
                                .and_then(|v| v.as_str())
                        );
                    }
                }
                SignalingEvent::ConnectRequest {
                    capture_type,
                    ice_id,
                    transport,
                } => {
                    if pending.len() >= 32 {
                        pending.clear();
                    }
                    if let Some(id) = ice_id {
                        pending.insert(id, (capture_type, transport));
                    } else {
                        next = Some((capture_type, transport));
                    }
                }
                SignalingEvent::Soac(offer) => {
                    let key = offer.ice_id.as_ref().map(ice_key).unwrap_or_default();
                    if offer.msg_type == "offer" {
                        let Some((capture_type, transport)) =
                            pending.remove(&key).or_else(|| next.take())
                        else {
                            log::warn!("Offer ignored: no matching connection request");
                            continue;
                        };
                        // Never start desktop/file/terminal business handlers, even if requested explicitly.
                        if capture_type != 9 {
                            log::info!("Unsupported connection type ignored: {}", capture_type);
                            continue;
                        }
                        if let Some(peer) = peers.remove(&key) {
                            close(peer).await;
                        }
                        let peer = match WebRtcSession::new(signal.clone(), transport).await {
                            Ok(peer) => peer,
                            Err(_) => {
                                log::warn!("Peer creation failed");
                                continue;
                            }
                        };
                        peers.insert(key.clone(), peer);
                        match tokio::time::timeout(
                            Duration::from_secs(20),
                            peers[&key].handle_offer(&offer),
                        )
                        .await
                        {
                            Ok(Ok(answer)) => {
                                let response = SoacMessage {
                                    msg_type: "answer".into(),
                                    sdp: Some(answer),
                                    gzip_sdp: None,
                                    candidate: None,
                                    ice_id: offer.ice_id,
                                    client_id: offer.client_id,
                                    app_control_id: offer.app_control_id,
                                    sdp_mid: None,
                                    sdp_mline_index: None,
                                };
                                if signal.emit_soac(response).await.is_err() {
                                    if let Some(peer) = peers.remove(&key) {
                                        close(peer).await;
                                    }
                                }
                            }
                            _ => {
                                log::warn!("Offer negotiation failed or timed out");
                                if let Some(peer) = peers.remove(&key) {
                                    close(peer).await;
                                }
                            }
                        }
                    } else if offer.msg_type == "candidate" {
                        if let Some(peer) = peers.get(&key) {
                            if peer.add_candidate(&offer).await.is_err() {
                                log::warn!("Remote ICE candidate rejected");
                            }
                        }
                    }
                }
                SignalingEvent::Released(value) | SignalingEvent::Left(value) => {
                    let id = ["ice_id", "iceId"].iter().find_map(|k| {
                        value
                            .get(*k)
                            .or_else(|| value.get("data").and_then(|v| v.get(*k)))
                    });
                    for id in release_ids(peers, id) {
                        if let Some(peer) = peers.remove(&id) {
                            close(peer).await;
                        }
                    }
                }
                SignalingEvent::Disconnected => break,
            }
        }
        // Dropping the event receiver closes this room's heartbeat/socket workers.
        drop(incoming);
        drop(signal);
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn already_stopped_exits_before_login_or_network() {
        let (_sender, stop) = watch::channel(true);
        assert!(run(stop).await.is_ok());
    }
    #[test]
    fn explicit_release_never_closes_an_independent_peer() {
        let peers = HashMap::from([("1".into(), ()), ("2".into(), ())]);
        assert_eq!(release_ids(&peers, Some(&serde_json::json!(1))), vec!["1"]);
        for value in [
            serde_json::Value::Null,
            serde_json::json!(false),
            serde_json::json!({}),
            serde_json::json!("missing"),
        ] {
            assert!(release_ids(&peers, Some(&value)).is_empty());
        }
        assert_eq!(release_ids(&peers, None).len(), 2);
    }
}
