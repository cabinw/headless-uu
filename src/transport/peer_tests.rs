use super::*;
use crate::{
    initialization,
    protocol::{decode_wire, WireValue},
};
use std::collections::HashMap;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};
struct Viewer {
    pc: Arc<RTCPeerConnection>,
    channels: HashMap<String, Arc<RTCDataChannel>>,
    messages: mpsc::UnboundedReceiver<(String, Vec<u8>)>,
    router: Arc<Mutex<webrtc::util::vnet::router::Router>>,
}
fn soac(sdp: String) -> SoacMessage {
    SoacMessage {
        msg_type: "offer".into(),
        sdp: Some(sdp),
        gzip_sdp: None,
        candidate: None,
        ice_id: None,
        sdp_mid: None,
        sdp_mline_index: None,
        client_id: None,
        app_control_id: None,
    }
}
async fn connect() -> (WebRtcSession, Viewer) {
    let (left, right, router) = crate::test_network::virtual_network().await;
    let host = WebRtcSession::new(
        SignalingClient::disconnected(),
        SessionIce::virtual_net(left),
    )
    .await
    .unwrap();
    let mut media = MediaEngine::default();
    media.register_default_codecs().unwrap();
    let pc = Arc::new(
        APIBuilder::new()
            .with_media_engine(media)
            .with_setting_engine(right)
            .build()
            .new_peer_connection(Default::default())
            .await
            .unwrap(),
    );
    // Official tools can offer media m-lines even though no media is transmitted.
    for kind in [RTPCodecType::Audio, RTPCodecType::Video] {
        pc.add_transceiver_from_kind(kind, None).await.unwrap();
    }
    let (tx, messages) = mpsc::unbounded_channel();
    let mut channels = HashMap::new();
    for label in [
        "CONTROL_DATA_CHANNEL",
        "TEXT_DATA_CHANNEL",
        "FILE_DATA_CHANNEL",
    ] {
        let dc = pc.create_data_channel(label, None).await.unwrap();
        let (tx, label) = (tx.clone(), label.to_string());
        dc.on_message(Box::new(move |message| {
            let _ = tx.send((label.clone(), message.data.to_vec()));
            Box::pin(async {})
        }));
        channels.insert(dc.label().to_owned(), dc);
    }
    let mut gathered = pc.gathering_complete_promise().await;
    pc.set_local_description(pc.create_offer(None).await.unwrap())
        .await
        .unwrap();
    gathered.recv().await;
    let mut gathered = host.pc.gathering_complete_promise().await;
    let answer = host
        .handle_offer(&soac(pc.local_description().await.unwrap().sdp))
        .await
        .unwrap();
    assert!(answer.contains("a=max-message-size:262144"));
    assert!(answer.contains("video_0")); // inert H264 track retained
    gathered.recv().await;
    pc.set_remote_description(
        RTCSessionDescription::answer(with_max_message_size(
            &host.pc.local_description().await.unwrap().sdp,
        ))
        .unwrap(),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while channels
            .values()
            .any(|c| c.ready_state() != RTCDataChannelState::Open)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (
        host,
        Viewer {
            pc,
            channels,
            messages,
            router,
        },
    )
}
fn body(message: &[u8], number: u32) -> Option<&[u8]> {
    decode_wire(message)
        .ok()?
        .into_iter()
        .find_map(|f| match (f.number, f.value) {
            (n, WireValue::Bytes(b)) if n == number => Some(b),
            _ => None,
        })
}
async fn receive(viewer: &mut Viewer, label: &str, field: u32) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (name, message) = viewer.messages.recv().await.unwrap();
            if name == label && body(&message, field).is_some() {
                return message;
            }
        }
    })
    .await
    .unwrap()
}
fn mapping(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![8, 1, 16, 2, 24, 3, 32, kind];
    initialization::bytes(5, payload, &mut frame);
    initialization::envelope(1, 0, 27, &frame)
}
async fn send(viewer: &Viewer, label: &str, data: Vec<u8>) {
    viewer.channels[label].send(&data.into()).await.unwrap();
}

#[tokio::test]
async fn official_tool_handshake_and_large_mapping_frame_work_without_desktop() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let (host, mut viewer)=connect().await;
        let mut seen=std::collections::HashSet::new();
        while seen.len()<3 {
            let (label,message)=viewer.messages.recv().await.unwrap();
            if label=="CONTROL_DATA_CHANNEL" { for field in [7,8,13] { if body(&message,field).is_some() {seen.insert(field);} } }
        }
        for label in ["CONTROL_DATA_CHANNEL","TEXT_DATA_CHANNEL"] {
            send(&viewer,label,initialization::envelope(41,0,3,&[8,0])).await;
            let echo=receive(&mut viewer,label,3).await;
            assert_eq!(body(body(&echo,3).unwrap(),4),Some(&[][..]));
            let mut rpc=Vec::new(); initialization::bytes(1,&[8,41],&mut rpc); initialization::bytes(3,&[8,2],&mut rpc);
            send(&viewer,label,initialization::envelope(42,0,21,&rpc)).await;
            let reply=receive(&mut viewer,label,22).await;
            assert_eq!(body(body(&reply,22).unwrap(),1),Some([8,41].as_slice()));
            receive(&mut viewer,label,25).await;
        }
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let syn=serde_json::json!({"target_host":"127.0.0.1","target_port":listener.local_addr().unwrap().port()}).to_string();
        send(&viewer,"FILE_DATA_CHANNEL",mapping(0,syn.as_bytes())).await;
        let (mut target,_)=listener.accept().await.unwrap();
        let ack=receive(&mut viewer,"FILE_DATA_CHANNEL",27).await;
        assert_eq!(serde_json::from_slice::<serde_json::Value>(body(body(&ack,27).unwrap(),5).unwrap()).unwrap()["ok"],true);
        let payload:Vec<u8>=(0..192*1024).map(|i|(i%251) as u8).collect();
        send(&viewer,"FILE_DATA_CHANNEL",mapping(2,&payload)).await;
        let mut received=vec![0;payload.len()]; target.read_exact(&mut received).await.unwrap(); assert_eq!(received,payload);
        target.write_all(b"echo").await.unwrap();
        let message=receive(&mut viewer,"FILE_DATA_CHANNEL",27).await;
        assert_eq!(body(body(&message,27).unwrap(),5),Some(b"echo".as_slice()));
        host.close().await;
        assert_eq!(target.read(&mut [0]).await.unwrap(),0);
        viewer.pc.close().await.unwrap();viewer.router.lock().await.stop().await.unwrap();
    }).await.unwrap();
}
#[tokio::test]
async fn dropping_one_host_closes_its_tcp_and_leaves_other_peers_usable() {
    tokio::time::timeout(Duration::from_secs(20),async {
        let (host,mut viewer)=connect().await;
        let (other,mut other_viewer)=connect().await;
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let syn=serde_json::json!({"target_host":"127.0.0.1","target_port":listener.local_addr().unwrap().port()}).to_string();
        send(&viewer,"FILE_DATA_CHANNEL",mapping(0,syn.as_bytes())).await;
        let (mut target,_)=listener.accept().await.unwrap();receive(&mut viewer,"FILE_DATA_CHANNEL",27).await;
        drop(host);
        assert_eq!(tokio::time::timeout(Duration::from_secs(3),target.read(&mut [0])).await.unwrap().unwrap(),0);
        send(&other_viewer,"FILE_DATA_CHANNEL",mapping(2,b"unknown stream")).await;
        receive(&mut other_viewer,"FILE_DATA_CHANNEL",27).await;
        other.close().await; viewer.pc.close().await.unwrap(); other_viewer.pc.close().await.unwrap();
        viewer.router.lock().await.stop().await.unwrap();other_viewer.router.lock().await.stop().await.unwrap();
    }).await.unwrap();
}
#[test]
fn bundle_and_sctp_compatibility_are_preserved() {
    let sdp="a=group:BUNDLE data video\nm=audio 0 RTP 0\na=mid:audio\nm=video 9 RTP 98\na=mid:video\nm=application 9 SCTP 0\na=mid:data\na=sctp-port:5000\n";
    assert_eq!(negotiated_bundle_media(sdp), Some(("data".into(), 2)));
    let amended = with_max_message_size(sdp);
    assert!(amended.contains("a=max-message-size:262144"));
    assert_eq!(with_max_message_size(&amended), amended);
}
#[test]
fn compressed_offer_is_bounded_and_decoded_without_logging_credentials() {
    use std::io::Write;
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), Default::default());
    gzip.write_all(b"v=0\r\n").unwrap();
    let mut offer = soac(String::new());
    offer.gzip_sdp = Some(base64::engine::general_purpose::STANDARD.encode(gzip.finish().unwrap()));
    assert_eq!(offer_sdp(&offer).unwrap(), "v=0\r\n");
    offer.gzip_sdp = Some("!invalid!".into());
    assert!(offer_sdp(&offer).is_err());
    offer.sdp = Some("x".repeat(2 * 1024 * 1024 + 1));
    assert!(offer_sdp(&offer).is_err());
}
