//! AudioRelayPlus v1 kablo formatı — bkz. PROTOCOL.md

pub const MAGIC: [u8; 2] = *b"AR";
pub const VERSION: u8 = 1;
pub const DEFAULT_PORT: u16 = 48222;
pub const SAMPLE_RATE: u32 = 48000;
pub const OPUS_FRAME_SAMPLES: usize = 960; // 20 ms
pub const PCM_FRAME_SAMPLES: usize = 480; // 10 ms

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Pcm16 = 0,
    Opus = 1,
}

impl Codec {
    pub fn from_u8(v: u8) -> Option<Codec> {
        match v {
            0 => Some(Codec::Pcm16),
            1 => Some(Codec::Opus),
            _ => None,
        }
    }

    pub fn frame_samples(self) -> usize {
        match self {
            Codec::Pcm16 => PCM_FRAME_SAMPLES,
            Codec::Opus => OPUS_FRAME_SAMPLES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    pub session: u32,
    pub sample_rate: u32,
    pub channels: u8,
    pub codec: Codec,
    pub frame_ms: u8,
}

#[derive(Debug)]
pub enum Packet<'a> {
    Discover { nonce: u32 },
    DiscoverReply { nonce: u32, port: u16, name: String },
    Hello(Hello),
    HelloAck { session: u32 },
    Audio { session: u32, seq: u32, timestamp: u32, payload: &'a [u8] },
    Heartbeat { session: u32, time_ms: u32 },
    HeartbeatAck { session: u32, time_ms: u32 },
    Bye { session: u32 },
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn u16_at(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}

pub fn parse(buf: &[u8]) -> Option<Packet<'_>> {
    if buf.len() < 4 || buf[0..2] != MAGIC || buf[2] != VERSION {
        return None;
    }
    let body = &buf[4..];
    match buf[3] {
        1 if body.len() >= 4 => Some(Packet::Discover { nonce: u32_at(body, 0) }),
        2 if body.len() >= 7 => {
            let name_len = body[6] as usize;
            if body.len() < 7 + name_len {
                return None;
            }
            Some(Packet::DiscoverReply {
                nonce: u32_at(body, 0),
                port: u16_at(body, 4),
                name: String::from_utf8_lossy(&body[7..7 + name_len]).into_owned(),
            })
        }
        3 if body.len() >= 11 => Some(Packet::Hello(Hello {
            session: u32_at(body, 0),
            sample_rate: u32_at(body, 4),
            channels: body[8],
            codec: Codec::from_u8(body[9])?,
            frame_ms: body[10],
        })),
        4 if body.len() >= 4 => Some(Packet::HelloAck { session: u32_at(body, 0) }),
        5 if body.len() >= 12 => Some(Packet::Audio {
            session: u32_at(body, 0),
            seq: u32_at(body, 4),
            timestamp: u32_at(body, 8),
            payload: &body[12..],
        }),
        6 if body.len() >= 8 => Some(Packet::Heartbeat { session: u32_at(body, 0), time_ms: u32_at(body, 4) }),
        7 if body.len() >= 8 => Some(Packet::HeartbeatAck { session: u32_at(body, 0), time_ms: u32_at(body, 4) }),
        8 if body.len() >= 4 => Some(Packet::Bye { session: u32_at(body, 0) }),
        _ => None,
    }
}

fn header(tip: u8, cap: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + cap);
    v.extend_from_slice(&MAGIC);
    v.push(VERSION);
    v.push(tip);
    v
}

pub fn build_discover(nonce: u32) -> Vec<u8> {
    let mut v = header(1, 4);
    v.extend_from_slice(&nonce.to_be_bytes());
    v
}

pub fn build_discover_reply(nonce: u32, port: u16, name: &str) -> Vec<u8> {
    let name = name.as_bytes();
    let name = &name[..name.len().min(255)];
    let mut v = header(2, 7 + name.len());
    v.extend_from_slice(&nonce.to_be_bytes());
    v.extend_from_slice(&port.to_be_bytes());
    v.push(name.len() as u8);
    v.extend_from_slice(name);
    v
}

pub fn build_hello(h: &Hello) -> Vec<u8> {
    let mut v = header(3, 11);
    v.extend_from_slice(&h.session.to_be_bytes());
    v.extend_from_slice(&h.sample_rate.to_be_bytes());
    v.push(h.channels);
    v.push(h.codec as u8);
    v.push(h.frame_ms);
    v
}

pub fn build_hello_ack(session: u32) -> Vec<u8> {
    let mut v = header(4, 4);
    v.extend_from_slice(&session.to_be_bytes());
    v
}

pub fn build_audio(session: u32, seq: u32, timestamp: u32, payload: &[u8]) -> Vec<u8> {
    let mut v = header(5, 12 + payload.len());
    v.extend_from_slice(&session.to_be_bytes());
    v.extend_from_slice(&seq.to_be_bytes());
    v.extend_from_slice(&timestamp.to_be_bytes());
    v.extend_from_slice(payload);
    v
}

pub fn build_heartbeat(session: u32, time_ms: u32) -> Vec<u8> {
    let mut v = header(6, 8);
    v.extend_from_slice(&session.to_be_bytes());
    v.extend_from_slice(&time_ms.to_be_bytes());
    v
}

pub fn build_heartbeat_ack(session: u32, time_ms: u32) -> Vec<u8> {
    let mut v = header(7, 8);
    v.extend_from_slice(&session.to_be_bytes());
    v.extend_from_slice(&time_ms.to_be_bytes());
    v
}

pub fn build_bye(session: u32) -> Vec<u8> {
    let mut v = header(8, 4);
    v.extend_from_slice(&session.to_be_bytes());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_audio() {
        let pkt = build_audio(7, 42, 40320, &[1, 2, 3]);
        match parse(&pkt) {
            Some(Packet::Audio { session: 7, seq: 42, timestamp: 40320, payload }) => {
                assert_eq!(payload, &[1, 2, 3]);
            }
            other => panic!("beklenmeyen: {other:?}"),
        }
    }

    #[test]
    fn roundtrip_hello_and_reply() {
        let h = Hello { session: 9, sample_rate: 48000, channels: 1, codec: Codec::Opus, frame_ms: 20 };
        match parse(&build_hello(&h)) {
            Some(Packet::Hello(g)) => assert_eq!(g, h),
            other => panic!("beklenmeyen: {other:?}"),
        }
        match parse(&build_discover_reply(5, 48222, "Bilgisayarım")) {
            Some(Packet::DiscoverReply { nonce: 5, port: 48222, name }) => {
                assert_eq!(name, "Bilgisayarım");
            }
            other => panic!("beklenmeyen: {other:?}"),
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse(b"XX\x01\x05aaaa").is_none());
        assert!(parse(b"AR\x02\x05aaaa").is_none());
        assert!(parse(b"AR").is_none());
        assert!(parse(&build_hello_ack(1)[..5]).is_none());
    }
}
