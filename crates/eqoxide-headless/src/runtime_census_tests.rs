//! Outbound census: a scripted session through the real host frame (`Host::frame`, the run loop's
//! per-tick body) and the real network action loop, counting every outbound application packet in
//! the session trace by opcode. A handoff's "wire impact" statement cites this test's output.
//!
//! The network loop ticks twice per 20 ms host tick, as it does live (10 ms and 20 ms). Position
//! updates are classified as rest (zero delta, animation 0) or moving; their keepalive and throttle
//! cadence is wall-clock, so only the rest count is asserted exactly.
//!
//! Print the census with `cargo test -p eqoxide-headless census -- --nocapture`.

use super::{tests::{floor, host_with_wiring}, Frame, GameplayLifecycle, Host, Link, Status, Wiring};
use axum::{body::Body, http::{Request, StatusCode}};
use eqoxide_core::game_state::{make_entity, GameState};
use eqoxide_net::{
    action_loop::ActionLoop,
    packet_telemetry::SessionTrace,
    testkit::{drain_gameplay_commands, test_stream_with_peer},
    transport::{with_packet_trace, EqStream},
};
use eqoxide_zone_geometry::collision::SharedCollision;
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc, time::{Duration, Instant}};
use tokio::net::UdpSocket;
use tower::ServiceExt;

const REST: &str = "OP_CLIENT_UPDATE rest";
const MOVING: &str = "OP_CLIENT_UPDATE moving";

struct Census {
    host: Host,
    gs: GameState,
    action_loop: ActionLoop,
    stream: EqStream,
    lifecycle: GameplayLifecycle,
    controller: eqoxide_ipc::ControllerSlots,
    collision: SharedCollision,
    trace: SessionTrace,
    frame: Frame,
    started: Instant,
    read: u64,
    ready: bool,
    _peer: UdpSocket,
}

impl Census {
    /// The host as `run` builds it: no body, epoch 0, nothing published.
    async fn new() -> Self {
        let wiring = Wiring::new("/unused".into());
        let mut host = host_with_wiring(&wiring);
        host.body = None;
        host.epoch = 0;
        let started = Instant::now();
        let trace = SessionTrace::new(started);
        host.control = host.control.with_packet_trace(trace.clone());
        let (stream, _, peer, _) =
            with_packet_trace(trace.clone(), test_stream_with_peer(wiring.health)).await;
        Self {
            host,
            gs: GameState::new(),
            action_loop: ActionLoop::new(wiring.slots),
            stream,
            lifecycle: wiring.lifecycle,
            controller: wiring.controller,
            collision: wiring.collision,
            trace,
            frame: Frame::new(started),
            started,
            read: 0,
            ready: false,
            _peer: peer,
        }
    }

    /// One 20 ms host tick in run-loop order, with two 10 ms network ticks before it.
    async fn tick(&mut self) {
        for _ in 0..2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            drain_gameplay_commands(
                &mut self.action_loop, &mut self.stream, &mut self.gs, &self.lifecycle,
            );
        }
        let snapshot = self.gs.clone();
        let now = Instant::now();
        self.host.drain_completions();
        let link = Link { tick_age: 0, inbound_age: 0, link_age: 0, dropped: false };
        let ready = self
            .host
            .frame(
                &snapshot, &mut self.frame, &None, false, link, now, 0.02, self.started,
                &self.collision,
                &mut |_| Ok((Arc::new(floor()), json!({"status":"loaded"}))),
            )
            .unwrap();
        self.ready = ready;
        for command in self.host.control.drain_actions() {
            self.host.dispatch(command, &snapshot, ready, now).await;
        }
    }

    async fn ticks(&mut self, n: usize) {
        for _ in 0..n {
            self.tick().await;
        }
    }

    /// A zone entry in the order native sends it: the network owner begins the zone-in, the self
    /// spawn arrives, a few ticks pass in the handshake, then `OP_NewZone`.
    async fn enter_zone(&mut self, zone: &str) {
        self.controller.begin_zone_in(&mut self.gs);
        self.gs.player_id = 1;
        self.gs.player_name = "Census".into();
        [self.gs.player_x, self.gs.player_y, self.gs.player_z] = [0.0, 0.0, 0.0];
        self.gs.player_pos_known = true;
        self.gs.cur_hp = 100;
        self.gs.max_hp = 100;
        self.ticks(5).await;
        self.gs.world.zone_name = zone.into();
        self.gs.world.new_zone_applied = true;
        self.gs.upsert_entity(make_entity(7, "a rat", 5.0, 0.0, 0.0, true));
        self.gs.target_id = Some(7);
        // Geometry, body, the first publish, then ready.
        self.ticks(10).await;
        assert!(self.ready, "the scripted zone entry must reach ready");
    }

    async fn put(&mut self, id: &str, operation: Value) {
        let response = self.host.control.router().oneshot(
            Request::put(format!("/v2/player/commands/{id}"))
                .header("authorization", "Bearer test-credential")
                .header("content-type", "application/json")
                .body(Body::from(json!({
                    "session": "test-session",
                    "zone_epoch": self.host.epoch,
                    "operation": operation,
                }).to_string()))
                .unwrap(),
        ).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED, "{id}");
    }

    /// Outbound packets since the last call, by opcode name.
    fn census(&mut self) -> BTreeMap<String, usize> {
        let mut out = BTreeMap::new();
        loop {
            let page = self.trace.page(self.read, 1000, None, None);
            assert!(!page.gap, "the census must see every packet");
            if page.packets.is_empty() {
                break;
            }
            self.read = page.next;
            for packet in page.packets {
                let row = serde_json::to_value(&packet).unwrap();
                if row["dir"] != "out" {
                    continue;
                }
                let name = row["opcode_name"].as_str().unwrap().to_string();
                let name = if name == "OP_CLIENT_UPDATE" {
                    let fields = &row["fields"];
                    let still = fields["delta"].as_array().unwrap().iter().all(|d| d.as_f64() == Some(0.0));
                    if still && fields["animation"] == 0 { REST.into() } else { MOVING.into() }
                } else {
                    name
                };
                *out.entry(name).or_default() += 1;
            }
        }
        out
    }

    fn phase(&mut self, name: &str) -> BTreeMap<String, usize> {
        let counts = self.census();
        println!("census {name}: {counts:?}");
        counts
    }
}

fn count(counts: &BTreeMap<String, usize>, name: &str) -> usize {
    counts.get(name).copied().unwrap_or(0)
}

/// Every opcode outside position streaming, which is wall-clock paced.
fn commanded(counts: &BTreeMap<String, usize>) -> BTreeMap<String, usize> {
    counts
        .iter()
        .filter(|(name, _)| !name.starts_with("OP_CLIENT_UPDATE"))
        .map(|(name, n)| (name.clone(), *n))
        .collect()
}

fn expect(pairs: &[(&str, usize)]) -> BTreeMap<String, usize> {
    pairs.iter().map(|(name, n)| (name.to_string(), *n)).collect()
}

/// The routine journey's wire, phase by phase. A real client sends no attack-off at zone entry, on
/// a readiness gap, or when attack was never on; with attack on, each implicit stop sends exactly
/// one. A stop always sends one rest update.
///
/// MUTATION CHECKS: drop the `!forced && !self.auto_attack` guard in `drain_combat` → RED at
/// "zone entry" (implicit offs reach the wire); drop the zone-epoch reset → RED at "zone change"
/// (an off for attack the new zone never had); route `attack_off` through the implicit write → RED
/// at "explicit off while off"; delete the rest flush → RED at "move".
#[tokio::test]
async fn census_of_a_scripted_session() {
    let mut s = Census::new().await;

    s.enter_zone("qeynos2").await;
    let entry = s.phase("zone entry");
    assert_eq!(commanded(&entry), expect(&[]), "zone entry sends no commanded packet");
    s.ticks(5).await;
    assert_eq!(commanded(&s.phase("idle")), expect(&[]));

    // Move, then stop: moving updates, then exactly one rest update.
    s.put("move", json!({"kind":"move_for","east":1.0,"north":0.0,"duration_ms":300})).await;
    s.ticks(30).await;
    let moved = s.phase("move");
    assert!(count(&moved, MOVING) >= 1, "moving updates while moving: {moved:?}");
    assert_eq!(count(&moved, REST), 1, "one rest update per stop: {moved:?}");
    assert_eq!(commanded(&moved), expect(&[]));

    // Target, attack on, renew, lease expiry.
    s.put("target", json!({"kind":"target","entity_id":7})).await;
    s.ticks(2).await;
    assert_eq!(commanded(&s.phase("target")), expect(&[("OP_TARGET_MOUSE", 1), ("OP_CONSIDER", 1)]));
    s.put("attack", json!({"kind":"attack","lease_ms":200})).await;
    s.ticks(3).await;
    assert_eq!(commanded(&s.phase("attack on")), expect(&[("OP_AUTO_ATTACK", 1)]));
    s.ticks(15).await;
    assert_eq!(commanded(&s.phase("lease expiry")), expect(&[("OP_AUTO_ATTACK", 1)]),
        "expiry sends attack-off once");
    s.ticks(5).await;
    assert_eq!(commanded(&s.phase("after expiry")), expect(&[]));

    // Explicit off while off is always sent; sit and camp with attack off send no attack-off.
    s.put("off", json!({"kind":"attack_off"})).await;
    s.ticks(2).await;
    assert_eq!(commanded(&s.phase("explicit off while off")), expect(&[("OP_AUTO_ATTACK", 1)]));
    assert_eq!(s.host.control.command("off").unwrap().status, Status::AppliedLocal);
    s.put("sit", json!({"kind":"sit"})).await;
    s.ticks(2).await;
    assert_eq!(commanded(&s.phase("sit with attack off")), expect(&[("OP_SPAWN_APPEARANCE", 1)]));
    s.put("stand", json!({"kind":"stand"})).await;
    s.ticks(2).await;
    assert_eq!(commanded(&s.phase("stand")), expect(&[("OP_SPAWN_APPEARANCE", 1)]));

    // Zone change with attack on: the new zone's server has attack off, so nothing is sent.
    s.put("attack2", json!({"kind":"attack","lease_ms":5000})).await;
    s.ticks(3).await;
    assert_eq!(commanded(&s.phase("attack on again")), expect(&[("OP_AUTO_ATTACK", 1)]));
    s.enter_zone("qeynos").await;
    assert_eq!(commanded(&s.phase("zone change with attack on")), expect(&[]));

    // Death with attack on: one off at the readiness loss, none on the respawn zone-in.
    s.put("attack3", json!({"kind":"attack","lease_ms":5000})).await;
    s.ticks(3).await;
    assert_eq!(commanded(&s.phase("attack on in new zone")), expect(&[("OP_AUTO_ATTACK", 1)]));
    s.gs.player_dead_since = Some(Instant::now());
    s.gs.cur_hp = 0;
    s.ticks(10).await;
    assert_eq!(commanded(&s.phase("death")), expect(&[("OP_AUTO_ATTACK", 1)]),
        "one attack-off at death, not one per not-ready tick");
    s.gs.player_dead_since = None;
    s.enter_zone("qeynos2").await;
    assert_eq!(commanded(&s.phase("respawn zone-in")), expect(&[]));
}
