//! Outbound census: a scripted session through the real host frame (`Host::frame`, the run loop's
//! per-tick body) and the real network action loop, counting every outbound application packet in
//! the session trace by opcode. A handoff's "wire impact" statement cites this test's output.
//!
//! Timing. The host runs on a virtual clock, 20 ms per tick, so leases and movement durations end
//! on a fixed tick whatever the scheduler does. Live, they end on elapsed wall time, so a stalled
//! host ends them on its next tick; the census trades that for determinism. The network loop ticks
//! twice per host tick, as it does live (10 ms and 20 ms), and not at all during a zone handshake.
//! Production resumes gameplay drains when the zone-in exchange after `OP_NewZone` finishes, which
//! can be before or after the host has loaded geometry, so the session runs under both orderings.
//! Position streaming keeps the real wall clock.
//!
//! Classification.
//! - `OP_AutoAttack` rows are split by their on/off byte and named with their command ID.
//! - `OP_ClientUpdate` rows are named by the reason the network owner recorded for that sequence
//!   (`ControllerView::last_position_send`, read after every network tick), and a rest row also by
//!   its command ID. Every row must have a recorded reason.
//! - Moving rows and keepalives follow the wall clock and are not asserted exactly; rest and
//!   correction rows are.
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

const MOVING: &str = "OP_CLIENT_UPDATE moving";
const KEEPALIVE: &str = "OP_CLIENT_UPDATE keepalive";
const UNATTRIBUTED_REST: &str = "OP_CLIENT_UPDATE rest";
const ON: &str = "OP_AUTO_ATTACK on";
const OFF: &str = "OP_AUTO_ATTACK off";
const TICK: Duration = Duration::from_millis(20);

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
    clock: Instant,
    read: u64,
    /// The reason the network owner recorded for each position-update sequence it sent.
    reasons: std::collections::HashMap<u16, eqoxide_core::game_state::PositionSendReason>,
    last_send_at: Option<Instant>,
    ready: bool,
    net_running: bool,
    resume: Resume,
    /// The run loop's terminal network outcome, passed to every frame once set.
    terminal: Option<String>,
    shutdown: bool,
    _peer: UdpSocket,
}

impl Census {
    /// The host as `run` builds it: no body, epoch 0, nothing published.
    async fn new(resume: Resume) -> Self {
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
            clock: started,
            read: 0,
            reasons: Default::default(),
            last_send_at: None,
            ready: false,
            net_running: true,
            resume,
            terminal: None,
            shutdown: false,
            _peer: peer,
        }
    }

    /// One 20 ms host tick in run-loop order, with two 10 ms network ticks before it.
    async fn tick(&mut self) {
        for _ in 0..2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if self.net_running {
                drain_gameplay_commands(
                    &mut self.action_loop, &mut self.stream, &mut self.gs, &self.lifecycle,
                );
                // At most one position update per network tick, so the last send is this tick's.
                if let Some(sent) = self.controller.controller_view.lock().unwrap().last_position_send {
                    if self.reasons.insert(sent.seq, sent.reason).is_none() {
                        // A keepalive is owed only after the keepalive interval of silence.
                        if sent.reason == eqoxide_core::game_state::PositionSendReason::Keepalive {
                            let gap = self.last_send_at.map(|at| sent.at - at);
                            assert!(gap.is_none_or(|gap| gap >= Duration::from_millis(1300)),
                                "keepalive {gap:?} after the previous update");
                        }
                        self.last_send_at = Some(sent.at);
                    }
                }
            }
        }
        self.clock += TICK;
        let now = self.clock;
        let snapshot = self.gs.clone();
        self.host.drain_completions();
        let link = || Link { tick_age: 0, inbound_age: 0, link_age: 0, dropped: false };
        let shutdown = self.shutdown;
        let ready = self
            .host
            .frame(
                &snapshot, &mut self.frame, &self.terminal, &|| shutdown, &link, now, TICK.as_secs_f32(),
                self.started, &self.collision,
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
    /// spawn arrives, the handshake runs, `OP_NewZone` arrives, and the zone-in exchange finishes
    /// either at once (`Resume::AtNewZone`) or after the host has loaded geometry
    /// (`Resume::AfterGeometry`). Only then do gameplay drains resume.
    async fn enter_zone(&mut self, zone: &str) {
        self.net_running = false;
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
        if self.resume == Resume::AtNewZone {
            self.net_running = true;
        }
        // Geometry loads and the body appears.
        self.ticks(5).await;
        self.net_running = true;
        self.ticks(10).await;
        assert!(self.ready, "the scripted zone entry must reach ready");
    }

    /// A PUT whose admission outcome the caller checks itself.
    async fn put_any(&mut self, id: &str, operation: Value) {
        let _ = self.put_status(id, operation).await;
    }

    async fn put_status(&mut self, id: &str, operation: Value) -> StatusCode {
        self.host.control.router().oneshot(
            Request::put(format!("/v2/player/commands/{id}"))
                .header("authorization", "Bearer test-credential")
                .header("content-type", "application/json")
                .body(Body::from(json!({
                    "session": "test-session",
                    "zone_epoch": self.host.epoch,
                    "operation": operation,
                }).to_string()))
                .unwrap(),
        ).await.unwrap().status()
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

    /// Outbound packets since the last call, classified as described in the module doc.
    fn census(&mut self) -> BTreeMap<String, usize> {
        let mut out = BTreeMap::new();
        loop {
            let page = self.trace.page(self.read, 1000, None, None);
            assert!(!page.gap, "the census must see every packet");
            if page.packets.is_empty() {
                break;
            }
            self.read = page.next;
            for mut packet in page.packets {
                packet.include_raw_payload();
                let row = serde_json::to_value(&packet).unwrap();
                if row["dir"] != "out" {
                    continue;
                }
                let command = row["command_id"].as_str();
                let name = row["opcode_name"].as_str().unwrap().to_string();
                let name = match name.as_str() {
                    "OP_CLIENT_UPDATE" => {
                        use eqoxide_core::game_state::PositionSendReason as Reason;
                        let seq = row["fields"]["sequence"].as_u64().unwrap() as u16;
                        let reason = *self.reasons.get(&seq)
                            .unwrap_or_else(|| panic!("no recorded reason for position update {row}"));
                        // The trace row carries the same reason the sender recorded.
                        assert_eq!(row["fields"]["reason"], reason.as_str(), "{row}");
                        match (reason, command) {
                            (Reason::Moving, _) => MOVING.into(),
                            (Reason::Keepalive, _) => KEEPALIVE.into(),
                            (Reason::Rest, Some(id)) => format!("OP_CLIENT_UPDATE rest ({id})"),
                            (Reason::Rest, None) => UNATTRIBUTED_REST.into(),
                            (Reason::Correction, _) => "OP_CLIENT_UPDATE correction".into(),
                        }
                    }
                    "OP_AUTO_ATTACK" => {
                        // The whole payload: native rejects anything but exactly 4 bytes
                        // (eqemu:zone/client_packet.cpp@9a0579f37:3518).
                        let state = match row["payload_hex"].as_str().unwrap() {
                            "01000000" => ON.to_string(),
                            "00000000" => OFF.to_string(),
                            other => format!("OP_AUTO_ATTACK {other}"),
                        };
                        match command {
                            Some(id) => format!("{state} ({id})"),
                            None => state,
                        }
                    }
                    _ => name,
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

/// Every row except moving updates and keepalives, which follow the wall clock.
fn exact(counts: &BTreeMap<String, usize>) -> BTreeMap<String, usize> {
    counts
        .iter()
        .filter(|(name, _)| name.as_str() != MOVING && name.as_str() != KEEPALIVE)
        .map(|(name, n)| (name.clone(), *n))
        .collect()
}

fn expect(pairs: &[(&str, usize)]) -> BTreeMap<String, usize> {
    pairs.iter().map(|(name, n)| (name.to_string(), *n)).collect()
}

/// The routine journey's wire, phase by phase. A real client sends no attack-off at zone entry, on
/// a readiness gap, or when attack was never on; with attack on, each implicit stop sends exactly
/// one; an explicit off is sent and owned by its command. A stop sends exactly one rest update.
///
/// MUTATION CHECKS (each RED in at least one ordering): drop the `self.auto_attack` condition for
/// an implicit off in `drain_combat` → "zone entry"; drop the zone-epoch reset or the explicit
/// off's zone check → "zone change"; route `attack_off` through the implicit write → "explicit off
/// while off"; delete the rest flush, or send a second rest after it → "move"; send byte 1 for
/// every attack packet, or a one-byte payload → "lease expiry"; drop the host's settlement before
/// the zone fence → the "zone change" receipt check; keepalive interval at 290 ms or 1300 s → the
/// keepalive spacing check or "long idle". The terminal, shutdown and same-frame orderings are
/// pinned by the tests at the end of this file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Resume {
    AtNewZone,
    AfterGeometry,
}

#[tokio::test]
async fn census_when_drains_resume_at_new_zone() {
    scripted_session(Resume::AtNewZone).await;
}

#[tokio::test]
async fn census_when_drains_resume_after_geometry() {
    scripted_session(Resume::AfterGeometry).await;
}

async fn scripted_session(resume: Resume) {
    println!("census ordering: {resume:?}");
    let mut s = Census::new(resume).await;

    s.enter_zone("qeynos2").await;
    assert_eq!(exact(&s.phase("zone entry")), expect(&[]), "zone entry sends nothing exact");
    s.ticks(5).await;
    assert_eq!(exact(&s.phase("idle")), expect(&[]));
    // Long idle: at least 1.5 s of wall time, so the keepalive must come (its spacing is asserted
    // as each send is recorded).
    let idle_started = Instant::now();
    while idle_started.elapsed() < Duration::from_millis(1500) {
        s.tick().await;
    }
    let idle = s.phase("long idle");
    assert!(count(&idle, KEEPALIVE) >= 1, "a keepalive within 1.5 s of idle (interval 1.3 s): {idle:?}");
    assert_eq!(exact(&idle), expect(&[]));

    // Move for 300 ms, then stop: moving updates, then exactly one rest update, the move's.
    s.put("move", json!({"kind":"move_for","east":1.0,"north":0.0,"duration_ms":300})).await;
    s.ticks(30).await;
    let moved = s.phase("move");
    assert!(count(&moved, MOVING) >= 1, "moving updates while moving: {moved:?}");
    assert_eq!(exact(&moved), expect(&[("OP_CLIENT_UPDATE rest (move)", 1)]));

    // Target, attack on, lease expiry at 200 ms.
    s.put("target", json!({"kind":"target","entity_id":7})).await;
    s.ticks(2).await;
    assert_eq!(exact(&s.phase("target")), expect(&[("OP_TARGET_MOUSE", 1), ("OP_CONSIDER", 1)]));
    s.put("attack", json!({"kind":"attack","lease_ms":200})).await;
    s.ticks(3).await;
    assert_eq!(exact(&s.phase("attack on")), expect(&[("OP_AUTO_ATTACK on (attack)", 1)]));
    s.ticks(15).await;
    assert_eq!(exact(&s.phase("lease expiry")), expect(&[(OFF, 1)]), "expiry sends attack-off once");
    s.ticks(5).await;
    assert_eq!(exact(&s.phase("after expiry")), expect(&[]));

    // Explicit off while off is sent and owned; sit with attack off sends no attack-off.
    s.put("off", json!({"kind":"attack_off"})).await;
    s.ticks(2).await;
    assert_eq!(exact(&s.phase("explicit off while off")), expect(&[("OP_AUTO_ATTACK off (off)", 1)]));
    assert_eq!(s.host.control.command("off").unwrap().status, Status::AppliedLocal);
    s.put("sit", json!({"kind":"sit"})).await;
    s.ticks(2).await;
    assert_eq!(exact(&s.phase("sit with attack off")), expect(&[("OP_SPAWN_APPEARANCE", 1)]));
    s.put("stand", json!({"kind":"stand"})).await;
    s.ticks(2).await;
    assert_eq!(exact(&s.phase("stand")), expect(&[("OP_SPAWN_APPEARANCE", 1)]));

    // Sit with attack on: one off, owned by the sit.
    s.put("attack-sit", json!({"kind":"attack","lease_ms":5000})).await;
    s.ticks(3).await;
    assert_eq!(exact(&s.phase("attack on before sit")), expect(&[("OP_AUTO_ATTACK on (attack-sit)", 1)]));
    s.put("sit2", json!({"kind":"sit"})).await;
    s.ticks(2).await;
    assert_eq!(exact(&s.phase("sit with attack on")),
        expect(&[("OP_AUTO_ATTACK off (sit2)", 1), ("OP_SPAWN_APPEARANCE", 1)]));
    s.put("stand2", json!({"kind":"stand"})).await;
    s.ticks(2).await;
    assert_eq!(exact(&s.phase("stand again")), expect(&[("OP_SPAWN_APPEARANCE", 1)]));

    // Zone change with attack on and an explicit off still queued: the new zone's server has
    // attack off, so neither the halt nor the departed zone's explicit off is sent there.
    s.put("attack2", json!({"kind":"attack","lease_ms":5000})).await;
    s.ticks(3).await;
    assert_eq!(exact(&s.phase("attack on again")), expect(&[("OP_AUTO_ATTACK on (attack2)", 1)]));
    s.net_running = false;
    s.put("off-departing", json!({"kind":"attack_off"})).await;
    s.tick().await;
    s.enter_zone("qeynos").await;
    // central-okgn: the first update in the new zone can read as motion, depending on how its
    // wall-clock speed rounds, and then an unattributed rest follows. Both predate the attack-off
    // change and are excluded here; the move phase still pins the rest flush exactly.
    let mut changed = exact(&s.phase("zone change with attack on"));
    changed.remove(UNATTRIBUTED_REST);
    assert_eq!(changed, expect(&[]));
    let departing = s.host.control.command("off-departing").unwrap();
    assert_eq!((departing.status, departing.reason.as_deref()),
        (Status::Refused, Some("stale_zone_epoch_before_dispatch")), "nothing was sent: {departing:?}");

    // Death with attack on: one off at the readiness loss, none on the respawn zone-in.
    s.put("attack3", json!({"kind":"attack","lease_ms":5000})).await;
    s.ticks(3).await;
    assert_eq!(exact(&s.phase("attack on in new zone")), expect(&[("OP_AUTO_ATTACK on (attack3)", 1)]));
    s.gs.player_dead_since = Some(Instant::now());
    s.gs.cur_hp = 0;
    s.ticks(10).await;
    assert_eq!(exact(&s.phase("death")), expect(&[(OFF, 1)]),
        "one attack-off at death, not one per not-ready tick");
    s.gs.player_dead_since = None;
    s.enter_zone("qeynos2").await;
    assert_eq!(exact(&s.phase("respawn zone-in")), expect(&[]));

    // Camp with attack on: camp, sit and one attack-off, all owned by the camp.
    s.put("attack4", json!({"kind":"attack","lease_ms":5000})).await;
    s.ticks(3).await;
    assert_eq!(exact(&s.phase("attack on before camp")), expect(&[("OP_AUTO_ATTACK on (attack4)", 1)]));
    s.put("camp", json!({"kind":"camp"})).await;
    s.ticks(3).await;
    assert_eq!(exact(&s.phase("camp with attack on")),
        expect(&[("OP_AUTO_ATTACK off (camp)", 1), ("OP_CAMP", 1), ("OP_SPAWN_APPEARANCE", 1)]));
}

/// The run loop's terminal path through `Host::frame`: when the network owner ends, an
/// `attack_off` it never drained is refused as not sent, and a later one is refused at
/// admission. Dropping the terminal handling from `frame` leaves the first `accepted` forever.
#[tokio::test]
async fn a_terminal_network_outcome_settles_queued_attack_offs() {
    let mut s = Census::new(Resume::AfterGeometry).await;
    s.enter_zone("qeynos2").await;
    s.net_running = false;
    s.put("queued", json!({"kind":"attack_off"})).await;
    s.tick().await;
    assert_eq!(s.host.control.command("queued").unwrap().status, Status::Accepted);
    s.terminal = Some("session_ended".into());
    s.tick().await;
    for id in ["queued", "late"] {
        if id == "late" {
            s.put_any(id, json!({"kind":"attack_off"})).await;
            s.tick().await;
        }
        let receipt = s.host.control.command(id).unwrap();
        assert_eq!((receipt.status, receipt.reason.as_deref()), (Status::Refused, Some("not_ready")), "{id}: {receipt:?}");
        assert_eq!(receipt.evidence, Some(json!({"kind":"attack_off_not_sent","packets":[]})), "{id}");
    }
    assert_eq!(exact(&s.phase("terminal")), expect(&[]), "nothing is sent after the end");
}

/// The zone ends and then the session, both seen in one host frame: an `attack_off` still in the
/// journal keeps the zone's reason, not the session's.
#[tokio::test]
async fn a_zone_end_before_the_session_end_keeps_its_reason() {
    let mut s = Census::new(Resume::AfterGeometry).await;
    s.enter_zone("qeynos2").await;
    s.net_running = false;
    s.put("off", json!({"kind":"attack_off"})).await;
    // No scheduled publication is due: only the zone change's own publication can fence first.
    s.frame.publish_at = s.clock + Duration::from_secs(60);
    s.controller.begin_zone_in(&mut s.gs);
    s.terminal = Some("session_ended".into());
    s.tick().await;
    let receipt = s.host.control.command("off").unwrap();
    assert_eq!((receipt.status, receipt.reason.as_deref()),
        (Status::Refused, Some("stale_zone_epoch_before_dispatch")), "{receipt:?}");
    assert_eq!(receipt.evidence, Some(json!({"kind":"attack_off_not_sent","packets":[]})));
}

/// A kick sets shutdown and parks the network owner with no terminal outcome: from the first
/// shutdown frame, nothing more is admitted.
#[tokio::test]
async fn shutdown_closes_admission() {
    let mut s = Census::new(Resume::AfterGeometry).await;
    s.enter_zone("qeynos2").await;
    s.shutdown = true;
    s.tick().await;
    s.put_any("late", json!({"kind":"attack_off"})).await;
    let receipt = s.host.control.command("late").unwrap();
    assert_eq!((receipt.status, receipt.reason.as_deref()), (Status::Refused, Some("not_ready")), "{receipt:?}");
    assert_eq!(receipt.evidence, Some(json!({"kind":"attack_off_not_sent","packets":[]})));
}

/// An off the network owner sent in the departed zone, then the zone and the session end before
/// the next host frame: the receipt says it was sent, not fenced.
#[tokio::test]
async fn an_off_sent_before_a_zone_and_session_end_stays_sent() {
    let mut s = Census::new(Resume::AfterGeometry).await;
    s.enter_zone("qeynos2").await;
    s.net_running = false;
    s.put("off", json!({"kind":"attack_off"})).await;
    s.tick().await;
    let drained = s.host.command.take_attack_request(Some(s.gs.zone_epoch)).unwrap();
    for off in drained.explicit {
        s.host.command.attack_off_sent(off.command.as_ref().unwrap());
    }
    s.controller.begin_zone_in(&mut s.gs);
    s.terminal = Some("session_ended".into());
    s.tick().await;
    let receipt = s.host.control.command("off").unwrap();
    assert_eq!(receipt.status, Status::AppliedLocal, "{receipt:?}");
}
