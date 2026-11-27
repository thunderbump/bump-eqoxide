use super::{tests::host_with_wiring, GameplayLifecycle, Host, Status, Wiring};
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use eqoxide_core::game_state::{make_entity, GameState, InvItem, MerchantItem};
use eqoxide_net::{
    action_loop::ActionLoop,
    packet_telemetry::SessionTrace,
    protocol::*,
    testkit::{drain_gameplay_commands, test_stream_with_peer},
    transport::{with_packet_trace, EqStream},
};
use serde_json::{json, Value};
use std::time::Instant;
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
        host.control = host.control.with_packet_trace(trace.clone(), wiring.health.clone());
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

    async fn assert_packets(&self, id: &str, expected: &[u16]) {
        let response = self.host.control.router().oneshot(
            Request::get(format!("/v2/player/packets?command={id}"))
                .header("authorization", "Bearer test-credential")
                .body(Body::empty())
                .unwrap(),
        ).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 65_536).await.unwrap();
        let trace: Value = serde_json::from_slice(&bytes).unwrap();
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
