//! Take items listed by the currently opened corpse, never by a received spawn alone.
//! RoF2 ItemPacketLoot lists corpse slots. LootingItem_Struct is a 20-byte client request;
//! the returned OP_LootItem acknowledges it, while separate inventory packets report acquisition.
//! Native references: eqemu:common/patches/rof2.cpp@5dc093fe5:6466 (SerializeItem),
//! eqemu:common/patches/rof2_structs.h@5dc093fe5:2058 (LootingItem_Struct), and
//! eqemu:zone/corpse.cpp@5dc093fe5:1403 (catalog),
//! eqemu:zone/corpse.cpp@5dc093fe5:1420 (LootCorpseItem).
use crate::{
    item::parse_rof2_item,
    protocol::{OP_ITEM_PACKET, OP_LOOT_ITEM},
    transport::AppPacket,
};
use eqoxide_core::game_state::GameState;
use std::{
    collections::{HashSet, VecDeque},
    time::{Duration, Instant},
};

#[derive(Default)]
pub(crate) struct CorpseLoot {
    session: Option<(u64, u32, u32)>,
    seen: HashSet<u16>,
    pending: VecDeque<u16>,
    in_flight: Option<u16>,
    next_send: Option<Instant>,
    blocked: bool,
}
impl CorpseLoot {
    fn sync(&mut self, gs: &GameState) -> bool {
        let session = (gs.loot_session_active
            && gs.loot_confirmed
            && gs.loot_end_requested_at.is_none()
            && gs.loot_defensive_close_at.is_none()
            && gs.player_id != 0)
            .then(|| {
                gs.loot_current_corpse
                    .map(|id| (gs.zone_epoch, id, gs.player_id))
            })
            .flatten();
        if session != self.session {
            *self = Self {
                session,
                ..Self::default()
            };
        }
        session.is_some() && !self.blocked
    }
    pub(crate) fn observe(&mut self, gs: &mut GameState, packet: &AppPacket, now: Instant) {
        if !self.sync(gs) {
            return;
        }
        if packet.opcode == OP_ITEM_PACKET
            && packet.payload.get(..4) == Some(&0x66u32.to_le_bytes())
        {
            let Some((item, _)) = parse_rof2_item(&packet.payload[4..]) else {
                return;
            };
            // Finite per-session work. Repeated catalog entries never take the same slot twice.
            if item.main_slot != u16::MAX
                && self.seen.len() < 256
                && self.seen.insert(item.main_slot)
            {
                self.pending.push_back(item.main_slot);
            }
            gs.loot_last_activity = Some(now);
        } else if packet.opcode == OP_LOOT_ITEM && packet.payload.len() == 20 {
            let p = &packet.payload;
            let lootee = u32::from_le_bytes(p[0..4].try_into().unwrap());
            let looter = u32::from_le_bytes(p[4..8].try_into().unwrap());
            let slot = u16::from_le_bytes(p[8..10].try_into().unwrap());
            if self.session == Some((gs.zone_epoch, lootee, looter)) && self.in_flight == Some(slot)
            {
                self.in_flight = None;
                self.next_send = Some(now + Duration::from_millis(20));
                gs.loot_last_activity = Some(now);
                if i32::from_le_bytes(p[12..16].try_into().unwrap()) < 0 {
                    self.blocked = true;
                    self.pending.clear();
                }
            }
        }
    }
    /// One request at a time. A lost acknowledgement is never blindly retried; normal loot
    /// inactivity/close deadlines release the session and its outcome stays conservative.
    pub(crate) fn next_request(&mut self, gs: &mut GameState, now: Instant) -> Option<[u8; 20]> {
        if !self.sync(gs) || self.in_flight.is_some() || self.next_send.is_some_and(|at| now < at) {
            return None;
        }
        let slot = self.pending.pop_front()?;
        let (_, corpse, player) = self.session?;
        let mut p = [0; 20];
        p[0..4].copy_from_slice(&corpse.to_le_bytes());
        p[4..8].copy_from_slice(&player.to_le_bytes());
        p[8..10].copy_from_slice(&slot.to_le_bytes());
        p[12..16].copy_from_slice(&1i32.to_le_bytes()); // ordinary server inventory placement
        self.in_flight = Some(slot);
        gs.loot_last_activity = Some(now);
        Some(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn open() -> GameState {
        let mut gs = GameState::new();
        gs.player_id = 7;
        gs.zone_epoch = 1;
        gs.automatic_loot = false;
        gs.loot_session_active = true;
        gs.loot_confirmed = true;
        gs.loot_current_corpse = Some(16);
        gs
    }
    fn catalog(slot: u16) -> AppPacket {
        let mut blob = crate::item::tests::fixture();
        blob[26..28].copy_from_slice(&slot.to_le_bytes());
        let mut p = 0x66u32.to_le_bytes().to_vec();
        p.extend(blob);
        AppPacket {
            opcode: OP_ITEM_PACKET,
            payload: p,
        }
    }
    #[test]
    fn explicit_catalog_takes_once_and_ack_is_not_echoed_or_inventory() {
        let mut gs = open();
        let mut flow = CorpseLoot::default();
        let now = Instant::now();
        let item = catalog(23);
        crate::packet_handler::apply_packet(&mut gs, &item);
        flow.observe(&mut gs, &item, now);
        flow.observe(&mut gs, &item, now);
        let request = flow.next_request(&mut gs, now).unwrap();
        assert_eq!(&request[0..4], &16u32.to_le_bytes());
        assert_eq!(&request[4..8], &7u32.to_le_bytes());
        assert_eq!(&request[8..10], &23u16.to_le_bytes());
        assert_eq!(&request[12..16], &1i32.to_le_bytes());
        assert!(flow
            .next_request(&mut gs, now + Duration::from_secs(1))
            .is_none());
        let ack = AppPacket {
            opcode: OP_LOOT_ITEM,
            payload: request.to_vec(),
        };
        flow.observe(&mut gs, &ack, now);
        assert!(flow
            .next_request(&mut gs, now + Duration::from_secs(1))
            .is_none());
        assert!(
            gs.inventory.is_empty(),
            "catalog and ack do not prove acquisition"
        );
    }
    #[test]
    fn second_item_waits_for_matching_ack_and_cooldown_without_retry() {
        let mut gs = open();
        let mut flow = CorpseLoot::default();
        let now = Instant::now();
        flow.observe(&mut gs, &catalog(23), now);
        flow.observe(&mut gs, &catalog(24), now);
        let first = flow.next_request(&mut gs, now).unwrap();
        assert!(flow
            .next_request(&mut gs, now + Duration::from_secs(8))
            .is_none());
        let mut wrong = first;
        wrong[0..4].copy_from_slice(&17u32.to_le_bytes());
        flow.observe(
            &mut gs,
            &AppPacket {
                opcode: OP_LOOT_ITEM,
                payload: wrong.to_vec(),
            },
            now,
        );
        assert!(flow.next_request(&mut gs, now).is_none());
        flow.observe(
            &mut gs,
            &AppPacket {
                opcode: OP_LOOT_ITEM,
                payload: first.to_vec(),
            },
            now,
        );
        assert!(flow.next_request(&mut gs, now).is_none());
        let second = flow
            .next_request(&mut gs, now + Duration::from_millis(20))
            .unwrap();
        assert_eq!(&second[8..10], &24u16.to_le_bytes());
    }
    #[test]
    fn no_open_session_or_new_epoch_never_dispatches_old_catalog() {
        let mut gs = open();
        let mut flow = CorpseLoot::default();
        let now = Instant::now();
        gs.loot_confirmed = false;
        flow.observe(&mut gs, &catalog(23), now);
        assert!(flow.next_request(&mut gs, now).is_none());
        gs.loot_confirmed = true;
        flow.observe(&mut gs, &catalog(23), now);
        gs.zone_epoch = 2;
        assert!(flow.next_request(&mut gs, now).is_none());
        gs.loot_session_active = false;
        flow.observe(&mut gs, &catalog(24), now);
        assert!(flow.next_request(&mut gs, now).is_none());
    }
}
