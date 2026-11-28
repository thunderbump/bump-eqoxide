//! Single-writer command admission. The CPU host owns execution and physical deadlines.
//! Bind the returned router to a loopback listener; do not mount legacy mutation routes.

use axum::{
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use eqoxide_net::packet_telemetry::SessionTrace;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::watch;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CommandRequest {
    pub session: String,
    pub zone_epoch: u64,
    #[serde(default)]
    pub inventory_revision: Option<u64>,
    #[serde(default)]
    pub merchant_revision: Option<u64>,
    pub operation: Operation,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    MoveFor {
        east: f32,
        north: f32,
        duration_ms: u64,
    },
    Stop,
    Target {
        entity_id: u16,
    },
    Attack {
        lease_ms: u64,
    },
    AttackOff,
    Sit,
    Stand,
    Say {
        text: String,
    },
    Tell {
        recipient: String,
        text: String,
    },
    Loot {
        entity_id: u16,
    },
    MerchantOpen {
        entity_id: u16,
    },
    MerchantClose,
    Buy {
        slot: u32,
        quantity: u32,
    },
    Sell {
        slot: u32,
        quantity: u32,
    },
    InventoryMove {
        from_slot: u32,
        to_slot: u32,
        quantity: u32,
    },
    Camp,
}

impl Operation {
    pub fn is_priority(&self) -> bool {
        matches!(self, Self::Stop | Self::AttackOff)
    }

    fn validate(&self) -> Result<(), &'static str> {
        fn text_ok(text: &str, max: usize) -> bool {
            !text.trim().is_empty() && text.len() <= max && !text.chars().any(char::is_control)
        }
        match self {
            Self::MoveFor {
                east,
                north,
                duration_ms,
            } => {
                if !east.is_finite()
                    || !north.is_finite()
                    || east.abs() > 1.0
                    || north.abs() > 1.0
                    || !(1..=2000).contains(duration_ms)
                {
                    return Err(
                        "move_for requires finite axes in [-1,1] and duration_ms in [1,2000]",
                    );
                }
            }
            Self::Attack { lease_ms } if !(1..=5000).contains(lease_ms) => {
                return Err("attack lease_ms must be in [1,5000]")
            }
            Self::Target { entity_id } | Self::MerchantOpen { entity_id } if *entity_id == 0 => {
                return Err("entity_id must be nonzero")
            }
            Self::Say { text } if !text_ok(text, 512) => return Err("invalid chat text"),
            Self::Tell { recipient, text }
                if !text_ok(recipient, 64)
                    || recipient.chars().any(char::is_whitespace)
                    || !text_ok(text, 512) =>
            {
                return Err("invalid tell recipient or text")
            }
            Self::Buy { quantity, .. } if *quantity != 1 => return Err("buy quantity must be one"),
            Self::Sell { quantity, .. } | Self::InventoryMove { quantity, .. }
                if *quantity == 0 =>
            {
                return Err("quantity must be positive")
            }
            Self::InventoryMove {
                from_slot, to_slot, ..
            } if from_slot == to_slot => return Err("inventory slots must differ"),
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    Accepted,
    Sent,
    AppliedLocal,
    Confirmed,
    Refused,
    Unknown,
}

impl CommandStatus {
    fn busy(self) -> bool {
        matches!(self, Self::Accepted | Self::Sent)
    }
    /// A terminal receipt is never rewritten (central-6ly1.7, design A2.2).
    pub fn terminal(self) -> bool {
        !self.busy()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CommandReceipt {
    pub id: String,
    pub status: CommandStatus,
    pub terminal: bool,
    /// Always a snake_case code for `refused` and `unknown`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Prose for a human, beside the code. Never matched by scripts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Value>,
    pub accepted_at_ms: u64,
    pub updated_at_ms: u64,
    /// `false` on a refusal at admission: it took no live slot. Absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admitted: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct AdmittedCommand {
    pub id: String,
    pub request: CommandRequest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlayerEvent {
    pub cursor: String,
    /// Host clock (`clock_ms` in state) when the event was published.
    pub at_ms: u64,
    pub event: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventBatch {
    pub events: Vec<PlayerEvent>,
    pub cursor: String,
    pub gap: bool,
}

struct Entry {
    /// Kept while the command is live; dropped once terminal, so retained receipts hold no input.
    request: Option<CommandRequest>,
    /// Digest of the canonical JSON payload, taken before f32 conversion, so a changed retry is
    /// caught the same way whether the receipt is retained or already evicted.
    payload: [u8; 16],
    receipt: CommandReceipt,
    dispatched: bool,
    /// The newest event cursor right after this command's admission event, for replays.
    event_cursor: String,
}

impl Entry {
    fn is_priority(&self) -> bool {
        self.request.as_ref().is_some_and(|r| r.operation.is_priority())
    }
}

/// Evidence on every refused `attack_off`: no `OP_AutoAttack 0` was sent for it.
pub fn attack_off_not_sent() -> Value {
    json!({"kind":"attack_off_not_sent","packets":[]})
}

/// An evicted receipt keeps only enough to answer a replay: its identity never becomes reusable.
struct Tombstone {
    payload: [u8; 16],
    status: CommandStatus,
    reason: Option<String>,
}

struct Journal {
    zone_epoch: u64,
    ready: bool,
    /// The session is over: nothing is admitted, safety commands included.
    closed: bool,
    revision: u64,
    state: Value,
    entries: HashMap<String, Entry>,
    /// Non-terminal entries. Bounded by `LIVE_LIMIT` for ordinary work.
    live: HashSet<String>,
    /// Terminal entries in the order they became terminal; the front is evicted first.
    retained: VecDeque<String>,
    tombstones: HashMap<[u8; 16], Tombstone>,
    /// Identities issued from the ordinary space, and from each safety verb's reserve.
    identities: usize,
    stop_reserve_used: usize,
    attack_off_reserve_used: usize,
    priority: VecDeque<String>,
    ordinary: VecDeque<String>,
    sequence: u64,
    events: VecDeque<(u64, PlayerEvent)>,
}

/// At most this many ordinary commands may be non-terminal at once (design A3).
const LIVE_LIMIT: usize = 64;

struct Shared {
    session: String,
    authorization: String,
    /// Full receipts kept after they become terminal. Identities: sixteen times this. Each
    /// safety verb's reserve past the identity limit: one sixty-fourth of it, at least one.
    retain_capacity: usize,
    identity_capacity: usize,
    safety_reserve: usize,
    event_capacity: usize,
    started: Instant,
    journal: Mutex<Journal>,
    changed: watch::Sender<u64>,
}

/// Clones share one journal and one action queue. A command ID is never reusable in a session:
/// terminal receipts are evicted beyond `retain_capacity`, but their identity is kept.
#[derive(Clone)]
pub struct Control {
    shared: Arc<Shared>,
    packet_trace: SessionTrace,
}

impl Control {
    /// `journal_capacity` is the number of terminal receipts kept in full (design A3).
    /// A new process must receive a new session identity, even for the same character.
    pub fn new(
        session: String,
        credential: String,
        journal_capacity: usize,
        event_capacity: usize,
    ) -> Result<Self, String> {
        if session.is_empty()
            || session.contains(':')
            || session.len() > 128
            || session.chars().any(char::is_control)
        {
            return Err(
                "session must be nonempty, <=128 bytes, and contain no colon/control characters"
                    .into(),
            );
        }
        if credential.is_empty() || credential.chars().any(char::is_whitespace) {
            return Err("control credential must be nonempty and contain no whitespace".into());
        }
        if journal_capacity == 0 || event_capacity == 0 {
            return Err("journal and event capacities must be positive".into());
        }
        let (changed, _) = watch::channel(0);
        Ok(Self {
            packet_trace: SessionTrace::new(Instant::now()),
            shared: Arc::new(Shared {
                session,
                authorization: format!("Bearer {credential}"),
                retain_capacity: journal_capacity,
                identity_capacity: journal_capacity.saturating_mul(16),
                safety_reserve: (journal_capacity / 64).max(1),
                event_capacity,
                started: Instant::now(),
                changed,
                journal: Mutex::new(Journal {
                    zone_epoch: 0,
                    ready: false,
                    revision: 0,
                    state: json!({"phase":"starting"}),
                    entries: HashMap::new(),
                    live: HashSet::new(),
                    retained: VecDeque::new(),
                    tombstones: HashMap::new(),
                    identities: 0,
                    stop_reserve_used: 0,
                    attack_off_reserve_used: 0,
                    closed: false,
                    priority: VecDeque::new(),
                    ordinary: VecDeque::new(),
                    sequence: 0,
                    events: VecDeque::new(),
                }),
            }),
        })
    }

    pub(crate) fn with_packet_trace(mut self, trace: SessionTrace) -> Self {
        self.packet_trace = trace;
        self
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/v2/player/state", get(get_state))
            .route("/v2/player/commands/:id", get(get_command).put(put_command))
            .route("/v2/player/events", get(get_events))
            .route("/v2/player/packets", get(get_packets))
            .layer(DefaultBodyLimit::max(16 * 1024))
            .layer(middleware::from_fn_with_state(self.clone(), authenticate))
            .with_state(self.clone())
    }

    /// The host clock origin. State's `clock_ms` and every `at_ms` are measured from this.
    pub fn started(&self) -> Instant {
        self.shared.started
    }

    fn now_ms(&self) -> u64 {
        self.shared.started.elapsed().as_millis() as u64
    }

    /// Publication replaces client knowledge atomically. The host supplies field provenance.
    /// Changing epoch fences queued/in-flight commands; the host must also stop physical actions.
    /// A publication is not an event: it never wakes an event long-poll (central-6ly1.1).
    pub fn publish_state(&self, zone_epoch: u64, state: Value) -> Result<(), String> {
        if !state.is_object() {
            return Err("state must be a JSON object".into());
        }
        let mut journal = self.shared.journal.lock().unwrap();
        if zone_epoch < journal.zone_epoch {
            return Err("zone epoch cannot decrease".into());
        }
        if zone_epoch != journal.zone_epoch {
            self.fence_locked(&mut journal, "zone_epoch_changed", false);
            journal.zone_epoch = zone_epoch;
        }
        journal.state = state;
        journal.revision += 1;
        Ok(())
    }

    /// The host gates ordinary admission on real session readiness. Safety verbs bypass this gate.
    /// A readiness drop (death, a stale link, geometry loading) does not fence `attack_off`: it is
    /// the safety stop for exactly that moment (design A2.5), and the network owner settles it.
    pub fn set_ready(&self, ready: bool) {
        let mut journal = self.shared.journal.lock().unwrap();
        if journal.ready && !ready {
            self.fence_locked(&mut journal, "not_ready", true);
        }
        journal.ready = ready;
    }

    /// Shutdown has begun: admit nothing more, safety commands included. Live commands are left to
    /// their owners; `end_session` ends what remains.
    pub fn close_admission(&self) {
        self.shared.journal.lock().unwrap().closed = true;
    }

    /// The session is over: fence everything still live, `attack_off` included, since no network
    /// owner is left to settle it, and refuse every later command at admission.
    pub fn end_session(&self, reason: &str) {
        let mut journal = self.shared.journal.lock().unwrap();
        self.fence_locked(&mut journal, reason, false);
        journal.ready = false;
        journal.closed = true;
    }

    pub fn state(&self) -> Value {
        let journal = self.shared.journal.lock().unwrap();
        let mut state = journal.state.clone();
        let object = state.as_object_mut().unwrap();
        object.insert("session".into(), json!(self.shared.session));
        object.insert("zone_epoch".into(), json!(journal.zone_epoch));
        object.insert("revision".into(), json!(journal.revision));
        object.insert("event_cursor".into(), json!(self.cursor(journal.sequence)));
        // The clock at read time, so no `*_at_ms` in a receipt or event can be newer than it.
        object.insert("clock_ms".into(), json!(self.now_ms()));
        state
    }

    /// Drain once per host tick, then recheck observed revisions before executing each command.
    /// Draining itself provides no evidence that a command reached a network queue or took effect.
    pub fn drain_actions(&self) -> Vec<AdmittedCommand> {
        let mut journal = self.shared.journal.lock().unwrap();
        let mut ids: Vec<_> = journal.priority.drain(..).collect();
        ids.extend(journal.ordinary.drain(..));
        ids.into_iter()
            .filter_map(|id| {
                let entry = journal.entries.get(&id)?;
                let request = entry.request.clone()?;
                (entry.receipt.status == CommandStatus::Accepted).then(|| AdmittedCommand {
                    id,
                    request,
                })
            })
            .collect()
    }

    /// Atomically claim execution before side effects. This supplies no packet-send evidence.
    pub fn mark_dispatched(&self, id: &str) -> Result<(), String> {
        let mut journal = self.shared.journal.lock().unwrap();
        let entry = journal.entries.get_mut(id).ok_or("unknown command id")?;
        if entry.receipt.status != CommandStatus::Accepted {
            return Err("command no longer accepted".into());
        }
        entry.dispatched = true;
        Ok(())
    }

    /// Outcomes come only from the host's actual local or server observations.
    /// `sent`, `applied_local`, and `confirmed` require an evidence object with a kind.
    pub fn publish_outcome(
        &self,
        id: &str,
        status: CommandStatus,
        reason: Option<String>,
        evidence: Option<Value>,
    ) -> Result<CommandReceipt, String> {
        self.publish_outcome_with_detail(id, status, reason, None, evidence)
    }

    /// As `publish_outcome`, with prose for `detail` beside the reason code.
    pub fn publish_outcome_with_detail(
        &self,
        id: &str,
        status: CommandStatus,
        reason: Option<String>,
        detail: Option<String>,
        evidence: Option<Value>,
    ) -> Result<CommandReceipt, String> {
        if matches!(
            status,
            CommandStatus::Sent | CommandStatus::AppliedLocal | CommandStatus::Confirmed
        ) && !evidence
            .as_ref()
            .and_then(|v| v.get("kind"))
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
        {
            return Err("outcome requires evidence with a nonempty kind".into());
        }
        if matches!(status, CommandStatus::Refused | CommandStatus::Unknown)
            && !reason.as_ref().is_some_and(|s| !s.trim().is_empty())
        {
            return Err("refused/unknown outcomes require a reason".into());
        }
        let mut journal = self.shared.journal.lock().unwrap();
        let now_ms = self.now_ms();
        let entry = journal.entries.get_mut(id).ok_or("unknown command id")?;
        let previous = entry.receipt.status;
        if previous == status && entry.receipt.reason == reason && entry.receipt.evidence == evidence {
            return Ok(entry.receipt.clone());
        }
        // A terminal receipt never changes (design A2.2): only `accepted` and `sent` move on.
        let allowed = match previous {
            CommandStatus::Accepted => status != CommandStatus::Accepted,
            CommandStatus::Sent => status != CommandStatus::Accepted,
            _ => false,
        };
        if !allowed {
            return Err("invalid command status transition".into());
        }
        entry.receipt.status = status;
        entry.receipt.terminal = status.terminal();
        entry.receipt.reason = reason;
        entry.receipt.evidence = evidence;
        entry.receipt.detail = detail;
        entry.receipt.updated_at_ms = now_ms;
        let next = entry.receipt.clone();
        if next.terminal {
            self.retire_locked(&mut journal, id);
        }
        self.event_locked(&mut journal, json!({"kind":"command","command":next}));
        Ok(next)
    }

    pub fn publish_event(&self, event: Value) -> String {
        let mut journal = self.shared.journal.lock().unwrap();
        self.event_locked(&mut journal, event)
    }

    pub fn command(&self, id: &str) -> Option<CommandReceipt> {
        self.shared
            .journal
            .lock()
            .unwrap()
            .entries
            .get(id)
            .map(|e| e.receipt.clone())
    }

    /// Identities consumed from the ordinary space, for tests.
    #[cfg(test)]
    pub(crate) fn identities_for_test(&self) -> usize {
        self.shared.journal.lock().unwrap().identities
    }

    /// The event payloads after `after`, for host tests.
    #[cfg(test)]
    pub(crate) fn events_after_for_test(&self, after: u64) -> Vec<Value> {
        self.event_batch(after).unwrap().events.into_iter().map(|e| e.event).collect()
    }

    /// The final status of a command, retained or evicted. A host decision that depends on how a
    /// command ended must use this: an evicted receipt is still terminal.
    pub fn settled_status(&self, id: &str) -> Option<CommandStatus> {
        let journal = self.shared.journal.lock().unwrap();
        journal
            .entries
            .get(id)
            .map(|e| e.receipt.status)
            .or_else(|| journal.tombstones.get(&digest(id.as_bytes())).map(|t| t.status))
    }

    fn lookup(&self, id: &str) -> Result<CommandReceipt, ApiError> {
        let journal = self.shared.journal.lock().unwrap();
        if let Some(entry) = journal.entries.get(id) {
            return Ok(entry.receipt.clone());
        }
        match journal.tombstones.get(&digest(id.as_bytes())) {
            Some(tomb) => Err(expired(tomb)),
            None => Err(ApiError(StatusCode::NOT_FOUND, "unknown_command_id".into(), None)),
        }
    }

    /// A receipt just became terminal: it leaves the live set and joins the eviction queue.
    fn retire_locked(&self, journal: &mut Journal, id: &str) {
        if journal.live.remove(id) {
            journal.retained.push_back(id.to_owned());
            if let Some(entry) = journal.entries.get_mut(id) {
                entry.request = None;
            }
        }
        while journal.retained.len() > self.shared.retain_capacity {
            let Some(old) = journal.retained.pop_front() else { break };
            if let Some(entry) = journal.entries.remove(&old) {
                journal.tombstones.insert(digest(old.as_bytes()), Tombstone {
                    payload: entry.payload,
                    status: entry.receipt.status,
                    reason: entry.receipt.reason,
                });
            }
        }
    }

    fn cursor(&self, sequence: u64) -> String {
        format!("{}:{sequence}", self.shared.session)
    }

    fn event_locked(&self, journal: &mut Journal, event: Value) -> String {
        journal.sequence += 1;
        let cursor = self.cursor(journal.sequence);
        let at_ms = self.now_ms();
        journal.events.push_back((
            journal.sequence,
            PlayerEvent {
                cursor: cursor.clone(),
                at_ms,
                event,
            },
        ));
        while journal.events.len() > self.shared.event_capacity {
            journal.events.pop_front();
        }
        self.shared.changed.send_replace(journal.sequence);
        cursor
    }

    /// Fences touch only non-terminal receipts (design A2.2). Never-dispatched work is refused;
    /// dispatched work is unknown.
    fn fence_locked(&self, journal: &mut Journal, reason: &str, spare_attack_off: bool) {
        let spared = |entry: &Entry| {
            spare_attack_off
                && entry.request.as_ref().is_some_and(|r| matches!(r.operation, Operation::AttackOff))
        };
        let priority = std::mem::take(&mut journal.priority);
        journal.priority = priority
            .into_iter()
            .filter(|id| journal.entries.get(id).is_some_and(|entry| spared(entry)))
            .collect();
        journal.ordinary.clear();
        let now_ms = self.now_ms();
        let mut live: Vec<String> = journal.live.iter().cloned().collect();
        live.sort();
        let mut changes = Vec::new();
        for id in live {
            let Some(entry) = journal.entries.get_mut(&id) else { continue };
            if spared(entry) {
                continue;
            }
            let attack_off = entry.request.as_ref().is_some_and(|r| matches!(r.operation, Operation::AttackOff));
            entry.receipt.status = if entry.receipt.status == CommandStatus::Accepted && !entry.dispatched {
                CommandStatus::Refused
            } else {
                CommandStatus::Unknown
            };
            entry.receipt.terminal = true;
            entry.receipt.reason = Some(reason.into());
            entry.receipt.updated_at_ms = now_ms;
            if attack_off && entry.receipt.status == CommandStatus::Refused {
                // One shape for an off whose zone ended before it was sent, wherever it stopped.
                if reason == "zone_epoch_changed" {
                    entry.receipt.reason = Some("stale_zone_epoch_before_dispatch".into());
                }
                entry.receipt.evidence = Some(attack_off_not_sent());
            }
            changes.push(entry.receipt.clone());
            self.retire_locked(journal, &id);
        }
        for receipt in changes {
            self.event_locked(journal, json!({"kind":"command","command":receipt}));
        }
    }

    /// PUT processing in the frozen order of design A3: validate the ID, answer a known ID, run
    /// the capacity checks (a 503 consumes nothing), then write the identity, its outcome and the
    /// receipt event in one step under the journal lock.
    fn admit(&self, id: String, identity: Value) -> Result<(StatusCode, CommandReceipt, String), ApiError> {
        if !valid_command_id(&id) {
            return Err(ApiError(StatusCode::BAD_REQUEST, "invalid_command_id".into(), None));
        }
        let payload = digest(canonical(&identity).as_bytes());
        let mut journal = self.shared.journal.lock().unwrap();
        let now_ms = self.now_ms();
        if let Some(existing) = journal.entries.get(&id) {
            return if existing.payload == payload {
                Ok((StatusCode::OK, existing.receipt.clone(), existing.event_cursor.clone()))
            } else {
                Err(changed_payload())
            };
        }
        if let Some(tomb) = journal.tombstones.get(&digest(id.as_bytes())) {
            return if tomb.payload == payload {
                Err(expired(tomb))
            } else {
                Err(changed_payload())
            };
        }
        // Unit operations carry nothing but their kind; an internally tagged enum would ignore
        // extra fields there, so they are checked here (`deny_unknown_fields` covers the rest).
        // Only a JSON object is a command; serde would also accept a positional array.
        if !identity.is_object() {
            return Err(ApiError(StatusCode::UNPROCESSABLE_ENTITY, "invalid_command_payload".into(), None));
        }
        let unit_with_extras = identity["operation"].as_object().is_some_and(|op| {
            matches!(op.get("kind").and_then(Value::as_str),
                Some("stop" | "attack_off" | "sit" | "stand" | "merchant_close" | "camp"))
                && op.len() != 1
        });
        if unit_with_extras {
            return Err(ApiError(StatusCode::UNPROCESSABLE_ENTITY, "invalid_command_payload".into(), None));
        }
        let request: CommandRequest = serde_json::from_value(identity.clone()).map_err(|_| {
            ApiError(StatusCode::UNPROCESSABLE_ENTITY, "invalid_command_payload".into(), None)
        })?;
        let safety = request.operation.is_priority();
        let ordinary_space = journal.identities < self.shared.identity_capacity;
        if !ordinary_space {
            let used = match request.operation {
                Operation::Stop => journal.stop_reserve_used,
                Operation::AttackOff => journal.attack_off_reserve_used,
                _ => {
                    return Err(ApiError(StatusCode::SERVICE_UNAVAILABLE, "session_id_space_exhausted".into(), None))
                }
            };
            if used >= self.shared.safety_reserve {
                return Err(ApiError(StatusCode::SERVICE_UNAVAILABLE, "safety_reserve_exhausted".into(), None));
            }
        }
        // Every live receipt counts toward the limit; only safety commands are exempt from it.
        if !safety && journal.live.len() >= LIVE_LIMIT {
            return Err(ApiError(StatusCode::SERVICE_UNAVAILABLE, "live_command_limit".into(), None));
        }
        // From here the identity is consumed, whatever the outcome.
        if ordinary_space {
            journal.identities += 1;
        } else if matches!(request.operation, Operation::Stop) {
            journal.stop_reserve_used += 1;
        } else {
            journal.attack_off_reserve_used += 1;
        }
        let mut detail = None;
        let refusal = if request.session != self.shared.session {
            Some("stale_session".to_owned())
        } else if request.zone_epoch != journal.zone_epoch {
            Some("stale_zone_epoch".to_owned())
        } else if let Err(prose) = request.operation.validate() {
            detail = Some(prose.to_owned());
            Some("invalid_operation".to_owned())
        } else if journal.closed || (!safety && !journal.ready) {
            Some("not_ready".to_owned())
        } else if !safety
            && journal.live.iter().any(|live| {
                journal.entries.get(live).is_some_and(|e| !e.is_priority() && e.receipt.status.busy())
            })
        {
            Some("busy".to_owned())
        } else {
            None
        };
        let status = if refusal.is_some() {
            CommandStatus::Refused
        } else {
            CommandStatus::Accepted
        };
        let receipt = CommandReceipt {
            id: id.clone(),
            status,
            terminal: status.terminal(),
            evidence: (refusal.is_some() && matches!(request.operation, Operation::AttackOff))
                .then(attack_off_not_sent),
            reason: refusal,
            detail,
            accepted_at_ms: now_ms,
            updated_at_ms: now_ms,
            admitted: (status == CommandStatus::Refused).then_some(false),
        };
        if status == CommandStatus::Accepted {
            // A queued move must not restart immediately after a higher-priority stop.
            let mut superseded = Vec::new();
            if safety {
                let mut live: Vec<String> = journal.live.iter().cloned().collect();
                live.sort();
                for live_id in live {
                    let Some(entry) = journal.entries.get_mut(&live_id) else { continue };
                    let Some(live_request) = entry.request.as_ref() else { continue };
                    let cancel = matches!(
                        (&request.operation, &live_request.operation),
                        (Operation::Stop, Operation::MoveFor { .. })
                            | (Operation::AttackOff, Operation::Attack { .. })
                    );
                    if cancel && entry.receipt.status == CommandStatus::Accepted {
                        entry.receipt.status = if entry.dispatched {
                            CommandStatus::Unknown
                        } else {
                            CommandStatus::Refused
                        };
                        entry.receipt.terminal = true;
                        entry.receipt.reason = Some("superseded_by_safety_command".into());
                        entry.receipt.updated_at_ms = now_ms;
                        superseded.push(entry.receipt.clone());
                    }
                }
            }
            for canceled in superseded {
                self.retire_locked(&mut journal, &canceled.id);
                self.event_locked(&mut journal, json!({"kind":"command","command":canceled}));
            }
            if safety {
                journal.priority.push_back(id.clone());
            } else {
                journal.ordinary.push_back(id.clone());
            }
            journal.live.insert(id.clone());
        }
        journal.entries.insert(
            id.clone(),
            Entry {
                dispatched: false,
                request: Some(request),
                payload,
                receipt: receipt.clone(),
                event_cursor: String::new(),
            },
        );
        let cursor = self.event_locked(&mut journal, json!({"kind":"command","command":receipt}));
        if let Some(entry) = journal.entries.get_mut(&id) {
            entry.event_cursor = cursor.clone();
        }
        if status != CommandStatus::Accepted {
            // Terminal at birth: straight into the eviction queue, never live.
            journal.live.insert(id.clone());
            self.retire_locked(&mut journal, &id);
        }
        Ok((
            if status == CommandStatus::Accepted {
                StatusCode::ACCEPTED
            } else {
                StatusCode::CONFLICT
            },
            receipt,
            cursor,
        ))
    }

    fn event_batch(&self, after: u64) -> Result<EventBatch, ApiError> {
        let journal = self.shared.journal.lock().unwrap();
        if after > journal.sequence {
            return Err(ApiError(StatusCode::BAD_REQUEST, "future_event_cursor".into(), None));
        }
        let gap = journal
            .events
            .front()
            .is_some_and(|(first, _)| after < first.saturating_sub(1));
        Ok(EventBatch {
            events: journal
                .events
                .iter()
                .filter(|(seq, _)| *seq > after)
                .map(|(_, e)| e.clone())
                .collect(),
            cursor: self.cursor(journal.sequence),
            gap,
        })
    }
}

/// The payload as compared for a retry: serialized JSON (keys are sorted, since `serde_json` is
/// built without `preserve_order`) with `-0.0` written as `0.0`, the one value JSON equality and
/// text equality disagree on.
fn canonical(value: &Value) -> String {
    fn norm(v: &Value) -> Value {
        match v {
            Value::Number(n) if n.as_f64() == Some(0.0) && n.is_f64() => json!(0.0),
            Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
            Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), norm(v))).collect()),
            other => other.clone(),
        }
    }
    norm(value).to_string()
}

/// First 16 bytes of SHA-256: identities and payloads of evicted commands, compared on replay.
fn digest(bytes: &[u8]) -> [u8; 16] {
    let full = Sha256::digest(bytes);
    let mut out = [0u8; 16];
    out.copy_from_slice(&full[..16]);
    out
}

fn valid_command_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn changed_payload() -> ApiError {
    ApiError(StatusCode::CONFLICT, "command_id_reused_with_changed_payload".into(), None)
}

fn expired(tomb: &Tombstone) -> ApiError {
    ApiError::with_body(
        StatusCode::GONE,
        json!({"error":"command_id_expired","status":tomb.status,"reason":tomb.reason}),
    )
}

#[derive(Debug)]
struct ApiError(StatusCode, String, Option<Value>);

impl ApiError {
    fn with_body(status: StatusCode, body: Value) -> Self {
        Self(status, String::new(), Some(body))
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = self.2.unwrap_or_else(|| json!({"error":self.1}));
        (self.0, Json(body)).into_response()
    }
}

async fn authenticate(
    State(control): State<Control>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let valid = request
        .headers()
        .get_all(axum::http::header::AUTHORIZATION)
        .iter()
        .count()
        == 1
        && request
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .is_some_and(|value| value.as_bytes() == control.shared.authorization.as_bytes());
    if !valid {
        return ApiError(StatusCode::UNAUTHORIZED, "unauthorized".into(), None).into_response();
    }
    next.run(request).await
}

async fn get_state(State(control): State<Control>) -> Json<Value> {
    Json(control.state())
}

async fn get_command(
    State(control): State<Control>,
    Path(id): Path<String>,
) -> Result<Json<CommandReceipt>, ApiError> {
    control.lookup(&id).map(Json)
}

/// The receipt, plus `event_cursor`: the newest cursor once the command was recorded, so a wait
/// on its events can pass it as `after` and miss nothing.
async fn put_command(
    State(control): State<Control>,
    Path(id): Path<String>,
    request: Request<axum::body::Body>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    // The ID is checked before the body is read (design A3): an invalid ID gets a prompt 400,
    // even from a client that sent headers and then stalled.
    if !valid_command_id(&id) {
        return Err(ApiError(StatusCode::BAD_REQUEST, "invalid_command_id".into(), None));
    }
    let json = request
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<mime::Mime>().ok())
        // Exactly the test axum's `Json` extractor applies: `application/json` or
        // `application/*+json`, parameters parsed (quoted values included), lists rejected.
        .is_some_and(|m| {
            m.type_() == mime::APPLICATION
                && (m.subtype() == mime::JSON || m.suffix().is_some_and(|s| s == mime::JSON))
        });
    if !json {
        return Err(ApiError(StatusCode::UNSUPPORTED_MEDIA_TYPE, "content_type_must_be_application_json".into(), None));
    }
    let bytes = axum::body::to_bytes(request.into_body(), 16 * 1024).await.map_err(|_| {
        ApiError(StatusCode::UNPROCESSABLE_ENTITY, "invalid_command_payload".into(), None)
    })?;
    // A body that is not JSON is the same 422 as a payload of the wrong shape.
    let body: Value = serde_json::from_slice(&bytes).map_err(|_| {
        ApiError(StatusCode::UNPROCESSABLE_ENTITY, "invalid_command_payload".into(), None)
    })?;
    let (code, receipt, cursor) = control.admit(id, body)?;
    let mut value = serde_json::to_value(receipt).unwrap();
    value["event_cursor"] = json!(cursor);
    Ok((code, Json(value)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventQuery {
    after: Option<String>,
    wait: Option<f64>,
}

async fn get_events(
    State(control): State<Control>,
    Query(query): Query<EventQuery>,
) -> Result<Json<EventBatch>, ApiError> {
    let after = match query.after {
        None => 0,
        Some(cursor) => {
            let (session, sequence) = cursor.rsplit_once(':').ok_or(ApiError(StatusCode::BAD_REQUEST, "invalid_event_cursor".into(), None))?;
            if session != control.shared.session {
                return Err(ApiError(StatusCode::CONFLICT, "event_cursor_session_changed".into(), None));
            }
            sequence
                .parse()
                .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "invalid_event_cursor".into(), None))?
        }
    };
    let wait = query.wait.unwrap_or(0.0);
    if !wait.is_finite() || !(0.0..=30.0).contains(&wait) {
        return Err(ApiError(StatusCode::BAD_REQUEST, "wait_must_be_between_0_and_30_seconds".into(), None));
    }
    // Subscribe before inspecting the journal, so publication between read and wait is visible.
    let mut changed = control.shared.changed.subscribe();
    let batch = control.event_batch(after)?;
    if !batch.events.is_empty() || batch.gap || wait == 0.0 {
        return Ok(Json(batch));
    }
    // Recheck after every wake until events arrive or the original deadline passes, so a wake
    // that brought no event cannot end the poll early.
    let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(wait);
    loop {
        if tokio::time::timeout_at(deadline, changed.changed()).await.is_err() {
            return Ok(Json(control.event_batch(after)?));
        }
        let batch = control.event_batch(after)?;
        if !batch.events.is_empty() || batch.gap {
            return Ok(Json(batch));
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PacketQuery {
    after: Option<u64>,
    limit: Option<usize>,
    opcode: Option<String>,
    command: Option<String>,
    raw: Option<u8>,
}

async fn get_packets(
    State(control): State<Control>,
    Query(query): Query<PacketQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = query.limit.unwrap_or(500);
    if !(1..=2000).contains(&limit) {
        return Err(ApiError(StatusCode::BAD_REQUEST, "limit_must_be_between_1_and_2000".into(), None));
    }
    let raw = match query.raw {
        None | Some(0) => false,
        Some(1) => true,
        Some(_) => return Err(ApiError(StatusCode::BAD_REQUEST, "raw_must_be_0_or_1".into(), None)),
    };
    let mut page = control.packet_trace.page(
        query.after.unwrap_or(0),
        limit,
        query.opcode.as_deref(),
        query.command.as_deref(),
    );
    if raw {
        for packet in &mut page.packets {
            packet.include_raw_payload();
        }
    }
    Ok(Json(json!({
        "packets": page.packets,
        "next": page.next,
        "gap": page.gap,
        "counters": {
            // Successful EqStream::transmit calls, including retries.
            "sent": page.counts.sent,
            // Successful EqStream::poll_resend transmissions of already-sent datagrams.
            "resent": page.counts.resent,
            // EqStream::on_raw_recv counts socket reads before decoding, including transport control.
            "received": page.counts.received,
            // EqStream's unretried send failures, control queue losses, and reliable sends
            // abandoned at teardown that never reached the socket.
            "dropped_unretried": page.counts.dropped_unretried,
            // EqStream::handle_ordered_decoded counts newly exposed holes in incoming sequences.
            "sequence_gaps": page.counts.sequence_gaps,
        }
    })))
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
