//! Combat command verbs — the fully-migrated reference for the Wave-2 fan-out (see `mod.rs`).
//!
//! Domain: `/v1/combat/*` targeting, auto-attack, consider, spell cast/memorize/scribe, plus the
//! one `/v1/pet/command` slot (which rides in `ipc::CombatSlots` — see its doc). Every method is a
//! thin typed read/write of a slot in `self.combat`; validation and packet-building stay where they
//! were (the HTTP handler and `ActionLoop::tick`). No behavior change — just one typed surface.

use super::actions::TracedRequest;
use eqoxide_ipc::{AttackOffOutcome, AttackOffSettled, ExplicitAttackOff};

/// Explicit offs sent per network drain at most.
pub const EXPLICIT_OFFS_PER_DRAIN: usize = 8;

/// What the host abandons when it reads explicit-off outcomes.
#[derive(Clone, Copy, Debug)]
pub enum Abandon {
    None,
    OtherZones(u64),
    All,
}

/// Remove the explicit offs accepted in a zone other than `current`.
fn drain_stale(offs: &mut Vec<ExplicitAttackOff>, current: u64) -> Vec<ExplicitAttackOff> {
    let (stale, live) = std::mem::take(offs).into_iter()
        .partition(|off| off.zone_epoch.is_some_and(|epoch| epoch != current));
    *offs = live;
    stale
}

/// One drained attack request; see [`CommandState::take_attack_request`].
#[derive(Debug)]
pub struct AttackDrain {
    pub on: bool,
    /// Attribution for an implicit off, or for an on.
    pub command: Option<std::sync::Arc<str>>,
    /// Explicit offs, in request order. Non-empty only when `on` is false.
    pub explicit: Vec<ExplicitAttackOff>,
}
use super::{Action, CommandState};
use eqoxide_ipc::{CastEnd, CastRequest, CommandResult};
use tokio::sync::oneshot;

impl CommandState {
    // ── request_* : the VIEW (UI click-handlers + HTTP handlers) makes these writes ──────────────

    /// Target a spawn (POST /v1/combat/target{,/name}, the Target/Actions windows). The drain
    /// (`take_target`) auto-considers it. Caller validates the id is in-zone first.
    pub fn request_target(&self, spawn_id: u32) -> bool {
        self.enqueue(&self.combat.target, spawn_id, false, Action::CombatTarget)
    }

    /// Toggle auto-attack (POST/DELETE /v1/combat/attack, the Attack button). `on` = engage.
    /// An off here is explicit: it is sent even if the client believes attack is already off. It
    /// carries no packet attribution (packet command IDs are v2 command IDs) and no zone.
    ///
    /// An off is also accepted over a queued off (an implicit one from a halt, or another explicit
    /// one): it adds its own explicit entry, so a v1 `DELETE` during zoning is not refused as busy.
    /// Anything else over a queued request is refused, as for every single-slot mailbox.
    pub fn request_attack(&self, on: bool) -> bool {
        let key = std::sync::Arc::as_ptr(&self.combat.attack) as usize;
        let mut slot = self.combat.attack.lock().unwrap();
        match (*slot, on) {
            (Some(false), false) | (None, false) => {
                self.combat.attack_off_explicit.lock().unwrap().push(ExplicitAttackOff::default());
                *slot = Some(false);
            }
            (None, true) => *slot = Some(true),
            (Some(_), _) => return false,
        }
        self.actions.accept(key, false, Action::CombatAttack);
        true
    }

    /// Implicit safety off (a halt, lease expiry, sit, camp), last-wins over a queued on. The
    /// network owner sends it only while attack is on; with attack off it is drained and nothing
    /// goes on the wire. An overwritten attack must never lend its ID to the off packet.
    ///
    /// Over a queued off it changes no payload. With an explicit off queued it does nothing: that
    /// off goes out and owns its packet. Over an implicit off, a command ID (`sit`, `camp`) takes
    /// the packet's attribution, and no ID (a halt) never erases one.
    pub fn overwrite_attack_off(&self, command: Option<&str>) {
        let key = std::sync::Arc::as_ptr(&self.combat.attack) as usize;
        let mut slot = self.combat.attack.lock().unwrap();
        if *slot == Some(false) {
            if command.is_some() && self.combat.attack_off_explicit.lock().unwrap().is_empty() {
                self.actions.set_packet_command(key, command);
            }
            return;
        }
        *slot = Some(false);
        self.actions.replace_with_safety(key, command);
    }

    /// Explicit off (the `attack_off` command), last-wins over a queued on, and always sent while
    /// its zone lasts: it is a script's safety stop and a tester's way to send a redundant off.
    /// Each call is its own packet, attributed to `command`. An off accepted in zone `zone_epoch`
    /// and drained after that zone ended is dropped: the new zone's server starts with attack off.
    pub fn force_attack_off(&self, command: Option<&str>, zone_epoch: Option<u64>) {
        let key = std::sync::Arc::as_ptr(&self.combat.attack) as usize;
        let mut slot = self.combat.attack.lock().unwrap();
        if *slot != Some(false) {
            *slot = Some(false);
            self.actions.replace_with_safety(key, None);
        }
        self.combat.attack_off_explicit.lock().unwrap().push(ExplicitAttackOff {
            command: command.map(std::sync::Arc::from),
            zone_epoch,
        });
    }

    /// Consider a spawn (POST /v1/combat/consider, the Consider button) — con color/faction reply.
    pub fn request_consider(&self, spawn_id: u32) -> bool {
        self.enqueue(&self.combat.consider, spawn_id, false, Action::CombatConsider)
    }

    /// Cast a memorized gem, a spell id, or an item clicky (POST /v1/combat/cast, the spell-gem /
    /// spellbook windows). The handler builds the [`CastRequest`]; the drain resolves the target.
    pub fn request_cast(&self, req: CastRequest) -> bool {
        self.enqueue(&self.combat.cast, req, false, Action::CombatCast)
    }

    /// Command-with-result cast (A3 Migration 3, #448): queue the SAME cast as `request_cast` but hand
    /// in a `oneshot::Sender<CommandResult<CastEnd>>` the net thread fulfils with the TRUE outcome.
    /// POST /v1/combat/cast awaits the matching receiver under a timeout so it reports the real result
    /// (completed / fizzled / interrupted / refused / unconfirmed) instead of a premature queued 200.
    /// Writes the sibling `cast_await` slot — the fire-and-forget `cast` slot (UI path) is left
    /// untouched. See `crate::command_state::result`.
    pub fn request_cast_await(&self, req: CastRequest, tx: oneshot::Sender<CommandResult<CastEnd>>) -> bool {
        self.enqueue(&self.combat.cast_await, (req, tx), true, Action::CombatCastAwait)
    }

    /// Memorize a known spell (`scribing = 1`) or scribe a scroll (`scribing = 0`) into a book/gem
    /// `slot` (POST /v1/combat/{memorize,scribe}). `from` is the scroll's inventory wire slot for a
    /// scribe (moved to cursor first by the drain), `None` for a memorize. Tuple shape preserved
    /// verbatim from `ipc::MemSpellReq`.
    pub fn request_mem_spell(&self, slot: u32, spell_id: u32, scribing: u32, from: Option<u32>) -> bool {
        self.enqueue(&self.combat.mem_spell, (slot, spell_id, scribing, from), false, Action::CombatMemSpell)
    }

    /// Queue one OP_PetCommands byte (POST /v1/pet/command, the Pet window). See `PET_*` constants.
    pub fn request_pet_command(&self, cmd: u8) -> bool {
        self.enqueue(&self.combat.pet_cmd, cmd, false, Action::CombatPetCmd)
    }

    // ── take_* : the MODEL (`ActionLoop::tick`) drains these once per tick ────────────────────────

    /// Drain a pending target request. `Some(spawn_id)` if one was queued since the last tick.
    pub fn take_target(&self) -> Option<u32> {
        self.dequeue(&self.combat.target)
    }

    pub fn take_target_with_command(&self) -> Option<TracedRequest<u32>> {
        self.dequeue_with_command(&self.combat.target)
    }

    /// Drain a pending auto-attack toggle. `Some(true)` = engage, `Some(false)` = disengage.
    pub fn take_attack(&self) -> Option<bool> {
        self.take_attack_request(None).map(|drain| drain.on)
    }

    pub fn take_attack_with_command(&self) -> Option<TracedRequest<bool>> {
        self.take_attack_request(None).map(|drain| (drain.on, drain.command))
    }

    /// Drain the attack slot: the payload, the implicit packet attribution, and every explicit off
    /// queued with it, all taken under the slot lock so they belong to this payload. With
    /// `zone_epoch`, an explicit off accepted in another zone is not returned: it is settled as
    /// not sent, under the same lock, so the host can publish its receipt.
    pub fn take_attack_request(&self, zone_epoch: Option<u64>) -> Option<AttackDrain> {
        let mut slot = self.combat.attack.lock().unwrap();
        let on = slot.take()?;
        let mut explicit = std::mem::take(&mut *self.combat.attack_off_explicit.lock().unwrap());
        if let Some(current) = zone_epoch {
            let stale = drain_stale(&mut explicit, current);
            self.settle_attack_offs(stale, AttackOffOutcome::ZoneEnded);
        }
        // A burst of explicit offs goes out a few per network tick, so the transport processes
        // acknowledgements between them; the rest stay queued with the off still in the slot.
        if explicit.len() > EXPLICIT_OFFS_PER_DRAIN {
            let rest = explicit.split_off(EXPLICIT_OFFS_PER_DRAIN);
            *self.combat.attack_off_explicit.lock().unwrap() = rest;
            *slot = Some(on);
            return Some(AttackDrain { on, command: None, explicit });
        }
        // A stale zone fence belongs to the request that set it; explicit offs carry their own zone
        // and are sent regardless.
        let command = match self.actions.drain(std::sync::Arc::as_ptr(&self.combat.attack) as usize) {
            Ok(command) => command,
            Err(()) if !explicit.is_empty() => None,
            Err(()) => return None,
        };
        Some(AttackDrain { on, command, explicit })
    }

    /// Record that the network owner put this explicit off on the wire.
    pub fn attack_off_sent(&self, command: &std::sync::Arc<str>) {
        self.combat.attack_off_settled.lock().unwrap()
            .push(AttackOffSettled { command: command.clone(), outcome: AttackOffOutcome::Sent });
    }

    /// The host's read of explicit-off outcomes, once per tick before its fences. Under the slot
    /// lock, so no drain can send what is settled here as not sent:
    /// - `Abandon::OtherZones(epoch)`: the host has just seen the zone change to `epoch`; every
    ///   explicit off still queued from an earlier zone is settled as `ZoneEnded`.
    /// - `Abandon::All`: the session has ended; every queued explicit off is `SessionEnded`.
    pub fn take_attack_off_settlements(&self, abandon: Abandon) -> Vec<AttackOffSettled> {
        let _slot = self.combat.attack.lock().unwrap();
        let mut queued = self.combat.attack_off_explicit.lock().unwrap();
        match abandon {
            Abandon::None => {}
            Abandon::OtherZones(current) => {
                let stale = drain_stale(&mut queued, current);
                self.settle_attack_offs(stale, AttackOffOutcome::ZoneEnded);
            }
            Abandon::All => {
                let all = std::mem::take(&mut *queued);
                self.settle_attack_offs(all, AttackOffOutcome::SessionEnded);
            }
        }
        drop(queued);
        std::mem::take(&mut *self.combat.attack_off_settled.lock().unwrap())
    }

    fn settle_attack_offs(&self, offs: Vec<ExplicitAttackOff>, outcome: AttackOffOutcome) {
        let mut settled = self.combat.attack_off_settled.lock().unwrap();
        settled.extend(offs.into_iter().filter_map(|off| off.command).map(|command| AttackOffSettled { command, outcome }));
    }

    /// Drain a pending consider request.
    pub fn take_consider(&self) -> Option<u32> {
        self.dequeue(&self.combat.consider)
    }

    /// Drain a pending cast request.
    pub fn take_cast(&self) -> Option<CastRequest> {
        self.dequeue(&self.combat.cast)
    }

    /// Drain a pending awaited-cast request as `(CastRequest, Sender)` (A3 Migration 3, #448).
    /// `ActionLoop::drain_cast` takes this, sends the cast, and parks the `Sender` in `pending_cast`
    /// until the cast's outcome transition lands (or fires `Refused` immediately if it never started).
    /// Sibling of `take_cast`.
    pub fn take_cast_await(&self) -> Option<(CastRequest, oneshot::Sender<CommandResult<CastEnd>>)> {
        self.dequeue(&self.combat.cast_await)
    }

    /// Drain a pending memorize/scribe request as `(slot, spell_id, scribing, from)`.
    pub fn take_mem_spell(&self) -> Option<(u32, u32, u32, Option<u32>)> {
        self.dequeue(&self.combat.mem_spell)
    }

    /// Drain a pending pet command byte.
    pub fn take_pet_command(&self) -> Option<u8> {
        self.dequeue(&self.combat.pet_cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::CommandState;

    /// The core invariant of the facade: a `request_*` write and the matching `take_*` drain touch
    /// the SAME slot, the drain removes it (so a stale command can't fire twice), and a second drain
    /// sees nothing. Proven for every combat verb.
    #[test]
    fn request_then_take_round_trips_each_combat_slot() {
        let cs = CommandState::default();

        // request_target → take_target (once), then empty.
        cs.request_target(42);
        assert_eq!(cs.take_target(), Some(42));
        assert_eq!(cs.take_target(), None, "a drained target must not re-fire");

        cs.request_attack(true);
        assert_eq!(cs.take_attack(), Some(true));
        assert_eq!(cs.take_attack(), None);

        cs.request_consider(7);
        assert_eq!(cs.take_consider(), Some(7));
        assert_eq!(cs.take_consider(), None);

        cs.request_cast(eqoxide_ipc::CastRequest { gem: 3, target_id: Some(9), item_slot: None });
        let cast = cs.take_cast().expect("cast queued");
        assert_eq!((cast.gem, cast.target_id, cast.item_slot), (3, Some(9), None));
        assert!(cs.take_cast().is_none());

        cs.request_mem_spell(2, 202, 1, None);
        assert_eq!(cs.take_mem_spell(), Some((2, 202, 1, None)));
        assert_eq!(cs.take_mem_spell(), None);

        cs.request_pet_command(2);
        assert_eq!(cs.take_pet_command(), Some(2));
        assert_eq!(cs.take_pet_command(), None);
    }

    /// A slot with nothing queued drains to `None` — no phantom command.
    #[test]
    fn take_on_empty_slot_is_none() {
        let cs = CommandState::default();
        assert_eq!(cs.take_target(), None);
        assert_eq!(cs.take_attack(), None);
        assert!(cs.take_cast().is_none())
    }
}
