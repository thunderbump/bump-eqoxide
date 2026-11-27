use super::{tests::{floor, host_with_wiring}, GameplayLifecycle, Host, Status, Wiring};
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use eqoxide_core::game_state::{make_entity, GameState, InvItem, MerchantItem};
use eqoxide_net::{
    action_loop::ActionLoop,
    packet_telemetry::SessionTrace,
    protocol::*,
    testkit::{drain_gameplay_commands, test_stream_with_peer, LootFixture},
    transport::{with_packet_trace, EqStream},
};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tower::ServiceExt;

struct Session {
    host: Host,
    gs: GameState,
    action_loop: ActionLoop,
    stream: EqStream,
    lifecycle: GameplayLifecycle,
    _peer: UdpSocket,
}

impl Session {
    async fn new() -> Self {
        let wiring = Wiring::new("/unused".into());
        let mut host = host_with_wiring(&wiring);
        let mut gs = GameState::new();
        gs.zone_epoch = host.epoch;
        gs.player_id = 1;
        gs.upsert_entity(make_entity(7, "a rat", 0.0, 0.0, 0.0, true));
        gs.target_id = Some(7);
        gs.inventory_received = true;
        gs.inventory.push(InvItem {
            slot: 23, item_id: 11, name: "bone chips".into(), icon: 0, charges: 1,
            idfile: String::new(), click_spell_id: 0, filename: String::new(),
        });
        gs.merchant_open = Some(8);
        gs.merchant_items.push(MerchantItem { merchant_slot: 2, ..Default::default() });
        gs.upsert_entity(make_entity(8, "merchant", 0.0, 0.0, 0.0, true));
        host.update_knowledge(&gs);
        let trace = SessionTrace::new(Instant::now());
        host.control = host.control.with_packet_trace(trace.clone());
        host.control.publish_state(host.epoch, json!({"phase":"ready"})).unwrap();
        host.control.set_ready(true);
        let (stream, _, peer, _) = with_packet_trace(trace, test_stream_with_peer(wiring.health)).await;
        Self {
            host,
            gs,
            action_loop: ActionLoop::new(wiring.slots),
            stream,
            lifecycle: wiring.lifecycle,
            _peer: peer,
        }
    }

    async fn dispatch(&mut self, id: &str, operation: Value) {
        let response = self.host.control.router().oneshot(
            Request::put(format!("/v2/player/commands/{id}"))
                .header("authorization", "Bearer test-credential")
                .header("content-type", "application/json")
                .body(Body::from(json!({
                    "session": "test-session",
                    "zone_epoch": self.host.epoch,
                    "inventory_revision": self.host.inventory_revision,
                    "merchant_revision": self.host.merchant_revision,
                    "operation": operation,
                }).to_string()))
                .unwrap(),
        ).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let commands = self.host.control.drain_actions();
        assert_eq!(commands.len(), 1);
        for command in commands {
            self.host.dispatch(command, &self.gs, true, Instant::now()).await;
        }
    }

    fn drain(&mut self) {
        drain_gameplay_commands(
            &mut self.action_loop, &mut self.stream, &mut self.gs, &self.lifecycle,
        );
    }

    async fn packets(&self, query: &str) -> Value {
        let response = self.host.control.router().oneshot(
            Request::get(format!("/v2/player/packets?{query}"))
                .header("authorization", "Bearer test-credential")
                .body(Body::empty())
                .unwrap(),
        ).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 65_536).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn assert_packets(&self, id: &str, expected: &[u16]) {
        let trace = self.packets(&format!("command={id}")).await;
        let opcodes: Vec<_> = trace["packets"].as_array().unwrap().iter().map(|packet| {
            assert_eq!(packet["dir"], "out");
            assert_eq!(packet["command_id"], id);
            packet["opcode"].as_u64().unwrap() as u16
        }).collect();
        assert_eq!(opcodes, expected, "{id}: {trace}");
    }
}

/// Removing the generic dispatch tag or either target-drain tag loses a row for this command.
#[tokio::test]
async fn target_owns_target_and_consider_packets() {
    let mut session = Session::new().await;
    session.dispatch("target", json!({"kind":"target","entity_id":7})).await;
    session.drain();
    session.assert_packets("target", &[OP_TARGET_MOUSE, OP_CONSIDER]).await;
}

/// Passing no ID from the attack drain makes the filtered page empty.
#[tokio::test]
async fn attack_owns_auto_attack_packet() {
    let mut session = Session::new().await;
    session.dispatch("attack", json!({"kind":"attack","lease_ms":1000})).await;
    session.drain();
    session.assert_packets("attack", &[OP_AUTO_ATTACK]).await;
}

/// Dropping either the safety-off tag or the sit tag loses one of these two rows.
#[tokio::test]
async fn sit_owns_attack_off_and_appearance_packets() {
    let mut session = Session::new().await;
    session.dispatch("sit", json!({"kind":"sit"})).await;
    session.drain();
    session.assert_packets("sit", &[OP_AUTO_ATTACK, OP_SPAWN_APPEARANCE]).await;
}

/// Queuing attack-off before reserving the busy sit slot emits a packet for a refused command.
#[tokio::test]
async fn refused_sit_owns_no_packets() {
    let mut session = Session::new().await;
    session.gs.auto_attack = true;
    session.gs.sitting = true;
    // Occupy the slot independently of the journal's one-command admission rule.
    assert!(session.host.command.request_sit(false));
    session.dispatch("sit", json!({"kind":"sit"})).await;
    assert_eq!(session.host.control.command("sit").unwrap().status, Status::Refused);
    session.drain();
    session.assert_packets("sit", &[]).await;
    assert!(!session.gs.sitting, "the original stand request should drain");
    assert!(session.gs.auto_attack, "a refused sit must leave the attack mailbox alone");
}

/// Passing no ID to the channel-message send makes the filtered page empty.
#[tokio::test]
async fn say_owns_channel_message_packet() {
    let mut session = Session::new().await;
    session.dispatch("say", json!({"kind":"say","text":"hello"})).await;
    session.drain();
    session.assert_packets("say", &[OP_CHANNEL_MESSAGE]).await;
}

/// Passing no sell ID to either merchant send loses that row from the command page.
#[tokio::test]
async fn sell_owns_shop_request_and_sell_packets() {
    let mut session = Session::new().await;
    session.dispatch("sell", json!({"kind":"sell","slot":23,"quantity":1})).await;
    session.drain();
    session.assert_packets("sell", &[OP_SHOP_REQUEST, OP_SHOP_PLAYER_SELL]).await;
}

/// Removing the awaited-buy dispatch tag or passing None to send_shop_buy loses the pair.
#[tokio::test]
async fn awaited_buy_owns_shop_request_and_buy_packets() {
    let mut session = Session::new().await;
    session.dispatch("buy", json!({"kind":"buy","slot":2,"quantity":1})).await;
    session.drain();
    session.assert_packets("buy", &[OP_SHOP_REQUEST, OP_SHOP_PLAYER_BUY]).await;
}

/// Dropping the awaited-open tag makes an open disappear from the command page.
#[tokio::test]
async fn awaited_open_owns_shop_request_packet() {
    let mut session = Session::new().await;
    session.dispatch("open", json!({"kind":"merchant_open","entity_id":8})).await;
    session.drain();
    session.assert_packets("open", &[OP_SHOP_REQUEST]).await;
}

/// Passing None to either the camp mailbox write or halt loses camp or attack-off rows.
#[tokio::test]
async fn camp_owns_camp_appearance_and_attack_off_packets() {
    let mut session = Session::new().await;
    session.gs.auto_attack = true;
    session.dispatch("camp", json!({"kind":"camp"})).await;
    session.drain();
    session.assert_packets("camp", &[OP_CAMP, OP_SPAWN_APPEARANCE, OP_AUTO_ATTACK]).await;
    assert!(!session.gs.auto_attack);
}


/// A stand request must retain its dispatch ID through the shared sit slot.
#[tokio::test]
async fn stand_owns_appearance_packet() {
    let mut session = Session::new().await;
    session.gs.sitting = true;
    session.dispatch("stand", json!({"kind":"stand"})).await;
    session.drain();
    session.assert_packets("stand", &[OP_SPAWN_APPEARANCE]).await;
    assert!(!session.gs.sitting);
}

/// Removing the dispatch tag loses this tell's identity in the FIFO mailbox.
#[tokio::test]
async fn tell_owns_channel_message_packet() {
    let mut session = Session::new().await;
    session.dispatch("tell", json!({"kind":"tell","recipient":"Peer","text":"hello"})).await;
    session.drain();
    session.assert_packets("tell", &[OP_CHANNEL_MESSAGE]).await;
}

/// Dispatch must tag the merchant close even though it reuses the open packet opcode.
#[tokio::test]
async fn merchant_close_owns_shop_request_packet() {
    let mut session = Session::new().await;
    session.dispatch("close", json!({"kind":"merchant_close"})).await;
    session.drain();
    session.assert_packets("close", &[OP_SHOP_REQUEST]).await;
    assert!(session.gs.merchant_open.is_none());
}

/// Passing None to the priority attack-off write loses the command's only trace row.
#[tokio::test]
async fn attack_off_owns_auto_attack_packet() {
    let mut session = Session::new().await;
    session.gs.auto_attack = true;
    session.dispatch("off", json!({"kind":"attack_off"})).await;
    session.drain();
    session.assert_packets("off", &[OP_AUTO_ATTACK]).await;
    assert!(!session.gs.auto_attack);
}

/// Removing the dispatch tag from inventory move loses the emitted move packet.
#[tokio::test]
async fn inventory_move_owns_move_item_packet() {
    let mut session = Session::new().await;
    session.dispatch("move", json!({"kind":"inventory_move","from_slot":23,"to_slot":24,"quantity":1})).await;
    session.drain();
    session.assert_packets("move", &[OP_MOVE_ITEM]).await;
}

/// Dropping the ID at normal close loses the third row. A real close ACK must release the
/// command before the next, automatic entry opens the same corpse.
#[tokio::test]
async fn explicit_loot_owns_open_take_and_close() {
    let mut session = Session::new().await;
    let mut corpse = make_entity(9, "a corpse", 0.0, 0.0, 0.0, true);
    corpse.dead = true;
    session.gs.upsert_entity(corpse);
    session.gs.automatic_loot = false;
    session.dispatch("loot-1", json!({"kind":"loot","entity_id":9})).await;
    session.drain();
    session.gs.loot_queued_at = Some(Instant::now() - Duration::from_secs(1));
    let mut loot = LootFixture::default();
    loot.tick(&mut session.stream, &mut session.gs);

    let mut open = vec![0; 20];
    open[0] = 1;
    loot.receive(&mut session.gs, OP_MONEY_ON_CORPSE, open.clone());
    loot.receive_catalog_item(&mut session.gs);
    loot.tick(&mut session.stream, &mut session.gs);
    let mut item_ack = vec![0; 20];
    item_ack[..4].copy_from_slice(&9u32.to_le_bytes());
    item_ack[4..8].copy_from_slice(&session.gs.player_id.to_le_bytes());
    item_ack[8..10].copy_from_slice(&23u16.to_le_bytes());
    item_ack[12..16].copy_from_slice(&1i32.to_le_bytes());
    loot.receive(&mut session.gs, OP_LOOT_ITEM, item_ack);
    session.gs.loot_last_activity = Some(Instant::now() - Duration::from_secs(3));
    loot.tick(&mut session.stream, &mut session.gs);
    assert!(session.gs.loot_end_requested_at.is_some());
    let expected = [OP_LOOT_REQUEST, OP_LOOT_ITEM, OP_END_LOOT_REQUEST];
    session.assert_packets("loot-1", &expected).await;

    // Exercise the genuine close path. The next queue entry opens the same corpse automatically.
    loot.receive(&mut session.gs, OP_LOOT_COMPLETE, vec![]);
    assert!(!session.gs.loot_session_active);
    assert!(session.gs.loot_end_requested_at.is_none());
    session.gs.pending_loot.push_back(9.into());
    session.gs.loot_queued_at = Some(Instant::now() - Duration::from_secs(1));
    loot.tick(&mut session.stream, &mut session.gs);
    loot.receive(&mut session.gs, OP_MONEY_ON_CORPSE, open);
    loot.receive_catalog_item(&mut session.gs);
    loot.tick(&mut session.stream, &mut session.gs);
    session.assert_packets("loot-1", &expected).await;
    let trace = session.packets("opcode=OP_LOOT_REQUEST").await;
    let rows = trace["packets"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["command_id"], Value::Null);
    assert!(session.gs.loot_session_active);
    assert!(session.gs.loot_command_id.is_none());
}

/// Dropping the ID on the defensive close loses the packet triggered by the open timeout.
#[tokio::test]
async fn explicit_loot_owns_defensive_close_after_open_timeout() {
    let mut session = Session::new().await;
    let mut corpse = make_entity(9, "a corpse", 0.0, 0.0, 0.0, true);
    corpse.dead = true;
    session.gs.upsert_entity(corpse);
    session.dispatch("loot-timeout", json!({"kind":"loot","entity_id":9})).await;
    session.drain();
    session.gs.loot_queued_at = Some(Instant::now() - Duration::from_secs(1));
    let mut loot = LootFixture::default();
    loot.tick(&mut session.stream, &mut session.gs);
    assert!(session.gs.loot_session_active);
    assert!(!session.gs.loot_confirmed);

    session.gs.loot_last_activity = Some(Instant::now() - Duration::from_secs(6));
    loot.tick(&mut session.stream, &mut session.gs);
    assert!(session.gs.loot_defensive_close_at.is_some());
    session.assert_packets("loot-timeout", &[OP_LOOT_REQUEST, OP_END_LOOT_REQUEST]).await;
}

/// The physics publication, moving sender, and expiry/rest path all carry the move ID.
#[tokio::test]
async fn move_for_owns_moving_and_rest_updates() {
    use eqoxide_core::game_state::PositionSendReason;

    let mut session = Session::new().await;
    let floor = floor();
    session.gs.world.zone_name = "synthetic".into();
    session.host.step(&session.gs, Some(&floor), true, Instant::now(), 0.02);
    session.drain();

    session.dispatch("move", json!({"kind":"move_for","east":1,"north":0,"duration_ms":2000})).await;
    let until = session.host.motion.as_ref().unwrap().until;
    tokio::time::sleep(Duration::from_millis(300)).await;
    session.host.step(&session.gs, Some(&floor), true, Instant::now(), 0.05);
    session.drain();
    let last_send = || session.host.controller_slots.controller_view.lock().unwrap().last_position_send.unwrap();
    assert_eq!(last_send().reason, PositionSendReason::Moving);

    session.host.step(&session.gs, Some(&floor), true, until, 0.02);
    session.drain();
    assert_eq!(
        session.host.controller_slots.controller_view.lock().unwrap().last_position_send.unwrap().reason,
        PositionSendReason::Rest,
    );
    session.assert_packets("move", &[OP_CLIENT_UPDATE, OP_CLIENT_UPDATE]).await;
}

/// A stationary active command owns a keepalive, while a correction re-stream stays untagged.
#[tokio::test]
async fn active_motion_owns_keepalive_but_not_correction() {
    use eqoxide_core::game_state::PositionSendReason;

    let mut session = Session::new().await;
    let floor = floor();
    session.gs.world.zone_name = "synthetic".into();
    session.host.step(&session.gs, Some(&floor), true, Instant::now(), 0.02);
    session.drain();
    session.dispatch("wait-move", json!({"kind":"move_for","east":0,"north":0,"duration_ms":2000})).await;
    session.host.step(&session.gs, Some(&floor), true, Instant::now(), 0.02);
    session.drain();
    tokio::time::sleep(Duration::from_millis(1320)).await;
    session.drain();
    assert_eq!(
        session.host.controller_slots.controller_view.lock().unwrap().last_position_send.unwrap().reason,
        PositionSendReason::Keepalive,
    );
    session.gs.player_x += 30.0;
    session.drain();
    let trace = session.packets("opcode=OP_CLIENT_UPDATE").await;
    assert_eq!(trace["packets"].as_array().unwrap().len(), 2);
    assert_eq!(trace["packets"][1]["command_id"], Value::Null);
    session.assert_packets("wait-move", &[OP_CLIENT_UPDATE]).await;
}

/// If the network skips steps from two move commands, their joint update has no single owner.
#[tokio::test]
async fn position_window_with_two_commands_is_unattributed() {
    let mut session = Session::new().await;
    let floor = floor();
    session.gs.world.zone_name = "synthetic".into();
    session.host.step(&session.gs, Some(&floor), true, Instant::now(), 0.02);
    session.drain();

    for (id, east, north) in [("east", 1, 0), ("north", 0, 1)] {
        session.dispatch(id, json!({"kind":"move_for","east":east,"north":north,"duration_ms":2000})).await;
        session.host.step(&session.gs, Some(&floor), true, Instant::now(), 0.05);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    session.drain();
    let trace = session.packets("opcode=OP_CLIENT_UPDATE").await;
    assert_eq!(trace["packets"].as_array().unwrap().len(), 1);
    assert_eq!(trace["packets"][0]["command_id"], Value::Null);
}


/// Removing the adoption reset makes the first post-correction send look like mixed provenance.
#[tokio::test]
async fn correction_adoption_discards_late_pre_correction_provenance() {
    let mut session = Session::new().await;
    let floor = floor();
    session.gs.world.zone_name = "synthetic".into();
    session.host.step(&session.gs, Some(&floor), true, Instant::now(), 0.02);
    session.drain();

    session.dispatch("before", json!({"kind":"move_for","east":1,"north":0,"duration_ms":2000})).await;
    session.host.step(&session.gs, Some(&floor), true, Instant::now(), 0.05);
    let slots = session.host.controller_slots.clone();
    let late_publication = std::mem::take(&mut slots.controller_view.lock().unwrap().position_commands);
    assert_eq!(late_publication.command_id().map(|id| id.as_ref()), Some("before"));

    // Replay the race deterministically. The network snapshot missed A's publication, but it
    // is in the shared view by the time B takes the correction.
    session.gs.player_x = 30.0;
    session.drain();
    assert!(slots.pos_correction.lock().unwrap().is_some());
    slots.controller_view.lock().unwrap().position_commands = late_publication;
    session.dispatch("after", json!({"kind":"move_for","east":0,"north":1,"duration_ms":2000})).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    session.host.step(&session.gs, Some(&floor), true, Instant::now(), 0.05);
    session.drain();

    session.assert_packets("before", &[]).await;
    session.assert_packets("after", &[OP_CLIENT_UPDATE]).await;
    let trace = session.packets("opcode=OP_CLIENT_UPDATE").await;
    let rows = trace["packets"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["command_id"], Value::Null);
}
