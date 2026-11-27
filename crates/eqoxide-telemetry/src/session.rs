//! The headless owner's app-packet history. Each session has its own clock and cursor.

use crate::{opcode_name, Dir};
use eqoxide_protocol::protocol as p;
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Instant,
};

const CAPACITY: usize = 20_000;

#[derive(Clone)]
pub struct SessionTrace {
    started: Instant,
    state: Arc<Mutex<TraceState>>,
}

#[derive(Default)]
struct TraceState {
    packets: VecDeque<SessionPacket>,
    last_n: u64,
    counts: TransportCounts,
}

#[derive(Clone, Copy, Default)]
pub struct TransportCounts {
    pub sent: u64,
    pub resent: u64,
    pub received: u64,
    pub sequence_gaps: u64,
}

#[derive(Clone, Serialize)]
pub struct SessionPacket {
    n: u64,
    at_ms: u64,
    dir: Dir,
    opcode: u16,
    opcode_name: &'static str,
    len: usize,
    reliable: bool,
    wire_seq: Option<u16>,
    #[serde(serialize_with = "serialize_command_id")]
    command_id: Option<Arc<str>>,
    fields: Fields,
}

#[derive(Clone, Serialize)]
#[serde(untagged)]
enum Fields {
    Position {
        position: [f32; 3],
        delta: [f32; 3],
        animation: u32,
        heading: u32,
    },
    Target {
        target_id: u32,
    },
    Damage {
        source: u16,
        target: u16,
        damage: i32,
        spell: u32,
    },
    Empty {},
}

pub struct PacketPage {
    pub packets: Vec<SessionPacket>,
    pub next: u64,
    pub gap: bool,
    pub counts: TransportCounts,
}

impl SessionTrace {
    pub fn new(started: Instant) -> Self {
        Self {
            started,
            state: Arc::default(),
        }
    }

    /// This session-local capture is independent of the graphical telemetry toggle.
    pub fn capture(
        &self,
        dir: Dir,
        opcode: u16,
        payload: &[u8],
        wire_seq: Option<u16>,
        command_id: Option<Arc<str>>,
    ) {
        let at_ms = Instant::now().saturating_duration_since(self.started).as_millis() as u64;
        let fields = decode_fields(dir, opcode, payload);
        let mut state = self.state.lock().unwrap();
        if state.packets.len() == CAPACITY {
            state.packets.pop_front();
        }
        state.last_n += 1;
        let n = state.last_n;
        state.packets.push_back(SessionPacket {
            n,
            at_ms,
            dir,
            opcode,
            opcode_name: opcode_name(opcode),
            len: payload.len() + 2,
            reliable: wire_seq.is_some(),
            wire_seq,
            command_id,
            fields,
        });
    }

    pub fn record_send(&self, retry: bool) {
        let mut state = self.state.lock().unwrap();
        state.counts.sent += 1;
        state.counts.resent += u64::from(retry);
    }

    pub fn record_receive(&self) {
        self.state.lock().unwrap().counts.received += 1;
    }

    pub fn record_sequence_gap(&self) {
        self.state.lock().unwrap().counts.sequence_gaps += 1;
    }

    pub fn page(
        &self,
        after: u64,
        limit: usize,
        opcode: Option<&str>,
        command: Option<&str>,
    ) -> PacketPage {
        let state = self.state.lock().unwrap();
        let evicted_through = state.last_n - state.packets.len() as u64;
        let skip = (after.min(state.last_n).saturating_sub(evicted_through)) as usize;
        let packets: Vec<_> = state.packets.iter()
            .skip(skip)
            .filter(|packet| opcode.is_none_or(|name| packet.opcode_name == name))
            .filter(|packet| command.is_none_or(|id| packet.command_id.as_deref() == Some(id)))
            .take(limit)
            .cloned()
            .collect();
        // A short page scanned the whole tail, even if every record was filtered out.
        let next = if packets.len() == limit {
            packets.last().map_or(after, |packet| packet.n)
        } else {
            after.max(state.last_n)
        };
        PacketPage {
            packets,
            next,
            gap: after < evicted_through,
            counts: state.counts,
        }
    }
}

fn serialize_command_id<S: serde::Serializer>(
    id: &Option<Arc<str>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    id.as_deref().serialize(serializer)
}

fn decode_fields(dir: Dir, opcode: u16, body: &[u8]) -> Fields {
    // These are the wire layouts used by encode_client_position_update, build_target_packet,
    // and apply_combat_damage. Length checks keep truncated packets visible without fake fields.
    let u32_at = |i| u32::from_le_bytes(body[i..i + 4].try_into().unwrap());
    let u16_at = |i| u16::from_le_bytes(body[i..i + 2].try_into().unwrap());
    let f32_at = |i| f32::from_bits(u32_at(i));
    match (dir, opcode) {
        (Dir::Out, p::OP_CLIENT_UPDATE) if body.len() >= 46 => Fields::Position {
            position: [f32_at(18), f32_at(30), f32_at(26)],
            delta: [f32_at(10), f32_at(38), f32_at(22)],
            animation: u32_at(34) & 0x3ff,
            heading: u32_at(14) & 0xfff,
        },
        (Dir::Out, p::OP_TARGET_COMMAND | p::OP_TARGET_MOUSE) if body.len() >= 4 => {
            Fields::Target { target_id: u32_at(0) }
        }
        (Dir::In, p::OP_DAMAGE) if body.len() >= 13 => Fields::Damage {
            source: u16_at(2),
            target: u16_at(0),
            damage: u32_at(9) as i32,
            spell: u32_at(5),
        },
        _ => Fields::Empty {},
    }
}
