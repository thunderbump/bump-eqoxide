# Headless player API additions (central-6ly1)

Status: revision 3, 2026-11-27. Revisions 1 and 2 were each reviewed by gpt-6.2-sol and
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

The Rust server validates against client `d034b7e`. Scripts must tolerate both columns until
coordination moves that pin. Coordination approved every row on 2026-11-27, except row 15 of
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
| `sent` | no | The network owner claimed it (A2.3) and made the first transport attempt for a packet it owns. `evidence.packets` lists the trace `n` of every owned packet so far |
| `applied_local` | yes | A local-only operation took effect in the controller. Any packet it caused is listed in `evidence.packets`; the receipt does not depend on that packet |
| `confirmed` | yes | Evidence attributable to this command, as defined by its registry row, arrived. `evidence.kind` names it, and `evidence.packets` includes the confirming inbound `n` |
| `refused` | yes | Guaranteed to have had no effect: refused at admission, before the network claim, or by an explicit server rejection the registry names. `reason` says which |
| `unknown` | yes | Any other end: claimed and the evidence window lapsed, or the receipt was fenced after its claim |

**The attribution rule.** `confirmed` requires evidence the registry row defines and that can only
have been caused by this command. Anything weaker is `unknown`. When two outstanding commands could
explain the same reply, both become `unknown`.

Receipt fields: `id`, `status`, `terminal`, `reason`, `detail`, `evidence`, `accepted_at_ms`,
`updated_at_ms` and `completion`.

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
- The network owner **claims** the request in one step under the action tracker's lock. It checks
  that the flag is clear, that the request's zone epoch is current and that the deadline has not
  passed, then marks it claimed. Only then does it send.
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
- Deferred sends carry ownership. A packet sent later on a command's behalf (the loot pipeline's
  requests, the respawn selection, the camp logout, the give trade steps, a `face` heading update)
  carries that command's ID into the trace and the receipt. Ownership ends when the receipt is
  terminal.
- The action-to-command map is bounded by A3's live limit. Entries are removed at terminal.

### A2.4 Conflicts

| Class | Members | Rule |
| --- | --- | --- |
| Merchant context | `merchant_open`, `merchant_close`, `buy`, `sell` | One non-terminal at a time per session (`busy_merchant`). A money-moving `unknown` blocks further transactions until relog, as today |
| Inventory | `inventory_move`, `give` | One non-terminal at a time (`busy_inventory`). An `unknown` blocks transactions until relog |
| Same-reply kinds | `target`, `consider`, `hail`, `door`, `memorize`, `cast`, and each chat channel | One non-terminal per kind (`busy_same_kind`). A later reply that could belong to an earlier, now-terminal command of the same kind never confirms the new one. The window starts only once the previous command of that kind has been terminal for its full window length |
| Attack | `attack` | A new `attack` while one is non-terminal extends the existing lease and returns 409 `busy_same_kind` with the live receipt's ID, so one swing can never confirm two commands |
| Lifecycle | `camp`, `respawn`, `cross_zone` | While one is non-terminal, ordinary commands are refused with `lifecycle_pending`. `stop` and `attack_off` are still admitted |
| Everything else | movement, `stop`, `attack_off`, `say`, `tell` | No conflict |

The internal self-consider liveness probe stays reserved: a consider reply naming self never
confirms a user `consider`.

### A2.5 Admission while dead

- While dead, the host admits `stop`, `attack_off`, `camp` and `respawn`. Everything else is
  refused with `dead`.
- `respawn` while alive is refused with `not_dead`.
- `say "/camp"` follows `camp`'s rules.

### A2.6 Completion

- `completion` is `{"result","at_ms",...}`, written once, for operations whose registry row
  declares one.
- A `command_completed` event carries the same object, the command ID and the event cursor.
- A receipt that owes a completion cannot be evicted.
- A refused operation owes none.

## A3. Command identity and journal (central-6ly1.1)

PUT processing order:

1. **Validate the ID.** An invalid ID is 400.
2. **Known ID.** Payload equality is equality of the parsed JSON values, the comparison used today.
   - Same payload:
     - a live or terminal receipt is replayed with HTTP 200;
     - an expired one returns 410 `command_id_expired` with `{"status","reason"}`.
   - Changed payload: 409 `command_id_reused_with_changed_payload`, expired or not.
   - A known ID is always answered, even when every limit below is reached.
3. **New ID.**
   - The ID's identity record is written first, so the ID is never reusable from this point.
   - `stop` and `attack_off` are always admitted. They draw on 1024 identities reserved for them.
   - Other operations:
     - 503 `session_id_space_exhausted` when 1 048 576 identities exist;
     - 503 `live_command_limit` when 64 receipts are non-terminal or owe a completion.
   - Otherwise A2's admission rules apply. A refusal returns 409 with the receipt and
     `"admitted": false`.

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
- Receipts can always be re-read with GET. After a `gap`, re-read state and every outstanding
  command.
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
  - The top-level `cursor` is the newest event cursor at evaluation, so waits can be chained.
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
- Precedence: `network_panic`, `login_failed`, `camped`, `signal_logout`, `session_lost`.
- `observed` states what the client saw. For example, `camp_timer_completed_logout_sent` means
  logout was handed to the transport; the server sends no logout acknowledgement.
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
- `launch` returns after `ready.json` exists. It prints `{"dir","api_port","session","zone_epoch"}`.

### `command`

`command --dir DIR KIND [JSON]`

- It reads `session` and `zone_epoch` from state and mints an ID: the session followed by a
  random 64-bit hex value.
- It writes the ID to `DIR/pending/` before the first PUT. A retry after a transport error reuses
  that ID, up to 3 times.
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
| `face` | .4 | `applied_local` | Local; the heading update is an owned deferred send | `sent` with sequence and `n`, or `interrupted` | Server receipt is checked from the server log in the live test (amended .4 AC) |
| `jump` | .4 | `applied_local` | Local | – | Needs `on_ground`, not `in_water`, not sitting. Allowed while moving |
| `attack_off` | – | `applied_local` | Local; owns its `OP_AutoAttack` packet | – | Row 14 |
| `target` | .7 | `sent` → `unknown` | None direct. Native `OP_TargetMouse` gets no reply (`eqemu:zone/client_packet.cpp@cd66dd0a6:15125`). The server-side target is proven by an attributed swing | – | Amended .7 AC. Row 15 of revision 2 is withdrawn |
| `attack` | .7 | `sent` → `unknown` | Candidate: the first `OP_Damage` with source = self, target = the target at claim, melee, sent after the claim and within the lease | – | Native sends no toggle reply (`@cd66dd0a6:3517`). Riposte and in-flight swings need defining before promotion |
| `sit`, `stand` | – | `sent` → `unknown` | None. Native `Handle_OP_SpawnAppearance` excludes the sender (`@cd66dd0a6:14771`) | – | `self.sitting` is the client's own state |
| `say`, `tell`, `chat` | .9 | `sent` → `unknown` | Candidate: the self-echo, per channel. A tell echo uses channel 14 | – | Normalisation needs defining |
| `hail` | .9 | `sent` → `unknown` | NPC speech is an observation, not evidence | – | – |
| `consider` | .9 | `sent` → `unknown` | Candidate: an `OP_Consider` reply naming the entity, with the self-probe excluded | – | Sends a consider only; no retarget |
| `cast` | .9 | `sent` → `unknown` | Candidate: the awaited path's `CastEnd`: `completed`, `fizzled`, `interrupted`, plus a server refusal as `refused` | – | `gem` 0–8. The target is in the payload; no retarget |
| `memorize` | .9 | `sent` → `unknown` | Candidate: the `OP_MemorizeSpell` echo | – | `gem` 0–8 |
| `door` | .9 | `sent` → `unknown` | `OP_MoveDoor` is a broadcast, so it is an observation only | – | – |
| `loot` | – | `confirmed` on `corpse_opened` | Shipped. Ownership of the pipeline's packets is in PR #4 | – | – |
| `merchant_open`, `buy` | – | `confirmed` / `refused` | Shipped: the server echo; `OP_ShopEndConfirm` for buy. An unsolicited shop end makes the receipt `unknown` | – | Merchant-context serialisation (A2.4) |
| `merchant_close` | – | `sent` → `unknown` | Candidate: `OP_ShopEndConfirm` while no buy is pending | – | – |
| `sell` | – | `sent` → `unknown` | Candidate: `OP_ShopPlayerSell` echo | – | – |
| `inventory_move` | – | `sent` → `unknown` | None. Native applies the move silently | – | The transaction lock remains |
| `give` | .9 | `sent` → `unknown` | `returned` needs a received matching item; `accepted` evidence is to be defined | – | Inventory serialisation; ambiguous results lock transactions |
| `respawn` | .5 | `sent` → `unknown` at 30 s | Candidate: recovery after this death's hover window, either Ready after re-entry or in-zone recovery | – | Needs a native run. The bind selection is sent once; the reliable transport retransmits |
| `cross_zone` | .6 | `sent` → `unknown` at 60 s | Candidate: `zoned` with the expected destination | – | Needs a native run. Semantics in B3 |
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
  which completes `superseded`.
- The heading update bypasses the 280 ms rest spacing.
- While a correction is unadopted the update waits for adoption, for at most 2 s. After that it
  completes `interrupted` with `correction_pending`.

### `cross_zone`

- It means "cross the line I stand in".
  - `not_in_zone_line`: no local zone-line region at the stable position.
  - `region_data_unavailable`: no region map.
- Before the request, the host requires the body to be at rest and requires a rest update from
  that position to have been sent at least 300 ms earlier. The server resolves the line from its
  stored position.
- A same-zone translocator follows the existing reposition path and sends no `OP_ZoneChange`. That
  branch ends `applied_local` with `same_zone_reposition`.
- `zone.lines` in state lists `OP_SendZonepoints` entries as **destinations**.
  `zone.line_at_position` is the local region index, or null.

### `respawn`

- It is bound to the current death: `self.dead` and the held hover window.
- The bind selection is sent once. If the hover window expires and the server respawns the
  character itself, the command still ends at the next recovery, as an observation recorded in
  `evidence.automatic: true`. It is not promoted to `confirmed` unless the registry row allows it.

### Other operations

- `hail` takes an optional `entity_id`; the default is the current target.
- `consider` takes `entity_id`.
- `cast` takes `gem` and an optional `target_id`.
- `memorize` takes `gem` and `spell_id`.
- `door` takes `door_id`.
- `give` takes `entity_id`, `slot` and the current `inventory_revision`.
- `chat` takes a `channel` (`group`, `guild`, `shout`, `ooc` or `auction`) and `text` of 1–512
  bytes.

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
