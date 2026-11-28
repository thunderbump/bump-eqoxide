//! CPU host for the normal login/world/zone session and the shared physical controller.
use crate::{
    control::{AdmittedCommand, CommandStatus as Status, Control, Operation},
    profile::Profile,
};
use eqoxide_command::CommandState;
use eqoxide_core::{game_state::GameState, region_map::RegionMap};
use eqoxide_ipc as ipc;
use eqoxide_net::{action_loop::ActionLoopSlots, gameplay::GameplayLifecycle};
use eqoxide_physics::{
    movement::{manual_wish, CharacterController, MoveIntent},
    zone_in::ZoneIn,
};
use eqoxide_zone_geometry::{
    collision::{Collision, SharedCollision},
    native_map::{NativeMap, NativeMapLimits},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::Read,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

struct Wiring {
    command: CommandState,
    controller: ipc::ControllerSlots,
    snapshot: ipc::GameStateSnapshot,
    health: ipc::NetHealthShared,
    shutdown: Arc<AtomicBool>,
    slots: ActionLoopSlots,
    lifecycle: GameplayLifecycle,
    collision: SharedCollision,
}
impl Wiring {
    fn new(maps_dir: std::path::PathBuf) -> Self {
        let merchant = ipc::MerchantSlots::default();
        let inventory = ipc::InventorySlots::default();
        let interact = ipc::InteractSlots::default();
        let quest = ipc::QuestSlots::default();
        let group = ipc::GroupSlots::default();
        let guild = ipc::GuildSlots::default();
        let trainer = ipc::TrainerSlots::default();
        let social = ipc::SocialSlots::default();
        let chat = ipc::ChatSlots::default();
        let life = ipc::LifecycleSlots::default();
        let controller = ipc::ControllerSlots::default();
        let command = CommandState::new(
            ipc::CombatSlots::default(),
            merchant.clone(),
            inventory.clone(),
            interact.clone(),
            quest.clone(),
            group.clone(),
            guild.clone(),
            trainer,
            social.clone(),
            chat.clone(),
            life.clone(),
        );
        let snapshot = Arc::new(arc_swap::ArcSwap::from_pointee(GameState::new()));
        let health = Arc::new(Mutex::new(ipc::NetHealth::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let collision = Arc::new(std::sync::RwLock::new(None));
        let slots = ActionLoopSlots {
            world: Default::default(),
            quest,
            group_slots: group,
            command: command.clone(),
            social,
            merchant_slots: merchant,
            inventory_slots: inventory,
            interact,
            chat,
            controller: controller.clone(),
            guild_slots: guild,
            collision: collision.clone(),
            maps_dir,
        };
        let lifecycle = GameplayLifecycle {
            shutdown: shutdown.clone(),
            camp: life.camp,
            camp_until: life.camp_until,
            respawn: life.respawn,
            game_state_snapshot: snapshot.clone(),
            net_health: health.clone(),
        };
        Self {
            command,
            controller,
            snapshot,
            health,
            shutdown,
            slots,
            lifecycle,
            collision,
        }
    }
}

/// Deadline lives in the physics owner. No HTTP retry, poll, or slow transaction renews it.
struct Motion {
    id: Arc<str>,
    wish: [f32; 2],
    until: Instant,
}
struct Host {
    control: Control,
    command: CommandState,
    controller_slots: ipc::ControllerSlots,
    body: Option<CharacterController>,
    zone_in: ZoneIn,
    epoch: u64,
    heading: f32,
    motion: Option<Motion>,
    ending_motion: Option<Arc<str>>,
    attack_until: Option<Instant>,
    action_ids: HashMap<u64, (String, bool)>,
    transaction_uncertain: Arc<AtomicBool>,
    inventory_revision: u64,
    merchant_revision: u64,
    inventory_value: Value,
    merchant_value: Value,
    last_event: u64,
    last_echo: u64,
    completions: Completions,
    /// The last `CombatRecord::id` and death instant published as events (central-6ly1.8).
    last_combat: u64,
    last_death_count: u32,
    /// The self position in the last published state: every range check measures from this, so a
    /// script reading state sees the same distances the host decides with (central-6ly1.8).
    published_position: Option<[f32; 3]>,
}
impl Host {
    /// Publish an awaited outcome and settle the latch from the receipt as it stands: late
    /// evidence for a fenced `unknown` is not applied, so it cannot reopen transactions either.
    fn apply_completion(&self, done: Completion) {
        if done.transaction {
            // The latch first, then the receipt, so a script that sees `confirmed` can send its
            // next transaction at once. Both run on the host loop, so no fence falls between them.
            // A receipt already terminal (fenced to `unknown`) keeps its status, and so the latch.
            let settled = match self.control.settled_status(&done.id) {
                Some(status) if status.terminal() => status,
                _ => done.status,
            };
            self.transaction_uncertain
                .store(!matches!(settled, Status::Confirmed | Status::Refused), Ordering::Release);
        }
        let _ = self.control.publish_outcome_with_detail(&done.id, done.status, done.reason, done.detail, done.evidence);
    }
    /// Apply awaited outcomes on the host loop, so no fence can run between settling the latch
    /// and publishing the receipt. Returns whether anything was applied, so the caller can
    /// publish state at once.
    fn drain_completions(&mut self, _snapshot: &Arc<GameState>) -> bool {
        let mut applied = false;
        while let Ok(done) = self.completions.1.try_recv() {
            self.apply_completion(done);
            applied = true;
        }
        applied
    }
    fn outcome(&self, id: &str, status: Status, reason: Option<&str>, evidence: Option<Value>) {
        let _ = self
            .control
            .publish_outcome(id, status, reason.map(str::to_owned), evidence);
    }
    fn halt(&mut self, reason: &str, command: Option<&str>) {
        let motion = self.motion.take();
        let ending = self.ending_motion.take();
        if motion.is_some() || ending.is_some() {
            let mut view = self.controller_slots.controller_view.lock().unwrap();
            view.active_motion_command = None;
            view.position_commands = ipc::PositionCommands::None;
        }
        if let Some(motion) = motion {
            self.control
                .publish_event(json!({"kind":"motion_stopped","id":motion.id.as_ref(),"reason":reason}));
        }
        self.attack_until = None;
        // LAST-WINS: safety off must replace an attack still waiting in the net mailbox.
        self.command.overwrite_attack_off(command);
    }
    fn step(
        &mut self,
        gs: &GameState,
        col: Option<&Collision>,
        ready: bool,
        now: Instant,
        dt: f32,
    ) {
        if !ready {
            self.halt("not_ready", None);
            return;
        }
        if self.motion.as_ref().is_some_and(|m| now >= m.until) {
            let motion = self.motion.take().unwrap();
            self.control
                .publish_event(json!({"kind":"motion_expired","id":motion.id.as_ref()}));
            self.ending_motion = Some(motion.id);
        }
        if self.attack_until.is_some_and(|until| now >= until) {
            self.attack_until = None;
            self.command.overwrite_attack_off(None); // LAST-WINS: lease expiry
            self.control
                .publish_event(json!({"kind":"attack_lease_expired"}));
        }
        let Some(body) = self.body.as_mut() else {
            return;
        };
        // Take the correction under the view lock, the same order the net streamer writes it in,
        // so the issue count read here is the one that belongs to this correction.
        let taken = {
            let view = self.controller_slots.controller_view.lock().unwrap();
            let correction = self.controller_slots.pos_correction.lock().unwrap().take();
            correction.map(|pos| (pos, view.correction_issued))
        };
        if let Some((correction, _)) = taken {
            body.teleport(correction);
        }
        self.zone_in.on_frame(
            body,
            &gs.world.zone_name,
            false,
            true,
            col,
            gs.world.zone_underworld,
        );
        let stepped = col.is_some();
        if let Some(col) = col {
            let wish = self.motion.as_ref().map(|m| m.wish).unwrap_or([0.0, 0.0]);
            body.set_underworld(gs.world.zone_underworld);
            body.set_levitating(gs.player_levitating());
            body.step(
                MoveIntent {
                    wish_dir: wish,
                    speed: eqoxide_physics::movement::DEFAULT_MOVE_SPEED,
                    ..Default::default()
                },
                dt,
                col,
            );
        } else {
            body.clear_hold();
        }
        let mut view = self.controller_slots.controller_view.lock().unwrap();
        // Network zone entry invalidates this view under the same lock. Never republish an old epoch.
        if view.zone_epoch.is_some_and(|epoch| epoch != self.epoch) {
            return;
        }
        view.zone_epoch = Some(self.epoch);
        if let Some((_, issued)) = taken {
            // An old physics step can publish after the net owner's snapshot and before it
            // issues this correction. Replace that provenance before recording the adopted step.
            view.position_commands = ipc::PositionCommands::None;
            // Zone entry can settle a later correction while this step runs.
            view.correction_adopted = view.correction_adopted.max(issued);
        }
        let active_command = self.motion.as_ref().map(|motion| &motion.id);
        view.active_motion_command = active_command.cloned();
        if stepped {
            view.step_seq = view.step_seq.wrapping_add(1);
            view.position_commands.include(active_command);
            view.position_commands.include(self.ending_motion.take().as_ref());
        }
        view.pos = body.pos;
        view.heading = self.heading;
        view.initialized = true;
        view.publish_disclosures(body.disclosures());
        if view.landed_fall_height.is_none() {
            view.landed_fall_height = body.take_landed_fall_height();
        }
        if view.relocated.is_none() {
            view.relocated = body.take_relocation();
        }
    }
    fn update_knowledge(&mut self, gs: &GameState) {
        let inventory = json!({"known":gs.inventory_received,"items":gs.inventory,"coin":gs.coin,"coin_confirmed":gs.coin_confirmed});
        if inventory != self.inventory_value {
            self.inventory_revision += 1;
            self.inventory_value = inventory;
        }
        let merchant = json!({"open":gs.merchant_open,"items":gs.merchant_items});
        if merchant != self.merchant_value {
            self.merchant_revision += 1;
            self.merchant_value = merchant;
        }
        // The source queues are bounded (200 events, 32 echoes): anything evicted before this tick
        // saw it is reported, never silently lost (design A4).
        if let Some(first) = gs.chat_events.front() {
            if first.id > self.last_event + 1 {
                self.control.publish_event(json!({"kind":"source_events_dropped","source":"game_event",
                    "count":first.id - self.last_event - 1}));
            }
        }
        if let Some(first) = gs.received_self_echoes.front() {
            if first.id > self.last_echo + 1 {
                self.control.publish_event(json!({"kind":"source_events_dropped","source":"self_echo",
                    "count":first.id - self.last_echo - 1}));
            }
        }
        for event in gs.chat_events.iter().filter(|e| e.id > self.last_event) {
            self.control.publish_event(json!({"kind":"received_game_event","source":if event.category=="chat" {"received_packet"} else {"client_game_event"},"event_id":event.id,
                "category":event.category,"event_kind":event.kind,"from":event.from,"directed":event.directed,"text":event.text}));
        }
        if let Some(event) = gs.chat_events.back() {
            self.last_event = event.id;
        }
        for echo in gs
            .received_self_echoes
            .iter()
            .filter(|echo| echo.id > self.last_echo)
        {
            self.control.publish_event(json!({"kind":"received_self_echo","source":"received_packet",
                "channel":echo.channel,"sender":echo.sender,"text":echo.text,"truncated":echo.truncated,
                "recipient_delivery_confirmed":false}));
        }
        if let Some(echo) = gs.received_self_echoes.back() {
            self.last_echo = echo.id;
        }
        // Records older than the bounded source queue were evicted unseen: say so, never silently.
        if let Some(first) = gs.combat_records.front() {
            if first.id > self.last_combat + 1 {
                self.control.publish_event(json!({"kind":"source_events_dropped","source":"combat",
                    "count":first.id - self.last_combat - 1}));
            }
        }
        for hit in gs.combat_records.iter().filter(|r| r.id > self.last_combat) {
            self.control.publish_event(json!({"kind":"combat","source":"received_packet","record_id":hit.id,
                "zone_epoch":hit.zone_epoch,"source_id":hit.source_id,"target_id":hit.target_id,
                "source_name":hit.source_name,"target_name":hit.target_name,"amount":hit.amount,
                "spell_id":hit.spell_id,"damage_type":hit.damage_type,"result":hit.result.as_str()}));
        }
        if let Some(hit) = gs.combat_records.back() {
            self.last_combat = hit.id;
        }
        if gs.death_count != self.last_death_count {
            // Two deaths between host reads leave only the latest killer: report the miss.
            let missed = gs.death_count.wrapping_sub(self.last_death_count).saturating_sub(1);
            if missed > 0 {
                self.control.publish_event(json!({"kind":"source_events_dropped","source":"death","count":missed}));
            }
            self.last_death_count = gs.death_count;
            self.control.publish_event(json!({"kind":"died","source":"received_packet","death_count":gs.death_count,
                "killer":{"id":gs.killer_id,"name":known_killer_name(gs)}}));
        }
        for event in self.command.actions().take_events() {
            if let Some((id, transaction)) = self.action_ids.remove(&event.id) {
                let status = match event.result.as_str() {
                    "confirmed" => Status::Confirmed,
                    "refused" => Status::Refused,
                    _ => Status::Unknown,
                };
                let evidence = (status==Status::Confirmed).then(||json!({"kind":"server_action_receipt","action":event.kind,"detail":event.reason}));
                let _ = self.control.publish_outcome(&id, status, Some(event.reason.clone()), evidence);
                if transaction {
                    // The receipt as it stands, not this report: a fenced `unknown` stays unknown,
                    // and so does the latch, whatever arrives later.
                    let settled = self.control.settled_status(&id).unwrap_or(status);
                    self.transaction_uncertain.store(
                        !matches!(settled, Status::Confirmed | Status::Refused), Ordering::Release);
                }
            }
        }
    }
    async fn dispatch(
        &mut self,
        command: AdmittedCommand,
        gs: &GameState,
        ready: bool,
        now: Instant,
    ) {
        let id = command.id;
        let req = command.request;
        let me = self.published_position;
        if req.zone_epoch != self.epoch || (!ready && !req.operation.is_priority()) {
            self.outcome(
                &id,
                Status::Refused,
                Some("session_or_zone_not_ready"),
                None,
            );
            return;
        }
        // Claim under the journal lock before any side effect. A priority command may have
        // superseded this drained entry while the host was waiting to dispatch it.
        if self.control.mark_dispatched(&id).is_err() {
            return;
        }
        let operation = req.operation;
        if matches!(
            operation,
            Operation::Buy { .. } | Operation::Sell { .. } | Operation::InventoryMove { .. }
        ) {
            if self.transaction_uncertain.load(Ordering::Acquire) {
                self.outcome(
                    &id,
                    Status::Refused,
                    Some("transaction_outcome_unknown_relog_required"),
                    None,
                );
                return;
            }
            if req.inventory_revision != Some(self.inventory_revision) || !gs.inventory_received {
                self.outcome(
                    &id,
                    Status::Refused,
                    Some("inventory_revision_required_or_stale"),
                    None,
                );
                return;
            }
            if !matches!(operation, Operation::InventoryMove { .. })
                && req.merchant_revision != Some(self.merchant_revision)
            {
                self.outcome(
                    &id,
                    Status::Refused,
                    Some("merchant_revision_required_or_stale"),
                    None,
                );
                return;
            }
        }
        match operation {
            Operation::MoveFor {
                east,
                north,
                duration_ms,
            } => {
                if gs.sitting || self.body.is_none() {
                    self.outcome(&id, Status::Refused, Some("stand_before_moving"), None);
                    return;
                }
                let (wish, heading) = manual_wish([east, north]);
                if let Some(heading) = heading {
                    self.heading = heading;
                }
                self.motion = Some(Motion {
                    id: id.as_str().into(),
                    wish,
                    until: now + Duration::from_millis(duration_ms),
                });
                self.outcome(&id,Status::AppliedLocal,None,Some(json!({"kind":"controller_intent","duration_ms":duration_ms,"server_position_confirmed":false})));
            }
            Operation::Stop => {
                if let Some(motion) = self.motion.take() {
                    self.ending_motion = Some(motion.id);
                }
                self.outcome(
                    &id,
                    Status::AppliedLocal,
                    None,
                    Some(json!({"kind":"controller_stopped"})),
                );
            }
            Operation::AttackOff => {
                self.attack_until = None;
                self.command.overwrite_attack_off(Some(&id)); // LAST-WINS: safety request
                self.outcome(
                    &id,
                    Status::AppliedLocal,
                    None,
                    Some(json!({"kind":"attack_off_queued","server_confirmed":false})),
                );
            }
            Operation::MerchantOpen { entity_id } => {
                if !observed_entity(gs, me, entity_id as u32, 30.0) {
                    self.outcome(
                        &id,
                        Status::Refused,
                        Some("merchant_not_received_or_out_of_range"),
                        None,
                    );
                    return;
                }
                let (tx, rx) = tokio::sync::oneshot::channel();
                let context = self
                    .command
                    .actions()
                    .begin_for_zone("merchant_open", self.epoch)
                    .with_packet_command(&id);
                if !context
                    .scope(async { self.command.request_open_await(entity_id as u32, tx) })
                    .await
                {
                    self.outcome(&id, Status::Refused, Some("network_mailbox_busy"), None);
                    return;
                }
                let done = self.completions.0.clone();
                tokio::spawn(async move {
                    let _ = done.send(await_outcome(id, rx, "server_merchant_open", false).await);
                });
            }
            Operation::Buy { slot, quantity: _ } => {
                let Some(merchant) = gs.merchant_open else {
                    self.outcome(&id, Status::Refused, Some("merchant_closed"), None);
                    return;
                };
                if !gs
                    .merchant_items
                    .iter()
                    .any(|item| item.merchant_slot == slot)
                {
                    self.outcome(
                        &id,
                        Status::Refused,
                        Some("catalog_slot_not_received"),
                        None,
                    );
                    return;
                }
                let (tx, rx) = tokio::sync::oneshot::channel();
                let context = self
                    .command
                    .actions()
                    .begin_for_zone("buy", self.epoch)
                    .with_packet_command(&id);
                if !context
                    .scope(async { self.command.request_buy_await(merchant, slot, tx) })
                    .await
                {
                    self.outcome(&id, Status::Refused, Some("network_mailbox_busy"), None);
                    return;
                }
                self.transaction_uncertain.store(true, Ordering::Release);
                let done = self.completions.0.clone();
                tokio::spawn(async move {
                    let _ = done.send(await_outcome(id, rx, "server_buy_receipt", true).await);
                });
            }
            operation => {
                let transaction = matches!(
                    operation,
                    Operation::Sell { .. } | Operation::InventoryMove { .. }
                );
                let tracker = self.command.actions();
                let context = tracker
                    .begin_for_zone("headless_command", self.epoch)
                    .with_packet_command(&id);
                let accepted = context
                    .scope(async {
                        match operation {
                            Operation::Loot { entity_id } => {
                                gs.world.entities.get(&(entity_id as u32)).is_some_and(|e| {
                                    e.dead && e.is_npc && within(me, e, 20.0)
                                }) && self.command.request_loot(entity_id as u32)
                            }
                            Operation::Target { entity_id } => {
                                observed_entity(gs, me, entity_id as u32, 500.0)
                                    && self.command.request_target(entity_id as u32)
                            }
                            Operation::Attack { lease_ms } => {
                                if gs
                                    .target_id
                                    .and_then(|id| gs.world.entities.get(&id))
                                    .is_none_or(|e| {
                                        e.dead || !e.is_npc || !within(me, e, 30.0)
                                    })
                                {
                                    false
                                } else {
                                    let accepted = self.command.request_attack(true);
                                    if accepted {
                                        self.attack_until =
                                            Some(now + Duration::from_millis(lease_ms));
                                    }
                                    accepted
                                }
                            }
                            Operation::Sit => {
                                let accepted = self.command.request_sit_with_attack_off(&id);
                                if accepted {
                                    if let Some(motion) = self.motion.take() {
                                        self.ending_motion = Some(motion.id);
                                    }
                                    self.attack_until = None;
                                }
                                accepted
                            }
                            Operation::Stand => self.command.request_sit(false),
                            Operation::Say { text } => self.command.request_say(text),
                            Operation::Tell { recipient, text } => {
                                self.command.request_chat_send(ipc::ChatSend {
                                    chan: 7,
                                    to: recipient,
                                    text,
                                });
                                true
                            }
                            Operation::MerchantClose => {
                                self.command.request_merchant_trade(ipc::TradeCmd::Close)
                            }
                            Operation::Sell { slot, quantity } => {
                                if let Some(merchant) = gs.merchant_open {
                                    gs.inventory.iter().any(|item| {
                                        item.slot == slot as i32
                                            && quantity <= item.charges.max(1) as u32
                                    }) && self
                                        .command
                                        .request_merchant_sell(merchant, slot, quantity)
                                } else {
                                    false
                                }
                            }
                            Operation::InventoryMove {
                                from_slot,
                                to_slot,
                                quantity,
                            } => {
                                quantity == 1
                                    && from_slot <= 32
                                    && to_slot <= 32
                                    && gs
                                        .inventory
                                        .iter()
                                        .any(|item| item.slot == from_slot as i32)
                                    && self.command.request_inventory_move(from_slot, to_slot)
                            }
                            Operation::Camp => {
                                self.halt("camp", Some(&id));
                                self.command.request_camp_with_command(
                                    ipc::CampCmd::StartInZone(self.epoch), Some(&id),
                                );
                                true
                            }
                            _ => false,
                        }
                    })
                    .await;
                if accepted && context.accepted() {
                    if transaction {
                        self.transaction_uncertain.store(true, Ordering::Release);
                    }
                    self.action_ids.insert(context.id(), (id, transaction));
                } else {
                    tracker.discard(context.id());
                    self.outcome(
                        &id,
                        if accepted {
                            Status::Unknown
                        } else {
                            Status::Refused
                        },
                        Some(if accepted {
                            "queued_without_server_receipt"
                        } else {
                            "precondition_failed_or_network_mailbox_busy"
                        }),
                        None,
                    );
                }
            }
        }
    }
}
/// Milliseconds on the host's monotonic clock, which starts when `run` starts. State carries the
/// current reading as `clock_ms` so a script can compare any `*_at_ms` field with it.
fn clock_ms(started: Instant, at: Instant) -> u64 {
    at.saturating_duration_since(started).as_millis() as u64
}
/// The last self-position update put on the wire. The datagram is unreliable: this proves the
/// attempt and its content, never that the server received it.
fn position_send_json(
    sent: Option<eqoxide_core::game_state::PositionSend>,
    started: Instant,
    now: Instant,
) -> Value {
    sent.map_or(Value::Null, |sent| {
        json!({"sequence":sent.seq,"position":sent.pos,"delta":sent.delta,"heading":sent.heading,
            "animation":sent.animation,"reason":sent.reason.as_str(),"sent_at_ms":clock_ms(started,sent.at),
            "age_ms":now.saturating_duration_since(sent.at).as_millis() as u64,"delivery_confirmed":false})
    })
}
/// Whether the session may run ordinary commands, apart from process shutdown and a terminal
/// network outcome, which the run loop checks itself.
fn host_ready(gs: &GameState, host: &Host, col_loaded: bool, tick_age: u64, link_age: u64, dropped: bool) -> bool {
    gs.player_id != 0
        && gs.player_pos_known
        && !gs.world.zone_in_failed
        && host.body.is_some()
        && col_loaded
        && tick_age < 500
        && link_age < 15000
        && !dropped
        && gs.player_dead_since.is_none()
        // Range checks measure from the last published position, so none can run before one.
        && host.published_position.is_some()
        && !(gs.max_hp > 0 && gs.cur_hp <= 0)
}
/// The received roster within 500 units of the published position, nearest first, ties by spawn
/// ID, capped at 256 (central-6ly1.8). Empty while there is no body to measure from.
fn roster(
    gs: &GameState,
    position: Option<[f32; 3]>,
) -> (Vec<(f32, &eqoxide_core::game_state::Entity)>, bool) {
    let mut nearby: Vec<_> = gs
        .world
        .entities
        .values()
        .filter_map(|entity| {
            distance_from(position, entity)
                .filter(|d| *d <= 500.0)
                .map(|d| (d, entity))
        })
        .collect();
    nearby.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.spawn_id.cmp(&b.1.spawn_id)));
    let truncated = nearby.len() > 256;
    nearby.truncate(256);
    (nearby, truncated)
}
/// Self fields received from the server (central-6ly1.8), beside the local prediction.
fn server_self_fields(gs: &GameState, started: Instant) -> serde_json::Map<String, Value> {
    let value = json!({
        // Null until OP_NewZone: the self spawn arrives first and would pair the destination's
        // position with the departing zone's name.
        "server_position": gs.server_self_position.filter(|_| gs.world.new_zone_applied).map(|p| json!({"position":p.pos,"heading":p.heading,
            "source":p.source.as_str(),"at_ms":clock_ms(started,p.at)})),
        "corrections": {"count":gs.server_corrections,"last":gs.last_server_correction.map(|c|
            json!({"from":c.from,"to":c.to,"at_ms":clock_ms(started,c.at)}))},
        "level": gs.player_level,
        "mana": {"value":gs.cur_mana,"max":gs.max_mana,"known":gs.max_mana>0},
        "endurance": {"value":gs.cur_endurance,"max":gs.max_endurance,"known":gs.max_endurance>0,
            "confirmed":gs.endurance_confirmed},
        "xp": {"pct":gs.xp_pct},
        "buffs": gs.buffs.iter().map(|(slot,b)| json!({"slot":slot,"spell_id":b.spell_id,
            "ticks_remaining":b.duration_ticks})).collect::<Vec<_>>(),
        "skills": gs.player_skills,
        "dead": gs.player_dead_since.is_some(),
        "killer": gs.died_at.map(|_| json!({"id":gs.killer_id,"name":known_killer_name(gs)})),
    });
    match value {
        Value::Object(map) => map,
        _ => unreachable!("built as an object"),
    }
}
/// The killer's name, or null when unknown. `GameState::killed_by` falls back to "something" when
/// the death packet named no known spawn; a self-inflicted death names the player, resolved here
/// so the graphical overlay's wording is unchanged.
fn known_killer_name(gs: &GameState) -> Option<String> {
    if gs.killer_is_self && !gs.player_name.is_empty() {
        return Some(gs.player_name.clone());
    }
    let name = gs.killed_by.as_str();
    (!name.is_empty() && name != "something").then(|| name.to_owned())
}
/// Distance from the published self position, the one value every range check and the roster
/// use (central-6ly1.8). `None` while there is no body to measure from.
fn distance_from(me: Option<[f32; 3]>, entity: &eqoxide_core::game_state::Entity) -> Option<f32> {
    me.map(|p| entity.dist_to(p[0], p[1], p[2]))
}
fn within(me: Option<[f32; 3]>, entity: &eqoxide_core::game_state::Entity, radius: f32) -> bool {
    distance_from(me, entity).is_some_and(|d| d <= radius)
}
fn observed_entity(gs: &GameState, me: Option<[f32; 3]>, id: u32, radius: f32) -> bool {
    gs.world.entities.get(&id).is_some_and(|entity| !entity.dead && within(me, entity, radius))
}
/// An awaited command's outcome, handed back to the host loop. The host publishes it and settles
/// the transaction latch in one step before it dispatches anything else, so a script that sees the
/// receipt can never have its next transaction judged against the old latch.
struct Completion {
    id: String,
    status: Status,
    reason: Option<String>,
    detail: Option<String>,
    evidence: Option<Value>,
    transaction: bool,
}
type Completions = (
    tokio::sync::mpsc::UnboundedSender<Completion>,
    tokio::sync::mpsc::UnboundedReceiver<Completion>,
);

async fn await_outcome<T: serde::Serialize>(
    id: String,
    rx: tokio::sync::oneshot::Receiver<ipc::CommandResult<T>>,
    kind: &str,
    transaction: bool,
) -> Completion {
    let (status, reason, evidence) = match tokio::time::timeout(Duration::from_secs(8), rx).await {
        Ok(Ok(ipc::CommandResult::Resolved(receipt))) => (
            Status::Confirmed,
            None,
            Some(json!({"kind":kind,"receipt":receipt})),
        ),
        // The server's refusal is prose; the receipt's reason is a code (design A2.7).
        // A local claim refusal arrives as its code (`stale_zone_epoch_before_dispatch`); a
        // server's refusal is prose, which goes to `detail` under the code `server_refused`.
        Ok(Ok(ipc::CommandResult::Refused(text))) => {
            let is_code = !text.is_empty() && text.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
            let (reason, detail) = if is_code { (text, None) } else { ("server_refused".to_owned(), Some(text)) };
            return Completion { id, status: Status::Refused, reason: Some(reason), detail, evidence: None, transaction };
        }
        _ => (
            Status::Unknown,
            Some("no_server_evidence_within_window".into()),
            None,
        ),
    };
    Completion { id, status, reason, detail: None, evidence, transaction }
}

fn load_collision(dir: &std::path::Path, zone: &str) -> Result<(Arc<Collision>, Value), String> {
    if zone.is_empty() || !zone.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
        return Err("invalid received zone name".into());
    }
    let path = dir.join(format!("{zone}.map"));
    let limits = NativeMapLimits::default();
    let map = NativeMap::load(&path, &limits).map_err(|e| e.to_string())?;
    let stats = map.stats().clone();
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .map_err(|e| e.to_string())?
        .take((limits.max_file_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > limits.max_file_bytes {
        return Err("map changed beyond file limit".into());
    }
    let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
    let mut col = map.into_collision(32.0).map_err(|e| e.to_string())?;
    let water = RegionMap::try_load(dir, zone).map(Arc::new);
    let region = match &water {
        Ok(map) => {
            json!({"status":"ready","nodes":map.node_count(),"zone_line_indices":map.zone_line_indices()})
        }
        Err(e) => json!({"status":"unavailable","reason":format!("{e:?}")}),
    };
    col.set_region_data(water);
    Ok((
        Arc::new(col),
        json!({"status":"ready","source":"eqemu-map-v2","digest":digest,"triangles":stats.triangles,
        "models":stats.models,"placements":stats.placements,"region":region,"dynamic_doors":"unsupported"}),
    ))
}

pub async fn run(profile: Profile) -> Result<(), String> {
    // Bind before opening any gameplay connection: a busy control port fails without logging in.
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, profile.api_port))
        .await
        .map_err(|e| format!("control bind failed: {e}"))?;
    let session = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "system clock before epoch")?
            .as_nanos()
    );
    let wiring = Wiring::new(profile.maps_dir.clone());
    // 65 536 terminal receipts kept in full, 1 048 576 identities, a 32 768-event buffer (design
    // A3, A4). The control owns the host clock so every `at_ms`, `clock_ms` and packet `at_ms`
    // share one origin.
    let control = Control::new(session, profile.control_token.clone(), 65_536, 32_768)?;
    let started = control.started();
    let trace = eqoxide_net::packet_telemetry::SessionTrace::new(started);
    let control = control.with_packet_trace(trace.clone());
    let router = control.router();
    let api = tokio::spawn(async move { axum::serve(listener, router).await });
    println!("API_PORT={}", profile.api_port);
    let mut host = Host {
        control: control.clone(),
        command: wiring.command,
        controller_slots: wiring.controller,
        body: None,
        zone_in: Default::default(),
        epoch: 0,
        heading: 0.0,
        motion: None,
        ending_motion: None,
        attack_until: None,
        action_ids: HashMap::new(),
        transaction_uncertain: Arc::new(AtomicBool::new(false)),
        inventory_revision: 0,
        merchant_revision: 0,
        inventory_value: Value::Null,
        merchant_value: Value::Null,
        last_event: 0,
        last_echo: 0,
        completions: tokio::sync::mpsc::unbounded_channel(),
        last_combat: 0,
        last_death_count: 0,
        published_position: None,
    };
    let (net_tx, mut net) = tokio::sync::oneshot::channel();
    let config = profile.login_config();
    let expected = profile.expected();
    std::thread::Builder::new()
        .name("headless-net".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| "network runtime initialization failed".to_string())?
                    .block_on(eqoxide_net::transport::with_packet_trace(
                        trace,
                        eqoxide_net::login::run_login_flow_selected(
                            config,
                            1,
                            wiring.slots,
                            wiring.lifecycle,
                            expected,
                        ),
                    ))
            }))
            .unwrap_or_else(|_| Err("network task panicked".into()));
            let _ = net_tx.send(result);
        })
        .map_err(|_| "cannot spawn network owner".to_string())?;
    let shutdown = wiring.shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        shutdown.store(true, Ordering::Release);
    });
    let shutdown = wiring.shutdown.clone();
    tokio::spawn(async move {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
            shutdown.store(true, Ordering::Release);
        }
    });
    let mut interval = tokio::time::interval(Duration::from_millis(20));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut previous = Instant::now();
    let mut geometry = json!({"status":"unavailable","reason":"waiting_for_real_zone_entry"});
    let mut col: Option<Arc<Collision>> = None;
    let mut loaded_zone = String::new();
    let mut publish_at = Instant::now();
    let mut terminal: Option<String> = None;
    let mut shutdown_started: Option<Instant> = None;
    loop {
        interval.tick().await;
        let gs = wiring.snapshot.load_full();
        // Outcomes the server already settled are applied before any fence this tick can run,
        // so a readiness drop or zone change cannot turn a settled buy into `unknown`.
        if host.drain_completions(&gs) {
            publish_at = Instant::now();
        }
        let now = Instant::now();
        let dt = (now - previous).as_secs_f32().min(0.05);
        previous = now;
        if terminal.is_none() {
            match net.try_recv() {
                Ok(result) => {
                    terminal = Some(result.err().unwrap_or_else(|| "session_ended".into()))
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    terminal = Some("network_task_failed".into())
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
            }
            if terminal.is_some() {
                host.drain_completions(&gs);
                host.halt("network_ended", None);
                control.set_ready(false);
                control.publish_event(json!({"kind":"session_ended","reason":terminal}));
            }
        }
        if gs.zone_epoch != host.epoch {
            host.drain_completions(&gs);
            host.halt("zone_entry", None);
            control.set_ready(false);
            host.body = None;
            host.epoch = gs.zone_epoch;
            host.published_position = None;
            host.zone_in = ZoneIn::default();
            col = None;
            loaded_zone.clear();
            *wiring.collision.write().unwrap() = None;
            let mut view = host.controller_slots.controller_view.lock().unwrap();
            view.initialized = false;
            view.zone_epoch = Some(gs.zone_epoch);
            view.landed_fall_height = None;
            view.relocated = None;
        }
        // Native sends the destination's self spawn before OP_NewZone, so the position can be
        // known while `zone_name` still names the zone being left. Wait for the new zone itself.
        if gs.player_id != 0
            && gs.player_pos_known
            && gs.world.new_zone_applied
            && !gs.world.zone_name.is_empty()
            && loaded_zone != gs.world.zone_name
        {
            control.set_ready(false);
            host.halt("loading_geometry", None);
            loaded_zone = gs.world.zone_name.clone();
            match load_collision(&profile.maps_dir, &loaded_zone) {
                Ok((collision, metadata)) => {
                    *wiring.collision.write().unwrap() = Some(collision.clone());
                    col = Some(collision);
                    geometry = metadata;
                }
                Err(reason) => {
                    geometry = json!({"status":"unavailable","reason":reason});
                }
            }
        }
        if host.body.is_none()
            && gs.player_id != 0
            && gs.player_pos_known
            && gs.world.new_zone_applied
            && col.is_some()
        {
            let pos = [gs.player_x, gs.player_y, gs.player_z];
            if pos.iter().all(|v| v.is_finite()) {
                host.body = Some(CharacterController::new(pos));
                host.heading = gs.player_heading;
            }
        }
        let health = wiring.health.lock().unwrap();
        let tick_age = health.last_tick.elapsed().as_millis() as u64;
        let inbound_age = health.last_packet.elapsed().as_millis() as u64;
        let link_age = health.last_datagram.elapsed().as_millis() as u64;
        let dropped = health.session_drop.is_some();
        drop(health);
        let ready = terminal.is_none()
            && !wiring.shutdown.load(Ordering::Acquire)
            && host_ready(&gs, &host, col.is_some(), tick_age, link_age, dropped);
        host.step(&gs, col.as_deref(), ready, now, dt);
        host.update_knowledge(&gs);
        // A completion that arrived during this tick is applied before the publish, after the fences.
        if host.drain_completions(&gs) {
            publish_at = now;
        }
        if now >= publish_at {
            let position = host.body.as_ref().map(|body| body.pos);
            host.published_position = position;
            let last_send = host.controller_slots.controller_view.lock().unwrap().last_position_send;
            // Read the clock after the record, so `sent_at_ms` can never exceed `clock_ms`.
            let state_now = Instant::now();
            let (nearby, truncated) = roster(&gs, position);
            let entities:Vec<_>=nearby.iter().map(|(distance,entity)|json!({"id":entity.spawn_id,"class_id":entity.class_id,"name":entity.name,"npc":entity.is_npc,"dead":entity.dead,
                "position":[entity.x,entity.y,entity.z],"distance":distance,"heading":entity.heading,"equipment":entity.equipment,"hp_pct":entity.hp_pct,"level":entity.level,
                "los":position.zip(col.as_ref()).map(|(p,c)|c.line_clear([p[0],p[1],p[2]+5.0],[entity.x,entity.y,entity.z+5.0],0.0)),"los_source":"local_native_static_geometry"})).collect();
            let mut state = json!({"clock_ms":clock_ms(started,state_now),"phase":if terminal.is_some(){"failed"}else if ready{"ready"}else{"not_ready"},"failure":terminal,
                "freshness":{"network_tick_age_ms":tick_age,"inbound_age_ms":inbound_age,"link_age_ms":link_age},
                "zone":{"id":gs.world.zone_id,"name":gs.world.zone_name,"instance_id":Value::Null},"geometry":geometry,
                "self":{"id":gs.player_id,"name":gs.player_name,"position":position,"position_source":"local_prediction_from_received_self_spawn",
                    "heading":host.heading,"on_ground":host.body.as_ref().map(|b|b.on_ground),"in_water":host.body.as_ref().map(|b|b.in_water),
                    "hp":{"value":gs.cur_hp,"max":gs.max_hp,"known":gs.max_hp>0,"confirmed":gs.hp_confirmed},"sitting":gs.sitting,
                    "last_position_send":position_send_json(last_send,started,state_now),
                    "target_id":gs.target_id,"attack_on":gs.auto_attack,"motion_active":host.motion.is_some(),"hold":gs.player_hold.map(|hold|json!({"reason":hold.reason.as_str(),"seconds":hold.secs}))},
                "nearby":{"available":position.is_some(),"complete":false,"scope":"received_roster_within_radius","radius":500,"limit":256,"truncated":truncated,"entities":entities},
                "inventory":{"known":gs.inventory_received,"revision":host.inventory_revision,"items":gs.inventory,"source":"received_with_local_prediction","coin":gs.coin,"coin_confirmed":gs.coin_confirmed},
                "merchant":{"known":gs.merchant_open.is_some(),"catalog_complete":false,"revision":host.merchant_revision,"open":gs.merchant_open,"items":gs.merchant_items},
                "policy":{"loot":"explicit_only","automatic_zone_crossing":false},
                "transactions_blocked_until_relog":host.transaction_uncertain.load(Ordering::Acquire)
            });
            if let Some(me) = state["self"].as_object_mut() {
                me.extend(server_self_fields(&gs, started));
            }
            control.publish_state(host.epoch, state)?;
            publish_at = now + Duration::from_millis(100);
        }
        control.set_ready(ready);
        for command in control.drain_actions() {
            host.dispatch(command, &gs, ready, now).await;
        }
        if wiring.shutdown.load(Ordering::Acquire) {
            let started = shutdown_started.get_or_insert(now);
            if terminal.is_some() {
                break;
            }
            if now.duration_since(*started) >= Duration::from_secs(8) {
                api.abort();
                return Err("logout completion deadline exceeded; session outcome unknown".into());
            }
        }
        if api.is_finished() {
            host.halt("http_server_ended", None);
            wiring.shutdown.store(true, Ordering::Release);
            return Err("control HTTP server ended".into());
        }
    }
    api.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_zone_geometry::collision::ServerTriangle;
    pub(super) fn host() -> Host {
        host_with_wiring(&Wiring::new("/unused".into()))
    }

    pub(super) fn host_with_wiring(wiring: &Wiring) -> Host {
        Host {
            control: Control::new("test-session".into(), "test-credential".into(), 16, 32).unwrap(),
            command: wiring.command.clone(),
            controller_slots: wiring.controller.clone(),
            body: Some(CharacterController::new([0.0, 0.0, 0.0])),
            zone_in: Default::default(),
            epoch: 1,
            heading: 0.0,
            motion: None,
            ending_motion: None,
            attack_until: None,
            action_ids: HashMap::new(),
            transaction_uncertain: Arc::new(AtomicBool::new(false)),
            inventory_revision: 0,
            merchant_revision: 0,
            inventory_value: Value::Null,
            merchant_value: Value::Null,
            last_event: 0,
            last_echo: 0,
        completions: tokio::sync::mpsc::unbounded_channel(),
        last_combat: 0,
        last_death_count: 0,
        published_position: None,
        }
    }
    pub(super) fn floor() -> Collision {
        Collision::from_server_triangles(
            vec![
                ServerTriangle([
                    [-100.0, -100.0, 0.0],
                    [100.0, -100.0, 0.0],
                    [100.0, 100.0, 0.0],
                ]),
                ServerTriangle([
                    [-100.0, -100.0, 0.0],
                    [100.0, 100.0, 0.0],
                    [-100.0, 100.0, 0.0],
                ]),
            ],
            32.0,
        )
        .unwrap()
    }
    #[test]
    fn movement_and_attack_expire_without_a_caller() {
        let mut host = host();
        let col = floor();
        let start = Instant::now();
        let mut gs = GameState::new();
        gs.zone_epoch = 1;
        gs.world.zone_name = "synthetic".into();
        host.motion = Some(Motion {
            id: "one".into(),
            wish: [1.0, 0.0],
            until: start + Duration::from_millis(200),
        });
        host.attack_until = Some(start + Duration::from_millis(200));
        host.step(
            &gs,
            Some(&col),
            true,
            start + Duration::from_millis(100),
            0.1,
        );
        let moving = host.body.as_ref().unwrap().pos;
        assert!(moving[0] > 0.0);
        for i in 2..20 {
            host.step(
                &gs,
                Some(&col),
                true,
                start + Duration::from_millis(i * 100),
                0.1,
            );
        }
        assert_eq!(host.body.as_ref().unwrap().pos, moving);
        assert!(host.motion.is_none());
        assert!(host.attack_until.is_none());
        assert_eq!(host.command.take_attack(), Some(false));
    }
    #[test]
    fn network_loss_clears_wish_and_does_not_initialize_a_body() {
        let mut host = host();
        host.body = None;
        let now = Instant::now();
        host.motion = Some(Motion {
            id: "move".into(),
            wish: [1.0, 0.0],
            until: now + Duration::from_secs(2),
        });
        host.attack_until = Some(now + Duration::from_secs(5));
        host.step(&GameState::new(), None, false, now, 0.02);
        assert!(host.body.is_none());
        assert!(host.motion.is_none());
        assert!(host.attack_until.is_none());
        assert!(
            !host
                .controller_slots
                .controller_view
                .lock()
                .unwrap()
                .initialized
        );
    }
    /// central-vgwu: the step count tells the net streamer a physics step happened; a tick with
    /// no collision does not step the body and must not count.
    #[test]
    fn controller_view_counts_only_real_physics_steps() {
        let mut host = host();
        let col = floor();
        let mut gs = GameState::new();
        gs.zone_epoch = 1;
        fn host_step_seq(host: &Host) -> u64 {
            host.controller_slots.controller_view.lock().unwrap().step_seq
        }
        host.step(&gs, Some(&col), true, Instant::now(), 0.02);
        host.step(&gs, Some(&col), true, Instant::now(), 0.02);
        assert_eq!(host_step_seq(&host), 2);
        host.step(&gs, None, true, Instant::now(), 0.02);
        assert_eq!(host_step_seq(&host), 2, "an unstepped republish is not a step");
    }
    /// central-vgwu: the host reports which correction it applied, with the first position after
    /// applying it, so the net streamer can tell an adopted correction from a stale view.
    #[test]
    fn host_publishes_the_correction_it_adopted() {
        let mut host = host();
        let col = floor();
        let mut gs = GameState::new();
        gs.zone_epoch = 1;
        {
            let mut view = host.controller_slots.controller_view.lock().unwrap();
            view.correction_issued = 3;
            *host.controller_slots.pos_correction.lock().unwrap() = Some([40.0, 0.0, 0.0]);
        }
        host.step(&gs, Some(&col), true, Instant::now(), 0.02);
        let view = host.controller_slots.controller_view.lock().unwrap().clone();
        assert_eq!(view.correction_adopted, 3);
        assert!((view.pos[0] - 40.0).abs() < 0.01, "published with the corrected position");
    }
    /// central-vgwu review round 4: a step that took an older correction must not roll back a
    /// later one that zone entry settled while the step ran.
    #[test]
    fn an_in_flight_step_cannot_roll_back_a_settled_correction() {
        let mut host = host();
        let col = floor();
        let mut gs = GameState::new();
        gs.zone_epoch = 1;
        {
            let mut view = host.controller_slots.controller_view.lock().unwrap();
            view.correction_issued = 1;
            *host.controller_slots.pos_correction.lock().unwrap() = Some([40.0, 0.0, 0.0]);
        }
        // The race's end state: zone entry has already settled correction 2 while this step
        // holds correction 1, stamped with the count it read when it took the slot.
        host.controller_slots.controller_view.lock().unwrap().correction_adopted = 2;
        host.step(&gs, Some(&col), true, Instant::now(), 0.02);
        let view = host.controller_slots.controller_view.lock().unwrap().clone();
        assert_eq!(view.correction_adopted, 2, "adoption never moves backwards");
    }
    fn rat(id: u32, x: f32) -> eqoxide_core::game_state::Entity {
        eqoxide_core::game_state::make_entity(id, "a_rat", x, 0.0, 0.0, true)
    }
    /// central-6ly1.8: range checks and the roster measure from the published position, not from
    /// the net mirror `gs.player_x`, which can hold a different value (a server echo, a correction
    /// not yet adopted).
    #[test]
    fn range_checks_and_roster_use_the_published_position() {
        let mut gs = GameState::new();
        gs.player_pos_known = true;
        gs.player_x = 1000.0; // the mirror disagrees with the body
        gs.world.entities.insert(7, rat(7, 25.0));
        for id in (8..40).rev() {
            gs.world.entities.insert(id, rat(id, 10.0));
        }
        let me = Some([0.0, 0.0, 0.0]);
        assert!(observed_entity(&gs, me, 7, 30.0), "25 units from the body is in range");
        assert!(!observed_entity(&gs, None, 7, 30.0), "no body, no range");
        let (nearby, truncated) = roster(&gs, me);
        let order: Vec<_> = nearby.iter().map(|(_, e)| e.spawn_id).collect();
        let mut expected: Vec<u32> = (8..40).collect();
        expected.push(7);
        assert_eq!(order, expected, "nearest first, ties by spawn ID");
        assert_eq!(nearby.last().unwrap().0, 25.0, "and each entry carries its distance");
        assert!(!truncated);
        assert!(roster(&gs, None).0.is_empty());
    }
    /// central-6ly1.8: a dispatched command measures range from the position the last state
    /// publication showed, not from the body (which steps between publications) or the mirror.
    ///
    /// MUTATION CHECK: set `me` in `dispatch` from `self.body` or from `gs.player_x` → RED.
    #[tokio::test]
    async fn dispatch_measures_range_from_the_published_position() {
        use tower::ServiceExt;
        let mut host = host();
        host.control.publish_state(1, json!({})).unwrap();
        host.control.set_ready(true);
        host.body = Some(CharacterController::new([1000.0, 0.0, 0.0]));
        host.published_position = Some([0.0, 0.0, 0.0]);
        let mut gs = GameState::new();
        gs.zone_epoch = 1;
        gs.player_x = -1000.0;
        gs.world.entities.insert(7, rat(7, 25.0));
        let body = json!({"session":"test-session","zone_epoch":1,"operation":{"kind":"target","entity_id":7}});
        let request = axum::http::Request::builder().method("PUT").uri("/v2/player/commands/t1")
            .header("authorization", "Bearer test-credential").header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string())).unwrap();
        let response = host.control.router().oneshot(request).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
        for command in host.control.drain_actions() {
            host.dispatch(command, &gs, true, Instant::now()).await;
        }
        let receipt = host.control.command("t1").unwrap();
        assert_ne!(receipt.status, Status::Refused,
            "25 units from the published position is in range: {receipt:?}");
    }
    /// central-6ly1.8: no ordinary command runs before a publication has shown the position its
    /// range checks measure from.
    ///
    /// MUTATION CHECK: drop `host.published_position.is_some()` from `host_ready` → RED.
    #[test]
    fn readiness_waits_for_a_published_position() {
        let mut host = host();
        let mut gs = GameState::new();
        gs.player_id = 1;
        gs.player_pos_known = true;
        assert!(!host_ready(&gs, &host, true, 0, 0, false));
        host.published_position = Some([0.0; 3]);
        assert!(host_ready(&gs, &host, true, 0, 0, false));
    }
    /// central-6ly1.8: the server's view of self beside the prediction, and the vitals.
    #[test]
    fn state_publishes_server_position_corrections_and_vitals() {
        use eqoxide_core::game_state::{ServerCorrection, ServerPositionSource, ServerSelfPosition};
        let started = Instant::now();
        let at = started + Duration::from_millis(250);
        let mut gs = GameState::new();
        gs.world.new_zone_applied = true;
        gs.server_self_position = Some(ServerSelfPosition {
            pos: [1.0, 2.0, 3.0], heading: 90.0, source: ServerPositionSource::Correction, at });
        gs.server_corrections = 2;
        gs.last_server_correction = Some(ServerCorrection { from: [0.0; 3], to: [1.0, 2.0, 3.0], at });
        gs.player_level = 4;
        gs.cur_mana = 10;
        gs.max_mana = 20;
        gs.died_at = Some(at);
        gs.player_dead_since = Some(at);
        gs.killed_by = "something".into();
        gs.killer_id = Some(55);
        let f = server_self_fields(&gs, started);
        assert_eq!(f["server_position"], json!({"position":[1.0,2.0,3.0],"heading":90.0,"source":"server_correction","at_ms":250}));
        assert_eq!(f["corrections"], json!({"count":2,"last":{"from":[0.0,0.0,0.0],"to":[1.0,2.0,3.0],"at_ms":250}}));
        assert_eq!(f["level"], 4);
        assert_eq!(f["mana"], json!({"value":10,"max":20,"known":true}));
        assert_eq!(f["dead"], true);
        assert_eq!(f["killer"], json!({"id":55,"name":null}), "the id survives an unknown name");
        gs.killer_is_self = true;
        gs.player_id = 56; // re-entered under a new spawn ID: still a self-inflicted death
        gs.player_name = "Me".into();
        assert_eq!(server_self_fields(&gs, started)["killer"], json!({"id":55,"name":"Me"}),
            "a self-inflicted death names the player without changing killed_by");
        assert_eq!(f["server_position"]["source"], "server_correction");
        gs.world.new_zone_applied = false;
        assert_eq!(server_self_fields(&gs, started)["server_position"], Value::Null,
            "a self spawn that arrived before OP_NewZone is not paired with the old zone");
    }
    /// central-6ly1.8: each received swing becomes one structured event, and a death one `died`.
    #[test]
    fn combat_and_death_are_published_once_each() {
        use eqoxide_core::game_state::{CombatRecord, CombatResult};
        let mut host = host();
        let mut gs = GameState::new();
        gs.player_id = 1;
        gs.player_name = "Me".into();
        gs.world.entities.insert(7, rat(7, 5.0));
        let swing = |id| CombatRecord { id, source_id: 1, target_id: 7, amount: 3, spell_id: (id == 2).then_some(13),
            damage_type: 1, result: CombatResult::Hit, source_name: Some("Me".into()),
            target_name: Some("a_rat".into()), zone_epoch: 1, at: Instant::now() };
        gs.combat_records.push_back(swing(1));
        gs.combat_records.push_back(swing(2));
        host.update_knowledge(&gs);
        host.update_knowledge(&gs);
        gs.died_at = Some(Instant::now());
        gs.death_count = 1;
        gs.killer_id = Some(7);
        gs.killed_by = "a_rat".into();
        // The roster now names spawn 7 differently: the record keeps the name from its packet.
        gs.world.entities.get_mut(&7).unwrap().name = "a_guard".into();
        host.update_knowledge(&gs);
        host.update_knowledge(&gs);
        let batch = host.control.events_after_for_test(0);
        let kinds: Vec<_> = batch.iter().map(|e| e["kind"].as_str().unwrap().to_owned()).collect();
        assert_eq!(kinds, vec!["combat", "combat", "died"]);
        assert_eq!(batch[0], json!({"kind":"combat","source":"received_packet","record_id":1,"zone_epoch":1,
            "source_id":1,"target_id":7,"source_name":"Me","target_name":"a_rat","amount":3,"spell_id":null,
            "damage_type":1,"result":"hit"}));
        assert_eq!(batch[2]["killer"], json!({"id":7,"name":"a_rat"}));
        assert_eq!(batch[1]["spell_id"], 13, "the spell ID is published");
        assert_eq!(batch[2]["death_count"], 1);

        // Records evicted before the host saw them, and two deaths between reads, are reported.
        gs.combat_records.clear();
        gs.combat_records.push_back(swing(9));
        gs.death_count = 3;
        host.update_knowledge(&gs);
        let batch = host.control.events_after_for_test(0);
        let tail: Vec<_> = batch[3..].iter().map(|e| (e["kind"].as_str().unwrap().to_owned(),
            e.get("source").and_then(Value::as_str).unwrap_or_default().to_owned(),
            e.get("count").cloned().unwrap_or(Value::Null))).collect();
        assert_eq!(tail, vec![
            ("source_events_dropped".to_owned(), "combat".to_owned(), json!(6)),
            ("combat".to_owned(), "received_packet".to_owned(), Value::Null),
            ("source_events_dropped".to_owned(), "death".to_owned(), json!(1)),
            ("died".to_owned(), "received_packet".to_owned(), Value::Null),
        ]);
    }
    #[test]
    fn state_publishes_the_last_position_send() {
        use eqoxide_core::game_state::{PositionSend, PositionSendReason};
        let started = Instant::now();
        assert_eq!(position_send_json(None, started, started), Value::Null);
        let at = started + Duration::from_millis(1500);
        let sent = Some(PositionSend {
            seq: 41, pos: [1.0, 2.0, 3.0], delta: [0.0; 3], heading: 90.0, animation: 0,
            reason: PositionSendReason::Rest, at,
        });
        assert_eq!(
            position_send_json(sent, started, at + Duration::from_millis(20)),
            json!({"sequence":41,"position":[1.0,2.0,3.0],"delta":[0.0,0.0,0.0],"heading":90.0,
                "animation":0,"reason":"rest","sent_at_ms":1500,"age_ms":20,"delivery_confirmed":false})
        );
    }
    #[test]
    fn fenced_controller_cannot_stream_previous_zone() {
        let mut host = host();
        let col = floor();
        let mut gs = GameState::new();
        gs.zone_epoch = 1;
        host.controller_slots
            .controller_view
            .lock()
            .unwrap()
            .zone_epoch = Some(2);
        host.step(&gs, Some(&col), true, Instant::now(), 0.02);
        assert!(
            !host
                .controller_slots
                .controller_view
                .lock()
                .unwrap()
                .initialized
        );
    }
}

#[cfg(test)]
mod transaction_admission_tests {
    use super::*;
    #[tokio::test]
    async fn refused_transaction_does_not_poison_later_work() {
        let wiring = Wiring::new("/unused".into());
        let mut host = Host {
            control: Control::new("s".into(), "credential".into(), 8, 16).unwrap(),
            command: wiring.command,
            controller_slots: wiring.controller,
            body: None,
            zone_in: Default::default(),
            epoch: 1,
            heading: 0.0,
            motion: None,
            ending_motion: None,
            attack_until: None,
            action_ids: HashMap::new(),
            transaction_uncertain: Arc::new(AtomicBool::new(false)),
            inventory_revision: 0,
            merchant_revision: 0,
            inventory_value: Value::Null,
            merchant_value: Value::Null,
            last_event: 0,
            last_echo: 0,
        completions: tokio::sync::mpsc::unbounded_channel(),
        last_combat: 0,
        last_death_count: 0,
        published_position: None,
        };
        let mut gs = GameState::new();
        gs.inventory_received = true;
        host.control
            .publish_state(1, json!({"phase":"ready"}))
            .unwrap();
        host.control.set_ready(true);
        for (index, operation) in [
            Operation::Sell {
                slot: 23,
                quantity: 1,
            },
            Operation::InventoryMove {
                from_slot: 23,
                to_slot: 24,
                quantity: 1,
            },
        ]
        .into_iter()
        .enumerate()
        {
            use tower::ServiceExt;
            let id = format!("rejected-{index}");
            let request = crate::control::CommandRequest {
                session: "s".into(),
                zone_epoch: 1,
                inventory_revision: Some(0),
                merchant_revision: Some(0),
                operation: operation.clone(),
            };
            host.control
                .router()
                .oneshot(
                    axum::http::Request::builder()
                        .method("PUT")
                        .uri(format!("/v2/player/commands/{id}"))
                        .header("authorization", "Bearer credential")
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(
                            serde_json::to_vec(&request).unwrap(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            let admitted = host
                .control
                .drain_actions()
                .pop()
                .expect("admitted test transaction");
            host.dispatch(admitted, &gs, true, Instant::now()).await;
            assert!(!host.transaction_uncertain.load(Ordering::Acquire));
        }
        // A command accepted into a mailbox can still be definitively rejected before send.
        let tracker = host.command.actions();
        let context = tracker.begin_for_zone("inventory", 1);
        assert!(
            context
                .scope(async { host.command.request_inventory_move(23, 24) })
                .await
        );
        host.action_ids
            .insert(context.id(), ("queued-tx".into(), true));
        host.transaction_uncertain.store(true, Ordering::Release);
        tracker.set_zone_epoch(2);
        assert!(host.command.take_inventory_move().is_none());
        host.update_knowledge(&gs);
        assert!(!host.transaction_uncertain.load(Ordering::Acquire));
    }
}

#[cfg(test)]
mod execution_claim_tests {
    use super::*;
    use tower::ServiceExt;
    async fn put(control: &Control, id: &str, operation: Value) {
        control
            .router()
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri(format!("/v2/player/commands/{id}"))
                    .header("authorization", "Bearer test-credential")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        json!({
                "session":"test-session","zone_epoch":1,"operation":operation})
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn superseded_drained_attack_never_enters_network_mailbox() {
        let mut host = super::tests::host();
        host.control.publish_state(1, json!({})).unwrap();
        host.control.set_ready(true);
        put(
            &host.control,
            "attack",
            json!({"kind":"attack","lease_ms":1000}),
        )
        .await;
        let attack = host.control.drain_actions().pop().unwrap();
        put(&host.control, "off", json!({"kind":"attack_off"})).await;
        let mut gs = GameState::new();
        gs.target_id = Some(7);
        gs.upsert_entity(eqoxide_core::game_state::make_entity(
            7, "rat", 0.0, 0.0, 0.0, true,
        ));
        host.dispatch(attack, &gs, true, Instant::now()).await;
        assert!(host.command.take_attack().is_none());
        assert_eq!(
            host.control.command("attack").unwrap().status,
            Status::Refused
        );
    }
    #[tokio::test]
    async fn stale_awaited_buy_returns_definite_refusal_and_releases_transaction_latch() {
        let host = super::tests::host();
        host.control.publish_state(1, json!({})).unwrap();
        host.control.set_ready(true);
        put(
            &host.control,
            "buy",
            json!({"kind":"buy","slot":1,"quantity":1}),
        )
        .await;
        host.control.drain_actions();
        host.control.mark_dispatched("buy").unwrap();
        let tracker = host.command.actions();
        let context = tracker.begin_for_zone("buy", 1);
        let (tx, rx) = tokio::sync::oneshot::channel();
        assert!(
            context
                .scope(async { host.command.request_buy_await(7, 1, tx) })
                .await
        );
        host.transaction_uncertain.store(true, Ordering::Release);
        tracker.set_zone_epoch(2);
        assert!(host.command.take_buy_await().is_none());
        host.apply_completion(await_outcome("buy".into(), rx, "server_buy_receipt", true).await);
        assert!(!host.transaction_uncertain.load(Ordering::Acquire));
        assert_eq!(host.control.command("buy").unwrap().status, Status::Refused);
    }

    /// central-6ly1.1 review: a buy fenced to `unknown` keeps transactions blocked even if its
    /// server receipt arrives later; the receipt is terminal, and so is the uncertainty.
    #[tokio::test]
    async fn a_late_receipt_for_a_fenced_buy_keeps_transactions_blocked() {
        let host = super::tests::host();
        host.control.publish_state(1, json!({})).unwrap();
        host.control.set_ready(true);
        put(&host.control, "buy", json!({"kind":"buy","slot":1,"quantity":1})).await;
        host.control.drain_actions();
        host.control.mark_dispatched("buy").unwrap();
        host.transaction_uncertain.store(true, Ordering::Release);
        host.control.set_ready(false); // fence: dispatched, so unknown
        assert_eq!(host.control.command("buy").unwrap().status, Status::Unknown);
        let (tx, rx) = tokio::sync::oneshot::channel();
        tx.send(ipc::CommandResult::Resolved(json!({"late":true}))).unwrap();
        host.apply_completion(await_outcome("buy".into(), rx, "server_buy_receipt", true).await);
        assert_eq!(host.control.command("buy").unwrap().status, Status::Unknown);
        assert!(host.transaction_uncertain.load(Ordering::Acquire), "transactions stay blocked");
    }

    /// Review of PR #6: an awaited outcome becomes visible only when the host applies it, in the
    /// same step as the latch, so a script that sees `confirmed` can send its next transaction
    /// without it being judged against the old latch.
    #[tokio::test]
    async fn an_awaited_outcome_and_its_latch_settle_together() {
        let mut host = super::tests::host();
        host.control.publish_state(1, json!({})).unwrap();
        host.control.set_ready(true);
        put(&host.control, "buy", json!({"kind":"buy","slot":1,"quantity":1})).await;
        host.control.drain_actions();
        host.control.mark_dispatched("buy").unwrap();
        host.transaction_uncertain.store(true, Ordering::Release);
        let (tx, rx) = tokio::sync::oneshot::channel();
        tx.send(ipc::CommandResult::Resolved(json!({"ok":true}))).unwrap();
        let done = await_outcome("buy".into(), rx, "server_buy_receipt", true).await;
        host.completions.0.send(done).unwrap();
        assert_eq!(host.control.command("buy").unwrap().status, Status::Accepted,
            "not visible until the host loop applies it");
        host.drain_completions(&Arc::new(GameState::new()));
        assert_eq!(host.control.command("buy").unwrap().status, Status::Confirmed);
        assert!(!host.transaction_uncertain.load(Ordering::Acquire), "and the latch cleared with it");
    }

    /// Review of PR #6: a server refusal's prose goes to `detail`; `reason` is a code.
    #[tokio::test]
    async fn a_server_refusal_has_a_code_and_prose_detail() {
        let host = super::tests::host();
        host.control.publish_state(1, json!({})).unwrap();
        host.control.set_ready(true);
        put(&host.control, "buy", json!({"kind":"buy","slot":1,"quantity":1})).await;
        host.control.drain_actions();
        host.control.mark_dispatched("buy").unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<ipc::CommandResult<Value>>();
        tx.send(ipc::CommandResult::Refused("merchant refused".into())).unwrap();
        host.apply_completion(await_outcome("buy".into(), rx, "server_buy_receipt", true).await);
        let receipt = host.control.command("buy").unwrap();
        assert_eq!((receipt.status, receipt.reason.as_deref(), receipt.detail.as_deref()),
            (Status::Refused, Some("server_refused"), Some("merchant refused")));
        let (tx, rx) = tokio::sync::oneshot::channel::<ipc::CommandResult<Value>>();
        tx.send(ipc::CommandResult::Refused("stale_zone_epoch_before_dispatch".into())).unwrap();
        let done = await_outcome("x".into(), rx, "server_buy_receipt", true).await;
        assert_eq!((done.reason.as_deref(), done.detail), (Some("stale_zone_epoch_before_dispatch"), None),
            "a local claim refusal keeps its own code");
    }

    /// Review of PR #6: the same holds once the fenced receipt has been evicted.
    #[tokio::test]
    async fn a_late_receipt_after_eviction_keeps_transactions_blocked() {
        let mut host = super::tests::host();
        host.control = Control::new("test-session".into(), "test-credential".into(), 1, 100).unwrap();
        host.control.publish_state(1, json!({})).unwrap();
        host.control.set_ready(true);
        put(&host.control, "buy", json!({"kind":"buy","slot":1,"quantity":1})).await;
        host.control.drain_actions();
        host.control.mark_dispatched("buy").unwrap();
        host.transaction_uncertain.store(true, Ordering::Release);
        host.control.set_ready(false);
        host.control.set_ready(true);
        put(&host.control, "walk", json!({"kind":"move_for","east":1,"north":0,"duration_ms":100})).await;
        host.control.set_ready(false); // a second terminal receipt evicts the buy
        assert!(host.control.command("buy").is_none(), "premise: the buy was evicted");
        let (tx, rx) = tokio::sync::oneshot::channel();
        tx.send(ipc::CommandResult::Resolved(json!({"late":true}))).unwrap();
        host.apply_completion(await_outcome("buy".into(), rx, "server_buy_receipt", true).await);
        assert!(host.transaction_uncertain.load(Ordering::Acquire), "an evicted unknown still blocks");
    }

    /// central-6ly1.1 review: game events and self echoes evicted from their bounded source queues
    /// before the host saw them are reported.
    #[test]
    fn evicted_source_events_are_reported() {
        let mut host = super::tests::host();
        host.control = Control::new("test-session".into(), "test-credential".into(), 16, 1000).unwrap();
        let mut gs = GameState::new();
        for _ in 0..205 {
            gs.push_event("chat", "say", "someone", false, "hello");
        }
        host.update_knowledge(&gs);
        let events = host.control.events_after_for_test(0);
        assert_eq!(events[0], json!({"kind":"source_events_dropped","source":"game_event","count":5}));
        let mut gs = GameState::new();
        for n in 0..40u64 {
            gs.received_self_echoes.push_back(eqoxide_core::game_state::ReceivedSelfEcho {
                id: n + 1, channel: 8, sender: "Me".into(), text: "hi".into(), truncated: false,
            });
        }
        while gs.received_self_echoes.len() > 32 {
            gs.received_self_echoes.pop_front();
        }
        host.update_knowledge(&gs);
        let events = host.control.events_after_for_test(0);
        assert!(events.contains(&json!({"kind":"source_events_dropped","source":"self_echo","count":8})));
    }

    /// Review of PR #6: the action-tracker path follows the receipt as it stands too.
    #[tokio::test]
    async fn a_late_tracker_confirmation_cannot_reopen_transactions() {
        let mut host = super::tests::host();
        host.control.publish_state(1, json!({})).unwrap();
        host.control.set_ready(true);
        put(&host.control, "sell", json!({"kind":"sell","slot":1,"quantity":1})).await;
        host.control.drain_actions();
        host.control.mark_dispatched("sell").unwrap();
        host.transaction_uncertain.store(true, Ordering::Release);
        host.control.set_ready(false); // fenced to unknown
        let tracker = host.command.actions();
        let context = tracker.begin_for_zone("sell", 1);
        assert!(context.scope(async { host.command.request_sit(true) }).await, "premise: accepted");
        host.action_ids.insert(context.id(), ("sell".into(), true));
        tracker.finish(context.id(), "confirmed", "late");
        host.update_knowledge(&GameState::new());
        assert_eq!(host.control.command("sell").unwrap().status, Status::Unknown);
        assert!(host.transaction_uncertain.load(Ordering::Acquire));
    }
}

#[cfg(test)]
#[path = "runtime_packet_tests.rs"]
mod packet_tests;
