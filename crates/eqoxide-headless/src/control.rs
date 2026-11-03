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
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
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
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CommandReceipt {
    pub id: String,
    pub status: CommandStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Value>,
}

#[derive(Clone, Debug)]
pub struct AdmittedCommand {
    pub id: String,
    pub request: CommandRequest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlayerEvent {
    pub cursor: String,
    pub event: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventBatch {
    pub events: Vec<PlayerEvent>,
    pub cursor: String,
    pub gap: bool,
}

struct Entry {
    request: CommandRequest,
    // Preserve the JSON numbers before f32 conversion when checking changed retries.
    identity: Value,
    receipt: CommandReceipt,
    dispatched: bool,
}

struct Journal {
    zone_epoch: u64,
    ready: bool,
    revision: u64,
    state: Value,
    entries: HashMap<String, Entry>,
    ordinary_count: usize,
    stop_reserve: bool,
    attack_off_reserve: bool,
    priority: VecDeque<String>,
    ordinary: VecDeque<String>,
    sequence: u64,
    events: VecDeque<(u64, PlayerEvent)>,
}

struct Shared {
    session: String,
    authorization: String,
    capacity: usize,
    event_capacity: usize,
    journal: Mutex<Journal>,
    changed: watch::Sender<u64>,
}

/// Clones share one journal and one action queue. Command identities are never evicted.
#[derive(Clone)]
pub struct Control {
    shared: Arc<Shared>,
}

impl Control {
    /// Capacity counts ordinary journal entries, plus one reserved entry per safety verb.
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
            shared: Arc::new(Shared {
                session,
                authorization: format!("Bearer {credential}"),
                capacity: journal_capacity,
                event_capacity,
                changed,
                journal: Mutex::new(Journal {
                    zone_epoch: 0,
                    ready: false,
                    revision: 0,
                    state: json!({"phase":"starting"}),
                    entries: HashMap::new(),
                    ordinary_count: 0,
                    stop_reserve: false,
                    attack_off_reserve: false,
                    priority: VecDeque::new(),
                    ordinary: VecDeque::new(),
                    sequence: 0,
                    events: VecDeque::new(),
                }),
            }),
        })
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/v2/player/state", get(get_state))
            .route("/v2/player/commands/:id", get(get_command).put(put_command))
            .route("/v2/player/events", get(get_events))
            .layer(DefaultBodyLimit::max(16 * 1024))
            .layer(middleware::from_fn_with_state(self.clone(), authenticate))
            .with_state(self.clone())
    }

    /// Publication replaces client knowledge atomically. The host supplies field provenance.
    /// Changing epoch fences queued/in-flight commands; the host must also stop physical actions.
    pub fn publish_state(&self, zone_epoch: u64, state: Value) -> Result<(), String> {
        if !state.is_object() {
            return Err("state must be a JSON object".into());
        }
        let mut journal = self.shared.journal.lock().unwrap();
        if zone_epoch < journal.zone_epoch {
            return Err("zone epoch cannot decrease".into());
        }
        if zone_epoch != journal.zone_epoch {
            self.fence_locked(&mut journal, "zone_epoch_changed");
            journal.zone_epoch = zone_epoch;
        }
        journal.state = state;
        journal.revision += 1;
        let revision = journal.revision;
        self.event_locked(
            &mut journal,
            json!({"kind":"state_changed","revision":revision}),
        );
        Ok(())
    }

    /// The host gates ordinary admission on real session readiness. Safety verbs bypass this gate.
    pub fn set_ready(&self, ready: bool) {
        let mut journal = self.shared.journal.lock().unwrap();
        if journal.ready && !ready {
            self.fence_locked(&mut journal, "not_ready");
        }
        journal.ready = ready;
    }

    pub fn state(&self) -> Value {
        let journal = self.shared.journal.lock().unwrap();
        let mut state = journal.state.clone();
        let object = state.as_object_mut().unwrap();
        object.insert("session".into(), json!(self.shared.session));
        object.insert("zone_epoch".into(), json!(journal.zone_epoch));
        object.insert("revision".into(), json!(journal.revision));
        object.insert("event_cursor".into(), json!(self.cursor(journal.sequence)));
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
                (entry.receipt.status == CommandStatus::Accepted).then(|| AdmittedCommand {
                    id,
                    request: entry.request.clone(),
                })
            })
            .collect()
    }

    /// Queue admission has no packet-send evidence, but cancellation can no longer promise refusal.
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
        let entry = journal.entries.get_mut(id).ok_or("unknown command id")?;
        let previous = entry.receipt.status;
        let next = CommandReceipt {
            id: id.into(),
            status,
            reason,
            evidence,
        };
        if entry.receipt == next {
            return Ok(next);
        }
        let allowed = match previous {
            CommandStatus::Accepted => status != CommandStatus::Accepted,
            CommandStatus::Sent => matches!(
                status,
                CommandStatus::AppliedLocal
                    | CommandStatus::Confirmed
                    | CommandStatus::Refused
                    | CommandStatus::Unknown
            ),
            CommandStatus::AppliedLocal => {
                matches!(status, CommandStatus::Confirmed | CommandStatus::Unknown)
            }
            CommandStatus::Unknown => {
                matches!(status, CommandStatus::Confirmed | CommandStatus::Refused)
            }
            CommandStatus::Confirmed | CommandStatus::Refused => false,
        };
        if !allowed {
            return Err("invalid command status transition".into());
        }
        entry.receipt = next.clone();
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

    fn cursor(&self, sequence: u64) -> String {
        format!("{}:{sequence}", self.shared.session)
    }

    fn event_locked(&self, journal: &mut Journal, event: Value) -> String {
        journal.sequence += 1;
        let cursor = self.cursor(journal.sequence);
        journal.events.push_back((
            journal.sequence,
            PlayerEvent {
                cursor: cursor.clone(),
                event,
            },
        ));
        while journal.events.len() > self.shared.event_capacity {
            journal.events.pop_front();
        }
        self.shared.changed.send_replace(journal.sequence);
        cursor
    }

    fn fence_locked(&self, journal: &mut Journal, reason: &str) {
        journal.priority.clear();
        journal.ordinary.clear();
        let mut changes = Vec::new();
        for entry in journal.entries.values_mut() {
            if entry.receipt.status.busy() || entry.receipt.status == CommandStatus::AppliedLocal {
                entry.receipt.status = if entry.receipt.status == CommandStatus::Accepted {
                    if entry.dispatched {
                        CommandStatus::Unknown
                    } else {
                        CommandStatus::Refused
                    }
                } else {
                    CommandStatus::Unknown
                };
                entry.receipt.reason = Some(reason.into());
                changes.push(entry.receipt.clone());
            }
        }
        changes.sort_by(|a, b| a.id.cmp(&b.id));
        for receipt in changes {
            self.event_locked(journal, json!({"kind":"command","command":receipt}));
        }
    }

    fn admit(&self, id: String, identity: Value) -> Result<(StatusCode, CommandReceipt), ApiError> {
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "invalid_command_id".into(),
            ));
        }
        let mut journal = self.shared.journal.lock().unwrap();
        if let Some(existing) = journal.entries.get(&id) {
            return if existing.identity == identity {
                Ok((StatusCode::OK, existing.receipt.clone()))
            } else {
                Err(ApiError(
                    StatusCode::CONFLICT,
                    "command_id_reused_with_changed_payload".into(),
                ))
            };
        }
        let request: CommandRequest = serde_json::from_value(identity.clone()).map_err(|_| {
            ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_command_payload".into(),
            )
        })?;
        // An ordinary full journal cannot consume the dedicated safety reserves.
        if journal.ordinary_count < self.shared.capacity {
            journal.ordinary_count += 1;
        } else {
            match request.operation {
                Operation::Stop if !journal.stop_reserve => journal.stop_reserve = true,
                Operation::AttackOff if !journal.attack_off_reserve => {
                    journal.attack_off_reserve = true
                }
                _ => {
                    return Err(ApiError(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "command_journal_full".into(),
                    ))
                }
            }
        }
        let refusal = if request.session != self.shared.session {
            Some("stale_session".into())
        } else if request.zone_epoch != journal.zone_epoch {
            Some("stale_zone_epoch".into())
        } else if let Err(reason) = request.operation.validate() {
            Some(reason.into())
        } else if !request.operation.is_priority() && !journal.ready {
            Some("not_ready".into())
        } else if !request.operation.is_priority()
            && journal
                .entries
                .values()
                .any(|e| !e.request.operation.is_priority() && e.receipt.status.busy())
        {
            Some("busy".into())
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
            reason: refusal,
            evidence: None,
        };
        if status == CommandStatus::Accepted {
            // A queued move must not restart immediately after a higher-priority stop.
            let mut superseded = Vec::new();
            if request.operation.is_priority() {
                for entry in journal.entries.values_mut() {
                    let cancel = matches!(
                        (&request.operation, &entry.request.operation),
                        (Operation::Stop, Operation::MoveFor { .. })
                            | (Operation::AttackOff, Operation::Attack { .. })
                    );
                    if cancel && entry.receipt.status == CommandStatus::Accepted {
                        entry.receipt.status = CommandStatus::Refused;
                        entry.receipt.reason = Some("superseded_by_safety_command".into());
                        superseded.push(entry.receipt.clone());
                    }
                }
            }
            superseded.sort_by(|a, b| a.id.cmp(&b.id));
            for canceled in superseded {
                self.event_locked(&mut journal, json!({"kind":"command","command":canceled}));
            }
            if request.operation.is_priority() {
                journal.priority.push_back(id.clone());
            } else {
                journal.ordinary.push_back(id.clone());
            }
        }
        journal.entries.insert(
            id,
            Entry {
                dispatched: false,
                request,
                identity,
                receipt: receipt.clone(),
            },
        );
        self.event_locked(&mut journal, json!({"kind":"command","command":receipt}));
        Ok((
            if status == CommandStatus::Accepted {
                StatusCode::ACCEPTED
            } else {
                StatusCode::CONFLICT
            },
            receipt,
        ))
    }

    fn event_batch(&self, after: u64) -> Result<EventBatch, ApiError> {
        let journal = self.shared.journal.lock().unwrap();
        if after > journal.sequence {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "future_event_cursor".into(),
            ));
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

#[derive(Debug)]
struct ApiError(StatusCode, String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
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
        return ApiError(StatusCode::UNAUTHORIZED, "unauthorized".into()).into_response();
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
    control
        .command(&id)
        .map(Json)
        .ok_or(ApiError(StatusCode::NOT_FOUND, "unknown_command_id".into()))
}

async fn put_command(
    State(control): State<Control>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<CommandReceipt>), ApiError> {
    control
        .admit(id, body)
        .map(|(code, receipt)| (code, Json(receipt)))
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
            let (session, sequence) = cursor.rsplit_once(':').ok_or(ApiError(
                StatusCode::BAD_REQUEST,
                "invalid_event_cursor".into(),
            ))?;
            if session != control.shared.session {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    "event_cursor_session_changed".into(),
                ));
            }
            sequence
                .parse()
                .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "invalid_event_cursor".into()))?
        }
    };
    let wait = query.wait.unwrap_or(0.0);
    if !wait.is_finite() || !(0.0..=30.0).contains(&wait) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "wait_must_be_between_0_and_30_seconds".into(),
        ));
    }
    // Subscribe before inspecting the journal, so publication between read and wait is visible.
    let mut changed = control.shared.changed.subscribe();
    let batch = control.event_batch(after)?;
    if !batch.events.is_empty() || batch.gap || wait == 0.0 {
        return Ok(Json(batch));
    }
    let _ = tokio::time::timeout(Duration::from_secs_f64(wait), changed.changed()).await;
    Ok(Json(control.event_batch(after)?))
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
