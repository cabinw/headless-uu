//! Minimal protobuf writers for the verified UU tool handshake.
//! Field layouts come from the upstream official-client protocol observations.
pub(crate) fn varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 128 {
        out.push((value as u8 & 127) | 128);
        value >>= 7;
    }
    out.push(value as u8);
}
pub(crate) fn integer(number: u32, value: u64, out: &mut Vec<u8>) {
    if value != 0 {
        varint(u64::from(number) << 3, out);
        varint(value, out);
    }
}
pub(crate) fn bytes(number: u32, value: &[u8], out: &mut Vec<u8>) {
    varint((u64::from(number) << 3) | 2, out);
    varint(value.len() as u64, out);
    out.extend_from_slice(value);
}
pub fn envelope(seq: u64, timestamp: u64, which: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    integer(1, seq, &mut out);
    integer(2, timestamp, &mut out);
    bytes(which, payload, &mut out);
    out
}

/// Tools require a nominal screen even when no capture backend exists. This is protocol
/// compatibility metadata, not a claim that a physical display or video stream is available.
pub fn screen_sources(seq: u64, timestamp: u64) -> Vec<u8> {
    let mut rect = Vec::new();
    for (field, value) in [(3, 1920), (4, 1080), (5, 1920), (6, 1080)] {
        integer(field, value, &mut rect);
    }
    let mut screen = Vec::new();
    integer(2, 60, &mut screen);
    for field in [3, 4, 6] {
        bytes(field, &rect, &mut screen);
    }
    integer(7, 1, &mut screen);
    screen.push(65);
    screen.extend_from_slice(&1.0f64.to_le_bytes());
    bytes(10, b"Linux desktop", &mut screen);
    let mut sources = Vec::new();
    bytes(1, &screen, &mut sources);
    envelope(seq, timestamp, 7, &sources)
}
pub fn capture_change(seq: u64, timestamp: u64) -> Vec<u8> {
    let mut payload = Vec::new();
    bytes(3, b"Linux desktop", &mut payload);
    envelope(seq, timestamp, 8, &payload)
}
pub fn secure_desktop(seq: u64, timestamp: u64) -> Vec<u8> {
    // All flags false: no lock state, SAS or auto-unlock support.
    let mut payload = Vec::new();
    bytes(1, &[], &mut payload);
    envelope(seq, timestamp, 15, &payload)
}
pub fn report_error(seq: u64, timestamp: u64, code: i32, message: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    integer(2, code as i64 as u64, &mut payload);
    bytes(3, message.as_bytes(), &mut payload);
    envelope(seq, timestamp, 12, &payload)
}
