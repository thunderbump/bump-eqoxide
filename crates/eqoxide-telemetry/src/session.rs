//! The headless owner's app-packet history. Each session has its own clock and cursor.

use crate::{opcode_name, Dir};
use eqoxide_protocol::protocol as p;
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Instant,
};

pub const SESSION_PACKET_CAPACITY: usize = 20_000;
const PAYLOAD_CAPACITY: usize = 512;

#[derive(Clone, Copy)]
pub struct PacketArrival {
    pub n: u64,
    pub at_ms: u64,
}

#[derive(Default)]
pub struct ReceiveEvidence {
    pub arrival: Option<PacketArrival>,
    pub acked_at: Option<Instant>,
}

#[derive(Clone)]
pub struct SessionTrace {
    started: Instant,
    state: Arc<Mutex<TraceState>>,
}

#[derive(Default)]
struct TraceState {
    packets: VecDeque<Arc<SessionPacket>>,
    last_n: u64,
    counts: TransportCounts,
}

#[derive(Clone, Copy, Default)]
pub struct TransportCounts {
    pub sent: u64,
    pub resent: u64,
    pub received: u64,
    pub sequence_gaps: u64,
    pub dropped_unretried: u64,
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
    pub arrival_n: Option<u64>,
    pub arrived_at_ms: Option<u64>,
    pub acked_at_ms: Option<u64>,
    pub ack_seq: Option<u16>,
    pub ack_exact: bool,
    #[serde(serialize_with = "serialize_command_id")]
    command_id: Option<Arc<str>>,
    fields: Fields,
    payload_truncated: bool,
    #[serde(skip)]
    payload: Arc<[u8]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload_hex: Option<String>,
}

impl SessionPacket {
    pub fn include_raw_payload(&mut self) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut hex = String::with_capacity(self.payload.len() * 2);
        for &byte in self.payload.iter() {
            hex.push(HEX[(byte >> 4) as usize] as char);
            hex.push(HEX[(byte & 0xf) as usize] as char);
        }
        self.payload_hex = Some(hex);
    }

    fn set_ack(&mut self, sequence: u16, at_ms: u64) {
        if self.acked_at_ms.is_none() {
            self.acked_at_ms = Some(at_ms);
            self.ack_seq = Some(sequence);
            self.ack_exact = self.wire_seq == Some(sequence);
        }
    }
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
    Spawn {
        entity: u32,
        name: String,
        position: [f32; 3],
        heading: u32,
    },
    InboundPosition {
        entity: u16,
        position: [f32; 3],
        heading: u32,
    },
    Entity {
        entity: u32,
    },
    Health {
        entity: u16,
        percent: u8,
    },
    Target {
        target_id: u32,
    },
    Damage {
        source: u16,
        target: u16,
        #[serde(rename = "type")]
        damage_type: u8,
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
        self.capture_with_receive(dir, opcode, payload, wire_seq, command_id, ReceiveEvidence::default());
    }

    pub fn capture_with_receive(
        &self,
        dir: Dir,
        opcode: u16,
        payload: &[u8],
        wire_seq: Option<u16>,
        command_id: Option<Arc<str>>,
        receive: ReceiveEvidence,
    ) -> u64 {
        let at_ms = self.clock_ms(Instant::now());
        let fields = decode_fields(dir, opcode, payload);
        let prefix = Arc::from(&payload[..payload.len().min(PAYLOAD_CAPACITY)]);
        let mut state = self.state.lock().unwrap();
        if state.packets.len() == SESSION_PACKET_CAPACITY {
            state.packets.pop_front();
        }
        state.last_n += 1;
        let n = state.last_n;
        let ack = receive.acked_at.zip(wire_seq);
        state.packets.push_back(Arc::new(SessionPacket {
            n,
            at_ms,
            dir,
            opcode,
            opcode_name: opcode_name(opcode),
            len: payload.len() + 2,
            reliable: wire_seq.is_some(),
            wire_seq,
            arrival_n: receive.arrival.map(|arrival| arrival.n),
            arrived_at_ms: receive.arrival.map(|arrival| arrival.at_ms),
            acked_at_ms: ack.map(|(at, _)| self.clock_ms(at)),
            ack_seq: ack.map(|(_, seq)| seq),
            ack_exact: ack.is_some(),
            command_id,
            fields,
            payload_truncated: payload.len() > PAYLOAD_CAPACITY,
            payload: prefix,
            payload_hex: None,
        }));
        n
    }

    fn clock_ms(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.started).as_millis() as u64
    }

    /// Resolve ACKs that were still queued when their application rows were captured. Replacing
    /// shared rows preserves the snapshot an HTTP reader may already be serializing.
    pub fn mark_acked(&self, rows: impl Iterator<Item = u64>, sequence: u16, at: Instant) {
        let at_ms = self.clock_ms(at);
        let mut state = self.state.lock().unwrap();
        let first = state.last_n - state.packets.len() as u64 + 1;
        for n in rows {
            if let Some(index) = n.checked_sub(first) {
                if let Some(packet) = state.packets.get_mut(index as usize) {
                    Arc::make_mut(packet).set_ack(sequence, at_ms);
                }
            }
        }
    }

    pub fn record_send(&self, retry: bool) {
        let mut state = self.state.lock().unwrap();
        state.counts.sent += 1;
        state.counts.resent += u64::from(retry);
    }

    pub fn record_drop(&self, count: u64) {
        // Stream teardown also runs during unwinding.
        if let Ok(mut state) = self.state.lock() {
            state.counts.dropped_unretried += count;
        }
    }

    pub fn record_receive(&self, at: Instant) -> PacketArrival {
        let mut state = self.state.lock().unwrap();
        state.counts.received += 1;
        PacketArrival { n: state.counts.received, at_ms: self.clock_ms(at) }
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
        let (tail, last_n, gap, counts) = {
            let state = self.state.lock().unwrap();
            let evicted_through = state.last_n - state.packets.len() as u64;
            let start = (after.min(state.last_n).saturating_sub(evicted_through)) as usize;
            let end = if opcode.is_none() && command.is_none() {
                start.saturating_add(limit).min(state.packets.len())
            } else {
                state.packets.len()
            };
            // Copy record handles and counters together. Filtering and packet copies run
            // after releasing the lock so a sparse match cannot block the network owner.
            let tail: Vec<_> = state.packets.range(start..end).cloned().collect();
            (tail, state.last_n, after < evicted_through, state.counts)
        };
        let packets: Vec<_> = tail.into_iter()
            .filter(|packet| opcode.is_none_or(|name| packet.opcode_name == name))
            .filter(|packet| command.is_none_or(|id| packet.command_id.as_deref() == Some(id)))
            .take(limit)
            .map(|packet| (*packet).clone())
            .collect();
        // A short page scanned the whole tail, even if every record was filtered out.
        let next = if packets.len() == limit {
            packets.last().map_or(after, |packet| packet.n)
        } else {
            after.max(last_n)
        };
        PacketPage {
            packets,
            next,
            gap,
            counts,
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
        (Dir::In, p::OP_ZONE_ENTRY) => {
            if let Some((spawn, _, heading)) = p::parse_rof2_spawn_with_wire_heading(body) {
                // The wire reader returns ASCII names. Cap malformed long names so decoded
                // metadata cannot bypass the trace's per-row byte bound.
                let name = if spawn.name.len() <= PAYLOAD_CAPACITY {
                    spawn.name
                } else {
                    spawn.name[..PAYLOAD_CAPACITY].to_owned()
                };
                Fields::Spawn {
                    entity: spawn.spawn_id,
                    name,
                    position: [spawn.x, spawn.y, spawn.z],
                    heading,
                }
            } else {
                Fields::Empty {}
            }
        }
        (Dir::In, p::OP_CLIENT_UPDATE) => {
            if let Some((position, heading)) = p::decode_position_update_with_wire_heading(body) {
                Fields::InboundPosition {
                    entity: position.spawn_id,
                    position: [position.x, position.y, position.z],
                    heading,
                }
            } else {
                Fields::Empty {}
            }
        }
        (Dir::In, p::OP_DELETE_SPAWN) if body.len() >= 4 => Fields::Entity {
            entity: u32_at(0),
        },
        (Dir::In, p::OP_MOB_HEALTH) if body.len() >= p::SIZE_MOB_HEALTH => Fields::Health {
            entity: u16_at(0),
            percent: body[2],
        },
        (_, p::OP_TARGET_COMMAND) | (Dir::Out, p::OP_TARGET_MOUSE) if body.len() >= 4 => {
            Fields::Target { target_id: u32_at(0) }
        }
        (Dir::In, p::OP_DAMAGE) if body.len() >= 13 => Fields::Damage {
            source: u16_at(2),
            target: u16_at(0),
            damage_type: body[4],
            damage: u32_at(9) as i32,
            spell: u32_at(5),
        },
        _ => Fields::Empty {},
    }
}
