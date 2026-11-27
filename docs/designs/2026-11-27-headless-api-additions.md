# Headless player API additions (central-6ly1)

Status: revision 3.1, 2026-11-27. This is the last document-only round; see "Deferred to code
review" at the end. Revisions 1 and 2 were each reviewed by gpt-6.2-sol and
claude-opus-5-5. Most findings against revision 2 concerned per-operation server evidence that a
design document cannot settle without a server. Coordination approved restructuring the document
into two parts:

- **Part A, the frozen core.** These cross-cutting rules hold for every operation. Change them
  only through the coordination thread.
- **Part B, the registry.** Every operation starts at `unknown`. The operation's own PR promotes a
  row, and only with attributable evidence, a native-source citation and a named live run.

`docs/headless-player.md` documents what has shipped. Everything extends `/v2/player`. Existing
routes, operations, fields and statuses remain unless the compatibility table lists them.

## Compatibility with client d034b7e

`d034b7e` is the comparison baseline. The Rust server's validation pin moved to `fea4596` on
2026-11-27, and coordination moves it further as each change ships. Coordination approved every row on 2026-11-27, except row 15 of
revision 2, which is withdrawn: `target` keeps sending `OP_TargetMouse`.

| # | Item | d034b7e | After |
| --- | --- | --- | --- |
| 1 | Stop on the wire | Resting position sent up to 1.3 s late, carrying the run's delta and animation | One zero-delta, animation-0 update on the first physics step at rest. **Shipped** in fea4596, with correction-adoption gating |
| 2 | `clock_ms`, `self.last_position_send` | Absent | Present. **Shipped** in fea4596 |
| 3 | `state_changed` events | About one every 100 ms; long-polls return early | Not published |
| 4 | Terminal receipts | A readiness drop rewrites `applied_local` to `unknown`; `unknown` can later become `confirmed` or `refused` | Never change (A2) |
| 5 | Network receipts | `unknown` with `processed_without_server_confirmation` | `sent` while evidence is possible, then `unknown`, unless the registry promotes the operation |
| 6 | `reason` | Codes, except validation failures, which are prose | Always a code; prose moves to `detail` |
| 7 | Refused at admission | Takes a journal capacity slot | No capacity slot; the ID still can never be reused (A3) |
| 8 | Journal capacity | 4096 entries for the process lifetime, then 503 | A3 |
| 9 | `nearby` order | By spawn ID | By distance from the published position, then spawn ID. Ships with central-6ly1.8 |
| 10 | Range checks | Measured from the `gs.player_x/y/z` mirror | Measured from the published `self.position`. Ships with central-6ly1.8 |
| 11 | `camp` while dead | Refused `not_ready` | Admitted (A2.5) |
| 12 | `busy` | Any non-priority command is refused while another is `accepted` or `sent` | Only the conflicts in A2.4 |
| 13 | Event buffer | 2048 events | 32 768 events; the event wrapper gains `at_ms` |
| 14 | `attack_off` receipt | `applied_local` | `applied_local`; its `evidence.packets` names the packet it queued (A2.1) |
| 15 | Process exit | Stays `failed` until signalled, then exits 0 | A6 |
| 16 | Profile-file error exit | 1 | 2 (bad arguments were already 2) |
| 17 | Expired command ID | Not possible | 410 `command_id_expired` (A3) |
| 18 | Movement and attack-lease deadlines | Wall clock | Accumulated physics time for movement; the attack lease stays on the network owner's monotonic clock (A7) |
| 19 | Readiness on stdout | `API_PORT=` before login | Also `EQOXIDE_READY` and `EQOXIDE_EXIT` lines (A6) |
| 20 | `session_ended` event | Published when the network task ends | Kept. The EXIT line carries the typed cause |
| 21 | Reason codes | `busy`, `session_or_zone_not_ready`, `precondition_failed_or_network_mailbox_busy`, `queued_without_server_receipt`, `superseded_by_safety_command` | Replaced by the A2 codes. Admission: `not_ready`, `dead`, `busy_merchant`, `busy_inventory`, `busy_same_kind`, `lifecycle_pending`. Claim (A2.3): `cancelled`, `network_mailbox_busy`, `dispatch_timeout`. Evidence: `no_server_evidence_within_window`, `no_server_receipt_in_protocol`. `superseded_by_safety_command` remains for a `move_for`, `goto` or `attack` cancelled by `stop` or `attack_off` |
| 22 | Death and readiness | Death makes the host not Ready, which fences every pending command | Death is not a readiness drop. A2.5 |
| 23 | `sit`, `stand`, `say`, `tell`, `sell`, `merchant_close`, `inventory_move` receipts | `unknown` straight after dispatch | `sent`, then `unknown`, until the registry promotes the operation |
| 24 | Attack renewal | A new `attack` is refused `busy` while the previous one is non-terminal; otherwise it re-sends | An admitted `lease_extended` (A2.4) |
| 25 | Transaction block | `buy`, `sell` and `inventory_move` share the uncertainty block | The inventory-changing class adds `give` and `loot` (A2.4) |
| 26 | Safety reserve | One extra identity each for `stop` and `attack_off` past capacity | 1024 each, past the identity limit (A3) |
| 27 | `api_port: 0` | Invalid | Ephemeral port (A6) |
| 28 | Bind failure and logout-deadline exits | Exit 1 | `bind_failed` 3, `logout_incomplete` 6 |
| 29 | Malformed command body | 422 `invalid_command_payload` | Unchanged (A3) |
| 30 | `busy` scope (correction to row 12) | Another **non-priority** command `accepted` or `sent` | A2.4 |

The `motion_expired` and `motion_stopped` events stay alongside `completion`. `say "/camp"` stays
a local camp toggle and is routed through the `camp` lifecycle rules (A2.5).

# Part A: frozen core

## A1. Clock

`clock_ms` sits at the top level of state. It is the host's monotonic clock in milliseconds,
starting just after the control port binds. Every timestamp the API adds is named `at_ms` or ends
in `_at_ms`, on this clock.

## A2. Receipts (central-6ly1.7)

### A2.1 Statuses

| Status | Terminal | Meaning |
| --- | --- | --- |
| `accepted` | no | Admitted, not yet claimed by its executor |
| `sent` | no | The network owner claimed it (A2.3) and the transport accepted its first owned packet. For a reliable packet, that means it was queued into the reliable window, from which it is retransmitted. For an unreliable packet, it means the socket send was attempted. `evidence.packets` lists the trace `n` of every owned packet up to the terminal transition |
| `applied_local` | yes | A local-only operation took effect in the controller. Any packet it caused is listed in `evidence.packets`; the receipt does not depend on that packet |
| `confirmed` | yes | Evidence attributable to this command, as defined by its registry row, arrived. `evidence.kind` names it, and `evidence.packets` includes the confirming inbound `n` |
| `refused` | yes | Guaranteed to have had no effect: refused at admission, before the network claim, or by an explicit server rejection the registry names. `reason` says which |
| `unknown` | yes | Any other end: claimed and the evidence window lapsed, or the receipt was fenced after its claim |

**The attribution rule.** `confirmed` requires evidence the registry row defines and that can only
have been caused by this command. Anything weaker is `unknown`. When two outstanding commands could
explain the same reply, both become `unknown`.

Receipt fields: `id`, `status`, `terminal`, `reason`, `detail`, `evidence`, `accepted_at_ms`,
`updated_at_ms` and `completion`.

**Exactly-once** means one execution per admitted ID within one session. Reliable bytes already
queued in the transport window can still be retransmitted after a fence. Dropping them would leave
a sequence gap.

### A2.2 Terminality and fences

- **A terminal receipt never changes.** `status`, `reason` and `evidence` are fixed when written.
  The only later write is `completion`, which goes from absent to present once (A2.6).
- **Fences** (`not_ready`, `zone_epoch_changed`) act only on non-terminal receipts. An `accepted`
  receipt that was not yet claimed becomes `refused`; a claimed one becomes `unknown`.
- **Fence-spanning operations.** `respawn`, `cross_zone` and `camp` are not fenced by the readiness
  drop or epoch change they cause. They settle by their registry rows, or end `unknown` at their
  deadline.

### A2.3 The network claim and exactly-once

- The host's dispatch puts the request in the network mailbox together with its command ID, a
  deadline and a cancellation flag. Dispatch alone never sets `sent`.
- **One transition point.** Each admitted command has one claim slot, a small mutex-guarded state:
  `unclaimed` → `claimed` → `sending` → `done`, or `unclaimed` → `cancelled`.
  - Every path that can stop a command goes through the slot's `try_cancel`, which succeeds only
    while the slot is `unclaimed`. That covers fences, death, priority cancellation, lifecycle start
    and the dispatch deadline. A won cancel publishes `refused`.
  - The network owner's `try_claim(epoch, now)` checks the cancel, epoch and deadline and moves to
    `claimed` atomically. Only a won claim may send.
  - A cancel that loses to a claim leaves the claim standing. The receipt then ends `unknown` (or as
    its evidence decides), never `refused`.
- **Lock order:** mailbox, then claim slot. The journal lock is taken only after every slot lock is
  released, and never while holding one. No I/O and no `.await` happen under any of these locks.
  Receipt outcomes are published after the locks are released.
- The network owner **claims** the request in one step through its claim slot. It checks that the
  slot is not cancelled, that the request's zone epoch is current and that the deadline has not
  passed. Only then does it send.
  - A request that fails the claim produces no packet and becomes `refused`, with `cancelled`,
    `stale_zone_epoch_before_dispatch` or `dispatch_timeout`.
- After the claim, a cancellation or fence can only produce `unknown`.
- If a later packet of a multi-packet request fails to reach the transport, the receipt becomes
  `unknown` with `partial_send`.
- A request that is not claimed within 2 s of acceptance becomes `refused` with
  `dispatch_timeout`.
- Priority commands: `stop` cancels every unclaimed `move_for` and `goto`, and `attack_off`
  cancels every unclaimed `attack`. Priority work is claimed before ordinary work in the same
  tick.
- **Single-slot mailboxes.** When a request's mailbox slot is still occupied, the host retries the
  dispatch every host tick until the claim deadline. If the slot never frees, the command becomes
  `refused` with `network_mailbox_busy`.
- **Named implementation work for the claim:**
  - `camp`, `respawn` and `cross_zone` get `Action` labels and go through the tracker.
  - The tracker's 30 s `expire` and its epoch fence exempt fence-spanning operations.
  - Trace capture returns `n`, so a receipt's `evidence.packets` can be filled.
- **Execution lifetime is separate from receipt terminality.**
  - A command's execution owns trace attribution until the execution finishes or is cancelled, even
    after its receipt is terminal. Examples: loot item takes and the close after `corpse_opened`, a
    `face` heading update, an `attack_off` packet.
  - `evidence.packets` is frozen at the terminal transition. `GET /v2/player/packets?command=ID`
    remains the complete list.
  - A fence suppresses an execution's unsent follow-ups, except named cleanup steps: the loot close,
    the logout, the trade cancel.
  - A fence-spanning operation owns only the transition it requested: one zone change, one respawn,
    one camp logout.
- The action-to-command map is bounded by A3's live limit. Entries are removed at terminal.

### A2.4 Conflicts

| Class | Members | Rule |
| --- | --- | --- |
| Merchant context | `merchant_open`, `merchant_close`, `buy`, `sell` | One non-terminal at a time per session (`busy_merchant`) |
| Inventory-changing | `buy`, `sell`, `inventory_move`, `give`, `loot` | One execution at a time (`busy_inventory`), including a loot pipeline that is still taking items after its receipt is terminal. Any `unknown` in this class blocks further members until relog, as the shared transaction block does today |
| Same-reply kinds | `target`, `consider`, `hail`, `door`, `memorize`, `cast`, and each chat channel (`say`, `tell`, `group`, ...) | One non-terminal per kind (`busy_same_kind`). Replies are matched by a quarantine key of kind plus entity, door or gem. A reply matching the key of a now-terminal command is never evidence for a newer one. If attribution stays ambiguous, the receipt is `unknown` |
| Attack | `attack` | A new `attack` while auto-attack is on is an admitted lease renewal. It becomes `applied_local` with `evidence.kind: "lease_extended"` and `extends`, the ID of the attack that turned auto-attack on. Only that originating attack can be confirmed by a swing. The lease and its target are bound at the network owner when auto-attack turns on. `attack_off` or a retarget ends the lease |
| Lifecycle | `camp`, `respawn`, `cross_zone` | While one is non-terminal, ordinary commands are refused with `lifecycle_pending`. When one starts, every unclaimed ordinary command is cancelled with `lifecycle_pending`; claimed ones run on. `stop` and `attack_off` are still admitted |
| Everything else | `move_for`, `goto`, `face`, `jump`, `stop`, `attack_off`, `sit`, `stand` | No conflict |

A command belongs to every class it is listed in. For example, `buy` must pass both the merchant
and the inventory checks.

The internal self-consider liveness probe stays reserved: a consider reply naming self never
confirms a user `consider`.

### A2.5 Admission while dead

- Death is not a readiness drop. Readiness no longer includes `player_dead_since`, so a character
  that logs in dead still reaches Ready and prints `EQOXIDE_READY`.
- At death, every non-terminal command other than `stop`, `attack_off`, `camp` and `respawn` is
  fenced with reason `dead`: `refused` if not yet claimed, `unknown` if claimed.
- While dead, the host admits `stop`, `attack_off`, `camp` and `respawn`. Everything else is
  refused with `dead`.
- Admission checks run in this order:
  1. `stale_session`
  2. `stale_zone_epoch`
  3. `invalid_operation`
  4. `not_ready`
  5. `dead`
  6. the conflict classes, in A2.4 order

  `stop` and `attack_off` skip checks 4 to 6. Checks 1 to 3 still apply to them.
- `respawn` while alive is refused with `not_dead`.
- `say "/camp"` follows `camp`'s rules.

### A2.6 Completion

- `completion` is `{"result","at_ms",...}`, written once, for operations whose registry row
  declares one.
- A `command_completed` event carries the same object, the command ID and the event cursor.
- A receipt that owes a completion cannot be evicted.
- A refused operation owes none.
- An interruption's completion (fence, death, lifecycle start, supersession) is written by the
  host in the same tick as the cause, so no receipt waits for an event that never comes.

## A3. Command identity and journal (central-6ly1.1)

PUT processing order:

1. **Validate the ID.** An invalid ID is 400.
2. **Known ID.** Payload equality is equality of the parsed JSON values, the comparison used today.
   - Same payload:
     - a live or terminal receipt is replayed with HTTP 200;
     - an expired one returns 410 `command_id_expired` with `{"status","reason"}`.
   - Changed payload: 409 `command_id_reused_with_changed_payload`, expired or not.
   - A known ID is always answered, even when every limit below is reached.
   - GET on an ID refused at admission returns that refusal's receipt, like any other receipt.
3. **New ID.**
   - The capacity checks run first, and a 503 consumes no identity, so a retry with the same ID
     later is a first request again:
     - 503 `session_id_space_exhausted` when 1 048 576 identities exist. `stop` and `attack_off`
       can still use a separate reserve of 1024 identities each. When that reserve is also used up,
       they get 503 `safety_reserve_exhausted`, and the session must be restarted.
     - 503 `live_command_limit` when 64 receipts are non-terminal or owe a completion. This does not
       apply to `stop` and `attack_off`.
   - Then, in one step under the journal lock, the host writes the identity, the initial outcome and
     its receipt event. Admitted and refused-at-admission requests both consume an identity. An
     admission refusal returns 409 with the receipt and `"admitted": false`, and does not count
     toward the live limit.
4. A malformed body (unparseable JSON, unknown fields) stays 422 `invalid_command_payload` and
   consumes no identity.

Retention:

- Full receipts are kept for the newest 65 536 entries that are terminal and owe no completion.
- Older entries keep only their identity: an ID digest, a payload digest, the final status and the
  reason.
- Deduplication covers one running session. A new process is a new session.

## A4. Events (central-6ly1.1)

- `state_changed` is gone. An idle long-poll waits its full `wait`.
- Each event is wrapped as `{"cursor","at_ms","event"}`.
- The buffer holds 32 768 events.
  - Supported load: 5 commands per second at up to 4 receipt events each, and 20 combat swings per
    second, each producing a structured event and the existing text event. That is about 60 events
    per second, so the buffer covers more than 9 minutes.
  - A reader that falls at most 120 s behind loses nothing.
- The host forwards game events from `GameState` once per 20 ms tick. Those source queues hold 200
  chat events and 512 combat records. If a source queue overflowed between two ticks, the host
  publishes `{"kind":"source_events_dropped","source"}`, so no loss is silent.
- Receipts can be re-read with GET while retained (A3). After that, GET returns 410
  `command_id_expired` with the final status and reason. After a `gap`, re-read state and every
  outstanding command.
- A state publication does not wake an event long-poll. A wake on the shared notifier rechecks the
  buffer until the poll's original deadline.
- A PUT response includes `event_cursor`: the newest cursor at the moment the command was admitted.
  A wait on that command's events passes it as `after`, so no event between the PUT and the wait is
  missed.
- Cursors:
  - `after` is exclusive, with the cursor in the form `SESSION:N`.
  - A cursor older than the oldest buffered event minus one returns the newer events with
    `gap: true`.
  - A future cursor is 400. A cursor from another session is 409.
  - A timeout returns no events and the unchanged cursor.

## A5. Wait (central-6ly1.10)

`POST /v2/player/wait` takes `{"timeout_ms": 1..30000, "until": <node>}`.

### Leaf nodes

- **Command:** `{"command":"ID"}` with exactly one of `"status":[...]`, `"terminal":true` or
  `"completed":true`.
- **Event:** `{"event":{"kind":"...", "<field>": <value>, ...}, "after":"SESSION:N"}`.
  - Fields match by top-level equality.
  - With no `after`, only events published after the call count.
- **State:** `{"state":{"pointer":"/json/pointer", "<op>": <value>}}`.
  - `op` is one of `eq`, `ne`, `lt`, `le`, `gt`, `ge`, `exists` or `within`.
  - `within` takes `{"point":[x,y]|[x,y,z],"radius"}`, and uses 2-D distance when the point has two
    coordinates.
  - A missing pointer or a type mismatch makes the comparison false, except for `exists`.

### Leaf states

Each leaf is `satisfied`, `pending`, `impossible` or `gap`.

- **Impossible:**
  - an unknown or expired command;
  - a terminal command whose status is outside `status`;
  - `completed` on a command that owes no completion and has none;
  - an event `after` from another session.
- **Gap:** an event `after` older than the buffer.

### Composite nodes

`{"all":[...]}` and `{"any":[...]}` take at most 16 children, with depth at most 4.

| Node | `satisfied` | `impossible` | `gap` | `pending` |
| --- | --- | --- | --- | --- |
| `all` | Every child satisfied | Any child impossible | No child impossible, any child gap | Otherwise |
| `any` | Any child satisfied | Every child impossible | Nothing satisfied or pending, any child gap | Otherwise |

### Evaluation and response

- Evaluation reads one snapshot of state, journal and event buffer under the journal lock. It
  re-runs on every publication and event until the result is not `pending` or the timeout passes.
- The response is
  `{"result":"satisfied"|"timeout"|"impossible"|"gap","at_ms","state":{...},"matches":[...],"cursor","detail"}`.
  - Each entry of `matches` is `{"path","command"}` or `{"path","event","cursor"}` for a satisfied
    leaf.
  - The top-level `cursor` is the newest event cursor at evaluation. To chain waits without
    skipping buffered events, pass the matched event's own `cursor` as the next `after`, not the
    top-level one.
- The HTTP status is 200 for every result. An invalid node is 400 `invalid_predicate`.

## A6. Process contract (central-6ly1.3)

### Startup and stdout

- `api_port: 0` binds an ephemeral port. Every port printed is the bound one.
- stdout carries only these lines, written with errors (including EPIPE) ignored. Logs go to
  stderr, and `RUST_LOG` is honoured.
  - `API_PORT=<port>`, printed at bind.
  - `EQOXIDE_READY {"api_port","session","zone_epoch"}`, printed once, the first time the host is
    Ready and admission is open.
  - `EQOXIDE_EXIT {"status","reason","command_id","observed","clock_ms"}`, printed once, as the
    last line.

### Readiness timeout and signals

- A zone-entry failure before the first Ready ends the process with `login_failed` and
  `reason: "zone_entry"`. After the first Ready, a failed zone change publishes `zone_failed`. The
  process exits `session_lost` only if the session is gone.
- If the host is not Ready within `ready_timeout_ms` (profile, default 120 000), it exits with
  `never_ready`.
- SIGINT, SIGTERM and SIGHUP request normal logout. A signal before Ready ends the login attempt
  and exits with `signal_logout`, with `observed: "before_ready"`.
- A second signal during logout exits at once with `signal_forced`.

### Termination cause

- The network phase that owns the stream returns a typed termination cause. Panics in the gameplay
  phase are caught inside that phase, while the stream is still owned, so the logout can be
  attempted before the process reports `network_panic`.
- A GM kick or a server-side disconnect is `session_lost`, with `reason` naming it.
- Precedence, highest first:
  1. `network_panic`
  2. `internal_error`
  3. `login_failed`
  4. `never_ready`
  5. `signal_forced`
  6. `logout_incomplete`
  7. `camped`
  8. `signal_logout`
  9. `session_lost`

  `logout_incomplete` carries the initiator in `reason` (`camp` or `signal`).
- Shutdown is a bounded state machine: `running` → `logging_out` (camp timer done, or a signal) →
  `exited`, with an 8 s logout deadline.
  - A server drop or 15 s of link silence while `running` is `session_lost`. While `logging_out`,
    the status follows the initiator.
  - The emergency logout send uses poison-tolerant access to transport health and the trace, so a
    panic that poisoned them cannot panic the cleanup.
- `observed` states what the client saw. For example, `camp_timer_completed_logout_sent` means
  logout was handed to the transport. RoF2 maps no `OP_LogoutReply`
  (`eqemu:utils/patches/patch_RoF2.conf@cd66dd0a6:76`), so the client cannot wait for an
  acknowledgement.
  - For `camped`, `command_id` is the camp command's ID.
  - A `camp` receipt can never be read after exit, so the EXIT line is its terminal record.

### Exit status

| `status` | Code |
| --- | --- |
| `camped` | 0 |
| `signal_logout` | 0 |
| `internal_error` | 1 |
| `network_panic` | 1 |
| `profile_error` | 2 |
| `usage_error` | 2 |
| `bind_failed` | 3 |
| `login_failed` | 4 |
| `session_lost` | 5 |
| `logout_incomplete` | 6 |
| `signal_forced` | 6 |
| `never_ready` | 7 |

A handled SIGTERM exits 0, so supervisors need no special case. The EXIT line distinguishes
`camped` from `signal_logout`.

## A7. Host robustness (central-6ly1.11)

- No `lock().unwrap()` on state shared with the network thread. A poisoned lock fails the session
  through the A6 path.
- Geometry loads on a blocking task keyed by zone epoch. The host stays not Ready until the load
  for the current epoch finishes, and a result for an older epoch is discarded.
- `move_for` and `goto` deadlines count accumulated physics time, with each step clamped to 50 ms,
  so a stall cannot shorten movement.
- The attack lease stays on the network owner's monotonic clock, so a host stall cannot leave the
  server swinging.
- State gains `freshness.host_stall_ms`: the longest gap between host steps in the last 10 s.

## A8. Packet trace (central-6ly1.2)

The contract is the one PR #4 implements, with these fixed parts.

- **Route.** `GET /v2/player/packets?after&limit&opcode&command` returns
  `{"session","packets","next","gap","counters"}`.
- **Capture.** Only the zone stream is recorded, always on for headless sessions, in a per-session
  ring of 20 000 rows on the host clock.
- **Row fields.** `n`, `at_ms`, `dir`, `opcode`, `opcode_name`, `len`, `reliable`, `wire_seq`,
  `acked_at_ms` (inbound reliable rows: when the client's ACK covering it was sent), `command_id`
  and `fields`.
  - `fields` holds raw wire values. Its keys for each opcode are listed in
    `docs/headless-player.md` and stable once listed.
- **`n` propagation.** Capture returns `n`, and inbound packets carry their `n` into packet
  handling. This is how receipts list evidence.
- **Cursor.** `after` is exclusive. `next` is the highest `n` scanned, so an empty filtered page
  still advances. `gap` is computed before filtering. A new `session` restarts at 0.
- **Counters.** Named and defined exactly as in `docs/headless-player.md`, by transport unit
  (datagram or app packet).

## A9. Client CLI (central-6ly1.10)

The binary is `eqoxide-headless-ctl`. Every subcommand prints one JSON object on stdout and exits 0
on success. Exit 2 is a usage error, 3 means the target process is gone, and 4 is an API error.

### `launch`

`launch --account FILE --login HOST:PORT --world-port N --world-name NAME --world-host HOST
--character NAME --maps DIR [--create JSON] [--dir DIR]`

- `--account` is a mode-0600 JSON file holding `{"username","password"}`.
- It writes a mode-0600 profile with a fresh token into a private directory.
- It forks a **supervisor** that starts `eqoxide-headless` with `api_port: 0`, collects its stdout
  lines into `DIR/ready.json` and `DIR/exit.json`, and records the child's PID and start time.
- `launch` returns after `ready.json` exists, after the child exits, which prints `exit.json` and
  exits 4, or after `--timeout-ms`, default 180 000, exit 4. On success it prints
  `{"dir","api_port","session","zone_epoch"}`.

### `command`

`command --dir DIR KIND [JSON]`

- It reads `session` and `zone_epoch` from state and mints an ID: the session followed by a
  random 64-bit hex value.
- It writes the ID and the exact PUT body to `DIR/pending/ID.json` before the first PUT. A retry
  after a transport error reuses that ID and body, up to 3 times. `command --dir DIR --resume ID`
  re-sends a pending command after its retries are exhausted.
- An uncertain transaction is never retried under a new ID.
- It prints the receipt.

### Other subcommands

- `wait --dir DIR PREDICATE_JSON [--timeout-ms N]` (A5).
- `state`, `events --after CURSOR`, `packets ...`.
- `stop --dir DIR`
  - It verifies the PID's start time, sends SIGTERM, then waits for `exit.json` and prints it.
  - If the process is already gone, it prints the existing `exit.json`, or exits 3 if there is
    none.

Relog is a new process and a new session. In-process character switching stays out of scope.

# Part B: registry

## B1. Promotion rule

Every row below is the only place an operation's receipt evidence is defined. A row is promoted
from `unknown` only by that operation's PR, which states:

- the evidence and why it can only have been caused by the command;
- the native EQEmu source it was checked against, as an `eqemu:` citation;
- the server and the run (commit, run ID) that showed it live.

A promotion without a live run is not allowed. The row stays `unknown`, with the run it needs
noted. The Rust fixture lacks player death and zone crossing, so `respawn` and `cross_zone` need a
native-server run, requested through coordination.

## B2. Rows

| Operation | Bead | Terminal path today | Evidence (once promoted) | Completion | Notes |
| --- | --- | --- | --- | --- | --- |
| `move_for` | – | `applied_local` | Local | `expired`, `stopped`, `superseded`, `interrupted` | Shipped. Completion is new |
| `stop` | – | `applied_local` | Local | – | Shipped |
| `goto` | .4 | `applied_local` | Local | `arrived`, `blocked`, `timeout`, `stopped`, `superseded`, `interrupted` | Semantics in B3 |
| `face` | .4 | `applied_local` | Local; the heading update is an owned deferred send | `heading_sent` with sequence and `n`, or `interrupted` | Native rebroadcasts a heading-only change to other clients, so the live test checks it from a second headless client's `nearby[].heading`. Proposed in place of the server-log wording of the .4 AC |
| `jump` | .4 | `applied_local` | Local | – | Needs `on_ground`, not `in_water`, not sitting. Allowed while moving |
| `attack_off` | – | `applied_local` | Local; owns its `OP_AutoAttack` packet | – | Row 14 |
| `target` | .7 | `sent` → `unknown` | None direct. Native `OP_TargetMouse` gets no reply (`eqemu:zone/client_packet.cpp@cd66dd0a6:15125`). The server-side target is proven by an attributed swing | – | Amended .7 AC. Row 15 of revision 2 is withdrawn. The `OP_Consider` reply the target drain requests is evidence only for that auxiliary consider; native answers it even if `OP_TargetMouse` set nothing (`eqemu:zone/client_packet.cpp@cd66dd0a6:5109`) |
| `attack` | .7 | `sent` → `unknown` | Candidate: the first `OP_Damage` with source = self, target = the target at claim, melee, sent after the claim and within the lease | – | Native sends no toggle reply (`@cd66dd0a6:3517`). Riposte and in-flight swings need defining before promotion |
| `sit`, `stand` | – | `sent` → `unknown` | None. Native `Handle_OP_SpawnAppearance` excludes the sender (`@cd66dd0a6:14771`) | – | `self.sitting` is the client's own state |
| `say`, `tell`, `chat` | .9 | `sent` → `unknown` | Candidates: `tell` on `ChatChannel_TellEcho` (`eqemu:zone/worldserver.cpp@cd66dd0a6:237`), which shows the receiving zone queued the message, not that the recipient's client received it; queued, queue-full and offline replies take separate paths. For other channels, the self-echo, which depends on chat filters (`eqemu:zone/entity.cpp@cd66dd0a6:2275`) | – | Text can be redacted or garbled; `#` and `^` commands give no echo |
| `hail` | .9 | `sent` → `unknown` | NPC speech is an observation, not evidence | – | – |
| `consider` | .9 | `sent` → `unknown` | Candidate: an `OP_Consider` reply naming the entity, with the self-probe excluded | – | Sends a consider only; no retarget |
| `cast` | .9 | `sent` → `unknown` | Candidate: the awaited path's `CastEnd`: `completed`, `fizzled`, `interrupted`, plus `cast_failed` as `refused` | – | `gem` 0–8. The target is in the payload; no retarget. The window must match `CAST_PENDING_REAP` (14 s) and the 30 s echo quarantine |
| `memorize` | .9 | `sent` → `unknown` | Candidate: the `OP_MemorizeSpell` echo | – | `gem` 0–8 |
| `door` | .9 | `sent` → `unknown` | `OP_MoveDoor` is a broadcast, so it is an observation only | – | – |
| `loot` | – | `confirmed` on `corpse_opened` | Shipped. Ownership of the pipeline's packets is in PR #4 | – | – |
| `merchant_open`, `buy` | – | `confirmed` / `refused` | Shipped: the server echo; `OP_ShopEndConfirm` for buy. An unsolicited shop end makes the receipt `unknown` | – | Merchant-context serialisation (A2.4) |
| `merchant_close` | – | `sent` → `unknown` | `OP_ShopEndConfirm` (`eqemu:zone/client.cpp@cd66dd0a6:13285`) is an empty notification that native also sends for buy failures and unsolicited merchant teardown, so it is an observation only | – | Merchant-context class |
| `sell` | – | `sent` → `unknown` | Candidate: `OP_ShopPlayerSell` echo; `itemslot = -1` means vetoed, which becomes `refused` with `sell_vetoed` | – | – |
| `inventory_move` | – | `sent` → `unknown` | None. Native applies the move silently | – | The transaction lock remains |
| `give` | .9 | `sent` → `unknown` | `returned` needs a received matching item; `accepted` evidence is to be defined | – | Inventory serialisation; ambiguous results lock transactions |
| `respawn` | .5 | `sent` → `unknown` at 330 s, the native 300 s hover default plus margin | Candidate: recovery after this death's hover window, either Ready after re-entry or in-zone recovery | – | Needs a native run. The bind selection is sent once; the reliable transport retransmits |
| `cross_zone` | .6 | `sent` → `unknown` at 60 s | Candidate: `zoned` whose zone equals the standing line's destination. A `SendZoneCancel` (`success = 1` with the current zone, `eqemu:zone/zoning.cpp@cd66dd0a6:382`) rewinds the server's position, so it is recorded as an observation under `unknown`, not `refused` | – | Needs a native run. Semantics in B3 |
| `camp` | .3 | EXIT line | `camp_timer_completed_logout_sent` | – | A6 |

## B3. Semantics that do not depend on evidence

### `goto`

Fields:

- `x`, `y`: the goal.
- `z`: optional.
- `tolerance`: 1.0–50, default 2.
- `z_tolerance`: 1–50, default 8. Used only when `z` is given.
- `timeout_ms`: 1–120 000, required.

Behaviour:

- Steering follows a straight line with local collision and no pathfinding.
- On the step that would overshoot, the host scales `MoveIntent.speed` so the body stops on the
  goal. Scaling the wish direction would not work, because the controller normalises it.
- Each step evaluates, in this order:
  1. `arrived`: the 2-D distance is at most `tolerance`, and, when `z` was given, `|dz|` is at most
     `z_tolerance`.
  2. `blocked`: over the last 1000 ms of physics time, the distance to the goal fell by less than
     0.5. This is first evaluated 1000 ms after start. Arrival in 2-D with `|dz|` out of tolerance
     is reported as `blocked`.
  3. `timeout`.
- `stopped`, `superseded` and `interrupted` (with `reason` `sit`, `dead`, `zone_entry`,
  `not_ready` or `camp`) end it.

### `face`

- Takes exactly one of `heading` (degrees counter-clockwise from north, as in state) or
  `entity_id`.
- During `goto` or `move_for` it is refused with `moving`. A newer `face` supersedes an older one,
  which completes `superseded`. So does a movement command that starts while the heading update is
  waiting for a correction, so a deferred heading cannot race new movement. The completion result for a sent heading is `heading_sent`.
- The heading update bypasses the 280 ms rest spacing.
- While a correction is unadopted the update waits for adoption, for at most 2 s. After that it
  completes `interrupted` with `correction_pending`.

### `cross_zone`

- It means "cross the line I stand in".
  - `not_in_zone_line`: no local zone-line region at the stable position.
  - `region_data_unavailable`: no region map.
- Before the request, the host requires the body to be at rest and requires a rest update from
  that position to have been sent at least 300 ms earlier. This is a heuristic: it orders nothing at
  the server, which resolves the line from its stored position. The host re-checks position and
  epoch just before sending. State shows the locally predicted destination; the server-selected one
  appears only in `zoned`.
- A same-zone translocator follows the existing reposition path and sends no `OP_ZoneChange`. That
  branch ends `applied_local` with `same_zone_reposition`.
- `zone.lines` in state lists `OP_SendZonepoints` entries as **destinations**.
  `zone.line_at_position` is the local region index, or null.

### `respawn`

- It is bound to the current death: `self.dead` and the held hover window.
- The bind selection is sent once. Native uses the same recovery path for an explicit and an
  automatic bind selection, so a recovery after the request is an observation whose cause is
  unknown. It is recorded under `unknown` unless the registry row promotes a distinguishable case.

### Other operations

- `hail` takes an optional `entity_id`; the default is the current target.
- `consider` takes `entity_id`.
- `cast` takes `gem` and an optional `target_id`.
- `memorize` takes `gem` and `spell_id`.
- `door` takes `door_id`.
- `give` takes `entity_id`, `slot` and the current `inventory_revision`.
- `chat` takes a `channel` (`group`, `guild`, `shout`, `ooc` or `auction`) and `text` of 1–512
  bytes.

### `server_position` (shipped in .8)

The server never sends a client its own ordinary position updates, because the rebroadcast
excludes the sender. `self.server_position` is therefore set only by server-originated moves:
corrections, summons, translocations and zone placement. It is null in normal play. Proposed
amendment to the .8 AC: "the last server-asserted self position".

### New state and events

These go with their Beads:

- `zone.lines` and `zone.line_at_position` (.6).
- `doors` (.9).
- `zoned` and `zone_failed` events (.6), each with `command_id` and `zone_epoch`. `zoned` is
  published in the same publication as the destination state and the matching receipt.

## Wire impact

| Section | Change |
| --- | --- |
| A1, row 1 | Shipped stop flush |
| A7 | Physics-time movement deadlines change when `move_for` stops only during a host stall |
| B3 | New operations send their existing packets only when invoked |
| Everything else | None |

## Deferred to code review

Coordination decided on 2026-11-27 that these points are settled in the `.1` and `.7`
implementation PRs, with a test for each, rather than in this document. Reviewers hold those PRs to
them.

1. The claim-slot transition (A2.3) and its lock order: no I/O under locks, and outcomes published
   after the locks are released. Tests race a fence, death, a priority stop and the deadline against
   a claim.
2. Execution lifetime against receipt terminality (A2.3): loot follow-ups after `corpse_opened`,
   `face` and `attack_off` deferred sends, and cleanup steps under a fence.
3. The transport boundary for `sent` and `partial_send`, including a failed first packet. `n` is
   propagated on inbound `AppPacket`s (A8).
4. Attack lease binding at the network owner, renewal, and ordering with `attack_off` and
   retargeting (A2.4).
5. Loot as a single pipeline, with replies correlated to the request that sent the open (A2.4).
6. The atomic journal commit and every capacity edge (A3), including a one-hour, 5 command/s soak
   test that checks continued admission and an uninterrupted receipt-event cursor (.1 AC).
7. The shutdown state machine (A6): a panic while holding a shared lock, a map load stalled past a
   host tick (.11 AC), and silence becoming `session_lost`.
