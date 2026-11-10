//! The small CONTROL/TEXT surface required by official port-mapping viewers.
//! No desktop, filesystem, terminal, clipboard or power operation is dispatched here.
use crate::{
    initialization::{self, bytes, integer},
    protocol::{decode_wire, WireField, WireValue},
};

fn message<'a>(fields: &[WireField<'a>], number: u32) -> Option<&'a [u8]> {
    fields.iter().find_map(|f| match (f.number, &f.value) {
        (n, WireValue::Bytes(v)) if n == number => Some(*v),
        _ => None,
    })
}
fn number(fields: &[WireField<'_>], number: u32) -> Option<u64> {
    fields.iter().find_map(|f| match (f.number, &f.value) {
        (n, WireValue::Varint(v)) if n == number => Some(*v),
        _ => None,
    })
}

pub fn metrics(seq: u64, timestamp: u64) -> Vec<u8> {
    // Official clients gate tools on Allow, even though no desktop exists. General and
    // Setting sections must be listed (Mac callBackSystemMetrics); no audio or path claim.
    initialization::envelope(
        seq,
        timestamp,
        13,
        &[10, 2, 1, 2, 26, 0, 34, 6, 64, 2, 72, 2, 80, 2],
    )
}
pub fn initial(mut next: impl FnMut() -> u64, timestamp: u64) -> Vec<Vec<u8>> {
    vec![
        initialization::capture_change(next(), timestamp),
        metrics(next(), timestamp),
        initialization::screen_sources(next(), timestamp),
    ]
}

pub fn replies(data: &[u8], mut next: impl FnMut() -> u64, timestamp: u64) -> Vec<Vec<u8>> {
    let Ok(fields) = decode_wire(data) else {
        return Vec::new();
    };
    if let Some(action) = message(&fields, 3).and_then(|b| decode_wire(b).ok()) {
        if number(&action, 1).unwrap_or(0) == 0 {
            let mut echo = vec![8, 1];
            bytes(
                2,
                format!(r#"{{"seq":{}}}"#, number(&fields, 1).unwrap_or(0)).as_bytes(),
                &mut echo,
            );
            // An explicit empty FeatureFlag. In particular no ff_file_transfer_ftp,
            // ff_clipboard, auto-lock, privacy-screen or desktop-capture version.
            bytes(4, &[], &mut echo);
            return vec![initialization::envelope(next(), timestamp, 3, &echo)];
        }
        if number(&action, 1) == Some(15) {
            // Existing official auto-unlock failure code; never parse or retain its password.
            return vec![initialization::report_error(
                next(),
                timestamp,
                -5,
                "headless host has no desktop",
            )];
        }
    }
    if message(&fields, 16)
        .and_then(|b| decode_wire(b).ok())
        .is_some_and(|q| number(&q, 1) == Some(1))
    {
        return vec![initialization::secure_desktop(next(), timestamp)];
    }
    if let Some(rpc) = message(&fields, 21).and_then(|b| decode_wire(b).ok()) {
        if [2, 3, 4, 6].iter().any(|n| message(&rpc, *n).is_some()) {
            // Upstream tool bootstrap accepts settings without starting capture. Keep its
            // response case/header, but report the fixed nominal source, not real encoding.
            let mut response = Vec::new();
            bytes(1, message(&rpc, 1).unwrap_or(&[]), &mut response);
            bytes(2, &[], &mut response);
            let mut sync = Vec::new();
            for (field, value) in [
                (1, 60),
                (2, 5),
                (3, 1),
                (5, 1920),
                (6, 1080),
                (9, 1920),
                (10, 1080),
            ] {
                integer(field, value, &mut sync);
            }
            return vec![
                initialization::envelope(next(), timestamp, 22, &response),
                initialization::envelope(next(), timestamp, 25, &sync),
            ];
        }
    }
    Vec::new()
}

/// Upstream headless peers emitted these observed forward_setting shapes too.
/// Retain the nominal source and legacy labels during the structural refactor; they
/// do not start X11 or an encoder. Removing them needs official-client acceptance.
pub fn legacy_settings(client: &str, ice: &str) -> [serde_json::Value; 2] {
    use serde_json::json;
    let codecs: Vec<_> = [1, 3].into_iter().map(|chroma| json!({
        "video_codec":1,"chroma_sampling":chroma,"codec_impl":5,"bit_depth":8,"width":3840,"height":2160
    })).collect();
    [
        json!({"client_id":client,"data":{"device_capability":{
        "display_info":[{"id":0,"fps":60,"type":0,"hdr":-1}],"video_codec_capability":codecs,"ice_id":ice
    },"type":"device_capability"}}),
        json!({"client_id":client,"data":{"sender_para_info":{
        "capture_impl":"X11","encoder_impl":"Software","ice_id":ice,
        "sender_media_infos":[{"capture_impl":"X11","encoder_impl":"Software","video_track_index":0}],"video_track_index":0
    },"type":"sender_para_info"}}),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn echo_has_no_removed_business_capabilities() {
        let request = initialization::envelope(200, 0, 3, &[8, 0]);
        let reply = replies(&request, || 1, 2).remove(0);
        let fields = decode_wire(&reply).unwrap();
        let action = decode_wire(message(&fields, 3).unwrap()).unwrap();
        assert_eq!(message(&action, 2), Some(br#"{"seq":200}"#.as_slice()));
        assert_eq!(message(&action, 4), Some(&[][..]));
    }
    #[test]
    fn file_terminal_input_power_and_clipboard_are_not_dispatched() {
        for case in [8, 9, 15, 16, 21, 27, 28, 29] {
            let mut rpc = Vec::new();
            bytes(1, &[8, 42], &mut rpc);
            bytes(case, &[], &mut rpc);
            assert!(replies(&initialization::envelope(1, 0, 21, &rpc), || 1, 0).is_empty());
        }
        for action in [2, 4, 9, 10, 12, 13, 14, 16, 18, 19] {
            assert!(replies(&initialization::envelope(1, 0, 3, &[8, action]), || 1, 0).is_empty());
        }
        assert!(replies(br#"{"action":"kbd_press","key":65}"#, || 1, 0).is_empty());
    }
    #[test]
    fn nominal_init_and_correlated_settings_reply_survive() {
        let mut seq = 0;
        let messages = initial(
            || {
                seq += 1;
                seq
            },
            0,
        );
        assert_eq!(
            messages
                .iter()
                .map(|m| decode_wire(m).unwrap().last().unwrap().number)
                .collect::<Vec<_>>(),
            [8, 13, 7]
        );
        let mut rpc = Vec::new();
        bytes(1, &[8, 42], &mut rpc);
        bytes(3, &[8, 2], &mut rpc);
        let reply = replies(&initialization::envelope(1, 0, 21, &rpc), || 1, 0);
        let fields = decode_wire(&reply[0]).unwrap();
        let response = decode_wire(message(&fields, 22).unwrap()).unwrap();
        assert_eq!(message(&response, 1), Some([8, 42].as_slice()));
    }
}
