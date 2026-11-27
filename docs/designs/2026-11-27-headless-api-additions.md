# Headless player API additions (central-6ly1)

Status: revision 2, 2026-11-27. Revision 1 was reviewed by gpt-6.2-sol and claude-opus-5-5.
Both found it unfit to implement. This revision answers every blocking finding.

This page is the interface the `central-6ly1.*` Beads implement. Test scripts in other
repositories will depend on it, so change a name, status, route or exit code only through the
coordination thread. `docs/headless-player.md` documents what has shipped. This page documents
what is decided and not yet built, and each part is trimmed from here as it lands.

Everything extends `/v2/player`. Existing routes, operations, fields and statuses remain unless
the table below lists them.

## Behaviour that changes from client d034b7e

The Rust server validates against client `d034b7e`. Scripts written now must tolerate both columns
until coordination moves that pin. Coordination approved items 1 to 10 on 2026-11-27. Items 11
onward are new in revision 2 and need the same approval.

| # | Item | d034b7e | After |
| --- | --- | --- | --- |
| 1 | Stop on the wire | Resting position sent up to 1.3 s late, carrying the run's delta and animation | One zero-delta, animation-0 update on the first physics step at rest. Shipped in fea4596 |
| 2 | `state_changed` events | Published about every 100 ms; long-polls return early | Not published |
| 3 | Terminal receipts | A readiness drop rewrites `applied_local` to `unknown`; `unknown` can later become `confirmed` or `refused` | Never change (§2) |
| 4 | `target`, `attack` receipts | `unknown` with `processed_without_server_confirmation` | `sent`, then `confirmed`, `refused` or `unknown` (§2.3) |
| 5 | `reason` | Codes, except validation failures, which are prose | Always a code; prose moves to `detail` |
| 6 | Refused at admission | Takes a journal capacity slot | Takes no capacity slot. The ID is still never reusable (§3.1) |
| 7 | Journal capacity | 4096 entries for the process lifetime, then 503 | §3.1 |
| 8 | `nearby` order | Spawn ID | Distance from the published position, ties broken by spawn ID |
| 9 | `camp` while dead | Refused `not_ready` | Admitted (§5.2) |
| 10 | Process exit | Stays `failed` until signalled, then exits 0 | §8 |
| 11 | `sent` | Never produced | Produced for every operation that puts a packet on the wire |
| 12 | `busy` | Any non-priority command is refused while another is `accepted` or `sent` | Only the conflicts in §2.4 |
| 13 | Range checks | Measured from `gs.player_x/y/z`, the net mirror | Measured from the published `self.position` (§4) |
| 14 | Event buffer | 2048 events | 32 768 events; the event wrapper gains `at_ms` |
| 15 | `target` on the wire (headless only) | `OP_TargetMouse` | `OP_TargetCommand` (§2.3). **Wire change** |
| 16 | API after a failed session | Readable until signalled | The process exits (§8). The exit line carries the failure |
| 17 | Profile error exit code | 1 | 2 |
| 18 | Expired command ID | Not possible | 410 `command_id_expired` (§3.1) |

The `motion_expired` and `motion_stopped` events remain alongside `completion`.

## 1. Clock

State carries `clock_ms`, the host's monotonic clock in milliseconds. It starts just after the
control port is bound (shipped). Every timestamp the API adds is a field ending `_at_ms` or named
`at_ms`, on this clock. Wall-clock time never appears.

## 2. Receipts (central-6ly1.7)

### 2.1 Statuses

| Status | Terminal | Meaning |
| --- | --- | --- |
| `accepted` | no | Admitted, not yet dispatched |
| `sent` | no | The network owner's drain of this command handed its packets to the transport. `evidence.packets` lists their trace `n` values (§6). This proves the client attempted the send; it does not prove the socket write succeeded or that the server received it |
| `applied_local` | yes | A local-only operation took effect in the controller |
| `confirmed` | yes | A server packet attributable to this command arrived. `evidence.kind` names it and `evidence.packets` lists its trace `n` |
| `refused` | yes | Not executed: refused at admission, at dispatch, by a drain-time check, or by an explicit server rejection. `reason` says which |
| `unknown` | yes | Dispatched, but no attributable evidence arrived within the operation's window. Also used when a fence interrupted the command after dispatch |

Receipt fields gain `terminal` (bool), `accepted_at_ms`, `updated_at_ms`, `detail` (optional
prose) and `completion` (§2.5).

### 2.2 Rules

- **A terminal receipt never changes.** Its `status`, `reason` and `evidence` are fixed once
  written. The only later write is `completion`, which goes from absent to present exactly once.
- **Fences.** `not_ready` and `zone_epoch_changed` act only on non-terminal receipts. An `accepted`
  receipt that was never dispatched becomes `refused`. A dispatched `accepted` or a `sent` receipt
  becomes `unknown`.
  - Fence-spanning operations are exempt: `respawn`, `cross_zone` and `camp`. They stay `sent`
    through the zone change and readiness drop they cause, then settle by their own rules in §2.3.
- **Windows start at `sent`.** If a command is not dispatched within 2 s of acceptance, it becomes
  `refused` with `dispatch_timeout`.
- **Reasons.** `reason` is always snake_case. Admission reasons:
  - `stale_session`
  - `stale_zone_epoch`
  - `invalid_operation`, with prose in `detail`
  - `not_ready`
  - `dead`
  - `busy_transaction`
  - `busy_same_kind`
  - `lifecycle_pending`

  Each operation adds its own reasons in §2.3.
- **Correlation.** The `ActionTracker` reports a drained request as `drained` without finishing it.
  The host keeps the action-to-command mapping until the receipt is terminal. The mapping is bounded
  by the journal (§3.1) and by a hard cap of 4096 entries; past the cap, the oldest mapping's
  command becomes `unknown` with `tracking_lost`.

### 2.3 Per-operation table

"Window" is measured from `sent`. When it lapses with no evidence, the receipt becomes `unknown`
with reason `no_server_evidence_within_window`. "None" in the evidence column means the protocol
provides no correlated reply: the receipt goes straight from `sent` to `unknown` with reason
`no_server_receipt_in_protocol`.

| Operation | Path | Confirming evidence (`evidence.kind`) | Window | Operation-specific refusals | Completion |
| --- | --- | --- | --- | --- | --- |
| `move_for` | `applied_local` | `controller_intent` | – | `stand_before_moving` | `expired`, `stopped`, `superseded`, `interrupted` |
| `goto` | `applied_local` | `controller_goal` | – | `stand_before_moving`, `invalid_operation` | §5.1 |
| `stop` | `applied_local` | `controller_stopped` | – | – | – |
| `face` | `applied_local` | `controller_heading` | – | `invalid_operation`, `entity_not_received` | `sent` or `interrupted` (§5.1) |
| `jump` | `applied_local` | `controller_jump` | – | `not_grounded`, `in_water` | – |
| `target` | `sent` → `confirmed` / `refused` | `OP_TargetCommand` echo naming the entity (`server_target_echo`); `OP_TargetReject` becomes `refused` with `target_rejected` | 3 s | `entity_not_received` | – |
| `attack` | `sent` → `confirmed` | The first received `OP_Damage` with source = self, target = the target at send time and no spell id, of any damage value, including 0 and the `DMG_*` sentinels (`server_melee_swing`) | The lease | `no_live_npc_target_in_range` | – |
| `attack_off` | `applied_local` | `attack_off_queued` | – | – | – |
| `sit`, `stand` | `sent` → `confirmed` | Self `OP_SpawnAppearance` with the matching animation (`server_appearance`) | 3 s | – | – |
| `say`, `tell`, `chat` | `sent` → `confirmed` | The server's self-echo of the same channel and text (`received_self_echo`). This does not prove delivery to anyone else | 3 s | `invalid_operation` | – |
| `hail` | `sent` → `confirmed` | A received NPC say from the hailed entity's name, unique within 200 units (`npc_reply`). Two received entities with that name give `unknown` with `ambiguous_name`. An NPC with no hail script gives `unknown` | 3 s | `entity_not_received` | – |
| `consider` | `sent` → `confirmed` | `OP_Consider` reply naming the entity (`server_consider`). `evidence` carries faction and level colours | 3 s | `entity_not_received` | – |
| `cast` | `sent` → `confirmed` / `unknown` | The awaited cast path's `CastEnd` (`cast_end`, with `evidence.outcome` one of `completed`, `fizzled`, `interrupted`) | 3 s for `OP_BeginCast`, then its cast time plus 3 s | `no_spell_in_gem`, `invalid_operation` | – |
| `memorize` | `sent` → `confirmed` | `OP_MemorizeSpell` echo for the same gem and spell (`server_memorize`) | 5 s | `invalid_operation` | – |
| `door` | `sent` → `confirmed` | `OP_MoveDoor` for that door ID (`server_door`, `evidence.attribution: "door_broadcast"`, because another player could cause the same packet). A locked door sends none, which ends `unknown` | 3 s | `door_not_received` | – |
| `loot` | `sent` → `confirmed` | The existing corpse-open handshake (`corpse_opened`) | 5 s | as today | – |
| `merchant_open`, `buy` | `sent` → `confirmed` / `refused` | As today: the server echo, or `OP_ShopEndConfirm` refusal | 8 s | as today | – |
| `sell` | `sent` → `confirmed` | `OP_ShopPlayerSell` echo for the slot (`server_sell`) | 8 s | as today | – |
| `inventory_move` | `sent` → `confirmed` | Inbound `OP_MoveItem` matching from and to (`server_move_item`) | 5 s | as today | – |
| `merchant_close` | `sent` → `unknown` | None | – | – | – |
| `give` | `sent` → `confirmed` / `unknown` | The awaited give path. `evidence.result` is `accepted`, or `returned` when the item comes back after `OP_FinishTrade` | 8 s | `inventory_revision_required_or_stale`, `entity_not_received` | – |
| `respawn` | `sent` → `confirmed` | The next zone entry while it is pending (`zoned`, carrying `zone_epoch`) | 30 s | `not_dead` | – |
| `cross_zone` | `sent` → `confirmed` / `refused` | `zoned` for a cross-zone line. For a same-zone translocator, the in-zone reposition (`same_zone_reposition`). An `OP_ZoneChange` echo with `success <= 0` becomes `refused` with `zone_change_rejected` | 60 s | `not_in_zone_line`, `region_data_unavailable` | – |
| `camp` | `sent` → `confirmed` | Session end after the camp timer (`camp_complete`), seen in the EXIT line rather than in a later read | 40 s | – | – |

Notes on the table:

- **`target`, wire change (headless only).** Native EQEmu's `Handle_OP_TargetMouse`
  (`eqemu:zone/client_packet.cpp@cd66dd0a6:15125`), the opcode the client sends today, replies with
  nothing attributable. `Handle_OP_TargetCommand` (`eqemu:zone/client_packet.cpp@cd66dd0a6:15085`) echoes the packet on success
  and sends `OP_TargetReject` on failure. Headless `target` therefore sends `OP_TargetCommand`; the
  graphical client keeps `OP_TargetMouse`.
  - The client's inbound `OP_TargetCommand` handler treats every packet as a forced target. It is
    taught that an echo naming a pending headless target is that request's confirmation.
  - The Rust server must answer `OP_TargetCommand` the same way. Coordination owns that check.
- **`attack`.** Native `Handle_OP_AutoAttack` (`eqemu:zone/client_packet.cpp@cd66dd0a6:3517`) sends no reply, and Bead .7's
  "server toggle" does not exist. The first attributed swing is the only evidence.
  - A new `attack` while one is `sent` replaces it. The old receipt becomes `unknown` with
    `superseded_by_renewal`.
  - `attack_off`, a retarget, a lease expiry or a fence ends a pending attack `unknown` with that
    reason.
  - A swing whose target is not the target at send time never confirms.

### 2.4 Conflicts (replaces the global `busy`)

| Class | Rule |
| --- | --- |
| Transactions: `buy`, `sell`, `inventory_move`, `give` | One non-terminal at a time (`busy_transaction`). An `unknown` outcome blocks further transactions until relog, as today |
| Correlated by kind: `consider`, `hail`, `door`, `memorize`, `cast` | One non-terminal per kind (`busy_same_kind`) |
| Lifecycle: `camp`, `respawn`, `cross_zone` | While one is non-terminal, every other ordinary command is refused with `lifecycle_pending`. `stop` and `attack_off` are still admitted |
| Everything else | No conflict |

A pending merchant open or buy no longer blocks movement, combat or chat.

### 2.5 Completion

`completion` is `{"result", "at_ms", ...}`, written once. A `command_completed` event carries the
same object together with the command ID. A receipt that still owes a completion cannot be evicted
(§3.1).

## 3. Journal and events (central-6ly1.1)

### 3.1 Command identity

- Command IDs are session-scoped and never reusable, including IDs refused at admission.
- Every admitted or refused ID is recorded as an identity: a digest of the ID, a digest of the
  payload, the final status and the reason. Up to 1 048 576 identities are kept per session. Past
  that, every PUT gets 503 `session_id_space_exhausted`, and the script starts a new process.
- Full receipts are kept for the newest 65 536 entries that are terminal and owe no completion.
  - A GET or PUT of an older ID returns 410 `command_id_expired` with
    `{"status","reason","payload_matches"}`. The command never runs again.
  - Non-terminal entries and entries that owe a completion are never evicted. The conflict rules
    keep their number small.
- Refused at admission: 409 with the receipt and `"admitted": false`. The refusal takes no capacity
  slot. A retry of the same ID returns the same refusal, so mint a new ID to try again.
- A changed payload under any known ID returns 409 with
  `{"error":"command_id_reused_with_changed_payload"}`.
- `stop` and `attack_off` are always admitted and executed; both are idempotent. The reserved-slot
  mechanism goes.

### 3.2 Events

- `state_changed` is gone. An idle long-poll waits its full `wait`.
- The wrapper becomes `{"cursor","at_ms","event"}`.
- The buffer holds 32 768 events.
  - Supported load: 5 commands per second at up to 4 events each, plus 20 combat events per second.
  - At that load the buffer covers more than 8 minutes. A reader that lags by at most 120 s loses
    nothing.
  - Receipts can always be re-read with GET, so a gap is recoverable: re-read state, then every
    outstanding command.
- Cursor rules (unchanged, now stated):
  - `after` is exclusive, and the cursor is `SESSION:N`.
  - A cursor older than the oldest buffered event minus one returns the newer events with
    `gap: true`.
  - A future cursor returns 400.
  - Another session's cursor returns 409.
  - A timeout returns `{"events": [], "gap": false}` with an unchanged cursor.

## 4. State additions (central-6ly1.8)

Under `self`:

- `server_position`: `{"position","heading","source","at_ms"}`, or null.
  - It is recorded at the packet boundary from the last self `OP_ClientUpdate` the server sent.
  - `source` is `server_update` within 12 units of the client's position, otherwise
    `server_correction`.
  - Feet datum, with heading in the state convention below. Cleared at zone entry.
- `corrections`: `{"count","last":{"from","to","at_ms"}|null}`. It counts server assertions more
  than 12 units from the client position. `last` is cleared at zone entry.
- `level`.
- `xp`, `mana` and `endurance`, each `{"value","max","known"}`.
- `buffs`: `[{"slot","spell_id","ticks_remaining"}]`. `skills`: `{"<id>": value}`.
- `dead` (bool), and `killer`: `{"id","name"}`. Either field may be null, and `id` is kept when
  the name is unknown.
- `last_position_send` (shipped) and `clock_ms` (shipped).

Heading convention, for state and `face`: degrees counter-clockwise from north, so 90 is west and
270 is east.

Position and range:

- `self.position` is the controller's predicted position, the value the client streams.
- Every host range check uses it, with the same 3-D distance as `nearby`.
- `nearby` is sorted by that distance, ties broken by spawn ID, and each entity gains `distance`,
  `heading` and `equipment` (the nine material slots as received).

Zone:

- `zone.lines`: `[{"index","destination":{"zone_id","position"}}]` from `OP_SendZonepoints`.
  These are **destinations**, not trigger positions.
- `zone.line_at_position`: the index of the local zone-line region the body stands in, or null.
  When the region map is unavailable it is `"unavailable"`.
- `doors`: `[{"id","name","position","open"}]` for received doors within the roster radius.

Event `combat`:

`{"kind":"combat","source":"received_packet","source_id","target_id","source_name","target_name","amount","spell_id","skill","result"}`.

- `result` is one of: `hit`, `miss`, `block`, `parry`, `riposte`, `dodge`, `invulnerable`, `rune`,
  `landed`, `other`.
- `amount` is the damage when `result` is `hit`, and 0 otherwise.
- `spell_id` is null for melee.
- `landed` means a spell action with no positive damage.

The existing text event stays. Event `died`: `{"kind":"died","killer":{"id","name"}}`.

## 5. New operations

### 5.1 Movement (central-6ly1.4)

- **`goto`.** Fields: `x`, `y`, optional `z`, `tolerance` 1.0–50 (default 2), `z_tolerance` 1–50
  (default 8, used only when `z` is given), and `timeout_ms` 1–120 000 (required).
  - Steering: a straight line with local collision, and no pathfinding. On the step that would
    overshoot, the host scales the wish so the body ends on the goal.
  - Each step evaluates, in this order:
    1. `arrived`: the 2-D distance is at most `tolerance`, and `|dz|` is at most `z_tolerance` when `z` is given.
    2. `blocked`: the distance to the goal fell by less than 0.5 units over the last 1000 ms.
    3. `timeout`.
  - Completion carries `position`, `distance` and `dz`. Other results:
    - `stopped`;
    - `superseded`, by another `goto` or `move_for`;
    - `interrupted`, with `reason`: `sit`, `dead`, `zone_entry`, `not_ready` or `camp`.
- **`face`.** Exactly one of `heading` or `entity_id`. The body turns without displacement.
  - The host flags an immediate heading update, which bypasses the 280 ms rest spacing.
  - Completion `sent` carries the `sequence` and trace `n` of the `OP_ClientUpdate` that carried the
    heading.
  - While a correction is unadopted, the update waits for adoption. A fence gives `interrupted`.
  - The protocol acknowledges no heading. Bead .4's "the server receives it" is verified in the live
    acceptance test from the server's log or a second, observing client, not from the receipt.
- **`jump`.** Uses the existing controller jump. It needs `on_ground` and not `in_water`.

### 5.2 Lifecycle (central-6ly1.5, .6)

- **While dead:**
  - Admitted: `stop`, `attack_off`, `camp`, `respawn`.
  - Everything else is refused with `dead`.
  - `respawn` is refused with `not_dead` while alive.
- **`respawn`.**
  - The network owner holds an epoch-bound request with the command ID and a 30 s deadline. It
    retries the bind selection every 5 s until the deadline, then stops retrying.
  - The next zone entry confirms it, including one the server starts on its own.
  - `camp` while a respawn is pending is refused with `lifecycle_pending`.
- **`cross_zone`.** "Cross the line I stand in."
  - Refusals: `not_in_zone_line` when `zone.line_at_position` is null, and
    `region_data_unavailable` when it is unavailable.
  - The network owner sends the normal `OP_ZoneChange` with zone ID 0, as the automatic path did;
    the server resolves the line from the position. It does so with an epoch-bound request and
    deadline, separate from the disabled automatic crossing.
  - No field steers the destination.
- **Event `zoned`.** `{"kind":"zoned","zone":{"id","name"},"zone_epoch","command_id"}`.
  - It is published once per zone entry, after the host is Ready in the destination and in the
    same publication as the new state.
  - The matching receipt is confirmed before the event is written, so a waiter can act at once.
  - `command_id` is null for an entry no command asked for.
- **Event `zone_failed`.** `{"kind":"zone_failed","reason","command_id"}`, with `reason` one of
  `zone_change_rejected`, `connect_failed`, `entry_timeout`.

### 5.3 Interaction (central-6ly1.9)

- `hail`: optional `entity_id`; the default is the current target.
- `consider`: `entity_id`.
- `cast`: `gem` 0–8 and optional `target_id`. The gem range matches the nine gems the RoF2 profile
  decoder supports.
- `memorize`: `gem` 0–8 and `spell_id`.
- `door`: `door_id` from `doors`.
- `give`: `entity_id` and inventory `slot`. It needs the current `inventory_revision`, and the item
  must still be in that slot at dispatch.
- `chat`: `channel` is one of `group`, `guild`, `shout`, `ooc`, `auction`. `text` is 1–512 bytes
  with no control characters. `say` and `tell` stay as they are.
- `hail`, `consider` and `cast` with an `entity_id` or `target_id` retarget as a side effect. The
  target change has no receipt of its own.

## 6. Packet trace (central-6ly1.2)

This section follows the implementation under review in PR #4.

`GET /v2/player/packets?after=N&limit=1..2000&opcode=NAME&command=ID` returns
`{"session","packets":[...],"next","gap","counters":{...}}`.

- Capture is always on for headless sessions. It covers only the zone stream: one per-session ring
  of 20 000 packets, stamped on the host clock. The graphical `/v1/observe/packets` is unchanged.
- Each row: `{"n","at_ms","dir","opcode","opcode_name","len","reliable","wire_seq","command_id","fields"}`.
- `after` is exclusive. `next` is the highest `n` scanned, not the highest matched, so an empty
  filtered page still advances. `gap` is computed before filtering.
- A new `session` means a new process, and the client restarts from 0.
- Attribution:
  - An outbound packet carries `command_id` only when that command's own drain produced it.
  - Deferred follow-ups, such as loot pipeline takes, carry null.
  - Retransmits do not add rows.
  - Inbound packets carry null. The link from reply to command is the receipt's `evidence.packets`,
    which lists the confirming inbound `n`.
- `fields` is decoded for `OP_ClientUpdate` (position, delta, animation, heading), `OP_TargetCommand`
  (both directions), `OP_TargetMouse` and `OP_Damage`. Other opcodes give `{}`. Any further fields
  are unstable.
- Counters, each defined against the transport:
  - `sent`: app packets handed to the transport.
  - `resent`: datagrams that went to the socket a second time.
  - `received`: inbound app packets.
  - `dropped_unretried`: datagrams lost with no retry, including those abandoned at teardown.
  - `sequence_gaps`: newly opened holes in the inbound reliable sequence.
  - The `NetHealth` counters are sampled separately from the page.
- At a few hundred inbound NPC updates per second, 20 000 rows cover a few minutes. Read promptly,
  or filter by `command`.

## 7. Wait (central-6ly1.10)

`POST /v2/player/wait` with `{"timeout_ms": 1..30000, "until": <node>}`.

A node is exactly one of:

- `{"command":"ID","status":[...]}`, `{"command":"ID","terminal":true}` or
  `{"command":"ID","completed":true}`.
- `{"event":{"kind":"...", "<field>": <value>, ...},"after":"SESSION:N"}`. Equality is on the named
  top-level event fields. When `after` is absent, only events after the call count.
- `{"state":{"pointer":"/json/pointer", "<op>": <value>}}`, with `op` one of:
  - `eq`, `ne`, `lt`, `le`, `gt`, `ge`;
  - `exists` (bool);
  - `within`: `{"point":[x,y]|[x,y,z],"radius"}`, 2-D when `point` has two values.
- `{"all":[nodes]}` or `{"any":[nodes]}`, with at most 16 nodes and depth 4.

Evaluation:

- Every check reads one consistent snapshot of state, journal and event buffer under the journal
  lock. It runs on each state publication and each event, until timeout.
- A missing pointer or a type mismatch makes that comparison false, except `exists`.
- **Impossible**, which answers at once:
  - a `command` that is unknown or expired;
  - a `command` already terminal with a status outside `status`;
  - `completed` on an operation that has no completion;
  - an `event` node whose `after` is from another session.
- **Gap**: an `event` node whose `after` is older than the buffer.

Response:
`{"result":"satisfied"|"timeout"|"impossible"|"gap","at_ms","state":{...},"matches":[{"path","command"|"event"}],"detail"}`.

- `matches` lists every satisfied leaf with its path, for example `"/all/1"`.
- The HTTP status is 200 for every result.
- An invalid node is 400 `invalid_predicate` with `detail`.

## 8. Process contract (central-6ly1.3)

- `api_port: 0` binds an ephemeral port. Every port printed is the bound one.
- stdout carries only these lines. Write errors, including EPIPE, are ignored. Logs go to stderr,
  and `RUST_LOG` is honoured.
  - `API_PORT=<port>`, at bind (kept for compatibility).
  - `EQOXIDE_READY {"api_port","session","zone_epoch"}`, printed once, the first time the host is
    Ready and admission is open.
  - `EQOXIDE_EXIT {"status","reason","clock_ms"}`, printed once, as the last line.
- Readiness timeout: if the host is never Ready within `ready_timeout_ms` (profile, default
  120 000), it logs out and exits `never_ready`.
- Signals: SIGINT, SIGTERM and SIGHUP all request normal logout. A second signal during logout
  exits at once with `signal_forced`. SIGTERM is the fast, ordinary logout for disposable fixtures.
- Exit status:

| `status` | Code | When |
| --- | --- | --- |
| `camped` | 0 | The camp timer completed and logout was sent |
| `signal_logout` | 0 | A signal requested logout and it was sent. The JSON line distinguishes it from `camped` |
| `internal_error` | 1 | An unexpected host error |
| `network_panic` | 1 | The network owner panicked. A logout was attempted if a stream remained |
| `profile_error` | 2 | Invalid profile or arguments |
| `bind_failed` | 3 | The control port could not be bound |
| `login_failed` | 4 | Login, world selection, world mismatch or character creation failed. `reason` says which |
| `session_lost` | 5 | The server dropped the session, or the link timed out, after Ready |
| `logout_incomplete` | 6 | The logout deadline (8 s) passed |
| `signal_forced` | 6 | A second signal arrived during logout |
| `never_ready` | 7 | See the readiness timeout above |

- The network owner reports a typed termination cause, with this precedence: `network_panic`,
  `login_failed`, `camped` (if the camp timer completed), `signal_logout`, `session_lost`.
- "Logout sent" means `OP_Logout` and the session disconnect were handed to the transport. The
  server sends no acknowledgement the client can wait for.
- Supervisors: a handled SIGTERM exits 0, so systemd needs no `SuccessExitStatus`.

## 9. Host robustness (central-6ly1.11)

- No `lock().unwrap()` on state shared with the network thread. A poisoned lock fails the
  session: fence, then attempt logout if the network owner still holds a stream, then exit
  `network_panic` or `internal_error`.
- Geometry loads off the host loop (a blocking task keyed by zone epoch). The host stays not Ready
  until the load for the current epoch finishes, and a result for an older epoch is discarded.
- Movement deadlines (`move_for`, `goto`, attack lease) count accumulated physics time, not wall
  time, so a stalled loop cannot shorten them. Each step stays clamped to 50 ms. State gains
  `freshness.host_stall_ms`, the longest step gap in the last 10 s.
- The action-to-command map is bounded as in §2.2.

## 10. Client CLI (central-6ly1.10)

The `eqoxide-headless-ctl` binary, in the headless crate. Every subcommand prints one JSON object:

- `launch --login HOST:PORT --world-port N --account FILE --character NAME --maps DIR [--expected-world NAME@HOST] [--create JSON]`.
  - It writes a mode-0600 profile with a fresh token into a private directory.
  - It starts `eqoxide-headless` with `api_port: 0` and waits for `EQOXIDE_READY`.
  - It prints `{"pid","api_port","session","zone_epoch","control_file"}`.
  - `control_file` (mode 0600) holds the port and token for the other subcommands.
- `command --control FILE KIND [JSON]`: fills `session` and `zone_epoch` from state and mints an ID
  (`<session-prefix>-<counter>`).
  - A transport error retries the same ID up to 3 times.
  - An uncertain transaction is never retried under a new ID.
  - It prints the receipt.
- `wait --control FILE PREDICATE_JSON [--timeout-ms N]`: §7.
- `state`, `events --after CURSOR`, and `packets ...`.
- `stop --control FILE`: SIGTERM, then it waits for `EQOXIDE_EXIT` and prints it.

Relog is a new process and a new session. In-process character switching stays out of scope.

## Wire impact

| Section | Change |
| --- | --- |
| §1 | Shipped stop flush (fea4596) |
| §2.3 | Headless `target` sends `OP_TargetCommand`, a headless wire change |
| §5 | New operations send their existing packets only when invoked |
| §3, §4, §6, §7, §8, §9, §10 | No packet changes |

Every Bead's PR states whether it changes the wire.
