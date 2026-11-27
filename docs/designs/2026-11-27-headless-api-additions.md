# Headless player API additions (central-6ly1)

Status: decided 2026-11-27. This is the interface the `central-6ly1.*` Beads implement. Test
scripts in other repositories will depend on it. Any change to a name, status or route here goes
through the coordination thread first. `docs/headless-player.md` documents what has shipped. This
page documents what is decided and not yet built, and it is trimmed as each part lands.

Everything here extends `/v2/player`. Existing routes, operations, fields and statuses remain
unless a section says **changed**.

## Behaviour that changes from client d034b7e

The Rust server validates against client `d034b7e`. Scripts written now must tolerate both columns
until coordination moves that pin. Coordination approved every row on 2026-11-27.

| Item | d034b7e | After |
| --- | --- | --- |
| Stop on the wire | Resting position sent at the next keepalive, up to 1.3 s late, with the run's delta and animation; first zero-delta update about 1.3 s after that | One update with zero delta and animation 0 on the first physics step at rest (central-vgwu) |
| `state_changed` events | One per state publication, about every 100 ms; long-polls return early | Not published |
| Terminal receipts | Readiness drop rewrites `applied_local` to `unknown`; `unknown` may become `confirmed`/`refused` | Never change |
| `target`, `attack` receipts | `unknown` with `processed_without_server_confirmation` | `sent`, then `confirmed` or `unknown` |
| `reason` | Codes, except validation failures, which are prose | Always a code; prose in `detail` |
| Refused at admission | Takes a journal slot; ID cannot be reused | No slot; ID can be reused; body has `journaled:false` |
| Journal capacity | 4096 for the process lifetime, then 503 | Section 3 |
| `nearby` order | Spawn ID | Distance |
| `camp` while dead | Refused (`not_ready`) | Admitted |
| Process exit | Stays `failed` until signalled, then exits 0 | Section 8 |

## 1. Clock

State carries `clock_ms`, the host's monotonic clock in milliseconds since process start (shipped
with central-vgwu). Every timestamp the API adds is a `*_at_ms` field on this clock. Wall-clock time
never appears.

## 2. Receipts (central-6ly1.7)

The status names stay the same: `accepted`, `sent`, `applied_local`, `confirmed`, `refused`,
`unknown`.

| Status | Terminal | Meaning |
| --- | --- | --- |
| `accepted` | no | Admitted, not yet dispatched |
| `sent` | no | The network owner drained the request and wrote its packet; server evidence is awaited |
| `applied_local` | yes | A local-only operation took effect in the controller (`move_for`, `goto`, `stop`, `face`, `jump`) |
| `confirmed` | yes | A server packet attributed to this command arrived; `evidence.kind` names it |
| `refused` | yes | Not executed; `reason` says why |
| `unknown` | yes | Dispatched but no attributable server evidence came within the operation's window, or a fence interrupted it after dispatch |

Rules:

- **A terminal receipt never changes.** Its `status`, `reason` and `evidence` are fixed once written.
  The single exception is `completion` (below), which goes from absent to present exactly once.
  **Changed:** today a readiness drop rewrites past `applied_local` receipts to `unknown`, and
  `unknown` can later become `confirmed` or `refused`. Both stop.
- Fences (`not_ready`, `zone_epoch_changed`) affect only non-terminal receipts. `accepted` that was
  never dispatched becomes `refused`. `accepted` after dispatch, and `sent`, become `unknown`.
- `sent` comes from the network drain (`ActionTracker` reports the drained request as processed).
  It is no longer collapsed into `unknown`/`processed_without_server_confirmation`.
- Each operation that can be confirmed has an evidence window, after which `sent` becomes
  `unknown` with reason `no_server_evidence_within_window`. Defaults: target 3 s, attack 6 s,
  consider 3 s, hail and say 3 s, door 3 s, give 8 s, cast = cast time + 3 s, respawn 30 s,
  zone crossing 60 s.
- Confirmation sources:
  - `target`: the server's target packet naming this entity.
  - `attack`: the first received melee swing whose source is self and target is the current
    target, or the server's auto-attack toggle, whichever comes first.
  - Merchant and buy: the server echoes they already use.
  - Ops in sections 5 and 6: each says what confirms it.
- **Reason codes. Changed:** `reason` on `refused` and `unknown` is always a snake_case code.
  Validation messages that were prose move to a new `detail` field, and their `reason` becomes
  `invalid_operation`. The existing codes keep their spellings.
- Receipts gain `terminal` (bool), `accepted_at_ms` and `updated_at_ms`.
- `completion` is added for long-running local operations. It is written once with
  `{"result": ..., "at_ms": ..., ...}`. A `command_completed` event carries the same object.
  Results:
  - `move_for`: `expired`, `stopped`, `superseded`, `interrupted`.
  - `goto`: `arrived`, `blocked`, `timeout`, `stopped`, `superseded`, `interrupted`.
  - `face`: `sent`, carrying the `sequence` of the position update that sent the heading.
  - `interrupted` carries the fence or halt reason.
- A pending merchant or buy request no longer makes other commands `busy`. Only transactions
  serialise against each other: buy, sell, inventory move and give.
- **Changed:** a command refused at admission takes no journal slot and keeps no identity. A later
  PUT with the same ID is evaluated afresh, because nothing ran. The 409 body is the receipt, with
  `"journaled": false`. A changed payload under a *journaled* ID is still 409 with
  `{"error":"command_id_reused_with_changed_payload"}`. Scripts tell the two apart by the body.

## 3. Events and journal (central-6ly1.1)

- **Changed:** `state_changed` is no longer published. State is read with `GET /v2/player/state`
  or the wait endpoint (section 7). An idle long-poll waits its full `wait`.
- Every event gains `at_ms`. The event buffer bound is 8192 events. A cursor older than the buffer
  still reports `gap:true`.
- Journal: identities are kept as `{identity digest, receipt}`. Up to 65 536 are retained, and the
  oldest terminal entries are evicted first. Non-terminal entries are never evicted. A GET or
  identical retry of an evicted ID returns 410 `command_id_expired` while the ID is still in the
  tombstone ring of the same size. After that, the ID is unknown and a PUT would run it again.
  Retries are for lost responses, so scripts must not retry after minutes.
- `stop` and `attack_off` are always admitted, at any journal fill. The reserved-slot mechanism goes.

## 4. State additions (central-6ly1.8)

New fields under `self`:

- `server_position`:
  `{"position":[x,y,z],"heading":h,"source":"server_spawn"|"server_correction"|"server_update","at_ms":n}`.
  This is the last position the server asserted for self. The existing `position` stays the
  local prediction. Range checks in the host use `position`, the same value that is published, so
  the separate `gs.player_x` copy stops deciding ranges.
- `corrections`: `{"count":n,"last":{"from":[..],"to":[..],"at_ms":n}|null}`.
- `level`, `xp` (`{"value","known"}`), `mana`, `endurance` (each `{"value","max","known"}`),
  `buffs` (`[{"slot","spell_id","ticks_remaining"}]`), `skills` (`{"<id>": value}`), `dead` (bool).

Entity changes:

- Entities in `nearby.entities` gain `heading` and `equipment` (as received).
- The roster cap sorts by distance and includes `distance`. **Changed** from sorted by spawn ID;
  `limit` and `truncated` keep their meaning.

New state needed by the section 5 operations:

- `zone.points`: `[{"index","zone_id","position"}]`, the zone points the server advertised.
- `doors`: `[{"id","name","position","state"}]`, received doors within the roster radius.

New event `combat`:
`{"kind":"combat","source":"received_packet","source_id","source_name","target_id","target_name","amount","damage_type","skill","spell_id","result":"hit"|"miss"|"dodge"|"parry"|"riposte"|"block"|"invulnerable"|"other"}`.
Unknown fields are null. The existing flattened text event remains.

New event `died`: `{"kind":"died","killer_id","killer_name"}`, with nulls when the killer is
unknown (central-6ly1.5).

## 5. New operations

All operations need `session` and `zone_epoch` like today.

| Kind | Fields | Receipt | Bead |
| --- | --- | --- | --- |
| `goto` | `x`, `y`, `z?`, `tolerance` 0.5–50 (default 2), `timeout_ms` 1–120000 | `applied_local`, then `completion` | .4 |
| `face` | exactly one of `heading` (degrees, state convention) or `entity_id` | `applied_local`, `completion.result:"sent"` | .4 |
| `jump` | none | `applied_local` | .4 |
| `respawn` | none; only while dead | `sent` → `confirmed` on the respawn zone entry | .5 |
| `cross_zone` | `zone_point?` (index from state `zone.points`); default is the zone line at the current position | `sent` → `confirmed` by `zoned` | .6 |
| `hail` | `entity_id?` (default current target) | `sent` → `confirmed` by the received NPC reply naming it, else `unknown` | .9 |
| `consider` | `entity_id` | `confirmed` by the consider reply; evidence carries faction and level colours | .9 |
| `cast` | `gem` 0–11, `target_id?` | `confirmed` by cast end (`evidence.result`: landed/interrupted/fizzled) | .9 |
| `memorize` | `gem`, `spell_id` | `confirmed` by the server mem echo | .9 |
| `door` | `door_id` from state `doors` | `confirmed` by the door state packet | .9 |
| `give` | `entity_id`, `slot` | transaction; `confirmed` by the trade completion echo | .9 |
| `say`, `tell` | as today, plus `channel` for `group`, `guild`, `shout`, `ooc`, `auction` in a new `chat` kind | unchanged | .9 |

Further rules:

- `goto` uses straight-line steering with local collision, and no pathfinding.
  - `arrived`: the 2-D distance is at most `tolerance`. The completion carries `position`, `distance` and `dz`.
  - `blocked`: less than 0.5 units of progress over 1 s.
  - `timeout`: `timeout_ms` passed.
  - A new `goto` or `move_for` supersedes a running one, and `stop` stops it.
- `face` turns without displacement. The rest flush sends the heading on the next physics step.
- **Changed:** `camp` is admitted while dead. `respawn` is refused unless dead.
- New event `zoned`: `{"kind":"zoned","zone":{"id","name"},"zone_epoch":n,"ready":true}`. It is
  published once per zone entry, when the host is Ready in the destination. A failed entry
  publishes `{"kind":"zone_failed","reason"}`.

## 6. Packet trace (central-6ly1.2)

`GET /v2/player/packets?after=N&limit=500&opcode=OP_Name&command=ID` returns
`{"packets":[...],"next":N,"gap":bool,"counters":{...}}`.

- Each packet: `{"n","at_ms","dir":"in"|"out","opcode","opcode_name","len","reliable","wire_seq","command_id","fields"}`.
  `fields` holds decoded key fields where a decoder exists. `command_id` is the headless command
  whose dispatch produced an outbound packet.
- `counters`: `sent`, `resent`, `received`, `dropped_unretried`, `sequence_gaps`.
- The buffer is bounded per session at 20 000 packets. It is source-backed by `eqoxide-telemetry`.

## 7. Wait (central-6ly1.10)

`POST /v2/player/wait` takes `{"timeout_ms": 1..30000, "until": <predicate>}`.

Predicates (exactly one key):

- `{"command":"ID","terminal":true}` or `{"command":"ID","status":["confirmed"]}` or
  `{"command":"ID","completed":true}`.
- `{"event":{"kind":"zoned", ...field equality...},"after":"SESSION:CURSOR"}`.
- `{"state":{"pointer":"/phase","eq":"ready"}}`. Comparators: `eq`, `ne`, `lt`, `gt`,
  `within:{"point":[x,y,z],"radius":r}` (2-D unless `point` has three values).
- `{"all":[...]}` and `{"any":[...]}` combine predicates.

The response is `{"satisfied":bool,"at_ms":n,"state":{...},"event":{...}|null,"command":{...}|null}`.
A wait that times out returns 200 with `satisfied:false`. An invalid predicate is 400.

## 8. Process contract (central-6ly1.3)

- `api_port: 0` binds an ephemeral port. `API_PORT=<port>` is still printed at bind.
- When first Ready, stdout prints one line: `EQOXIDE_READY {"api_port":n,"session":"...","zone_epoch":n}`.
- At exit, stdout prints one line `EQOXIDE_EXIT {"status":...,"reason":...}` and the process exits
  with that status's code:

| status | code |
| --- | --- |
| `camped` | 0 |
| `signal_logout` | 3 |
| `login_failed` | 4 |
| `session_lost` | 5 |
| `logout_incomplete` | 6 |
| `internal_error` | 1 |
| profile or usage error | 2 |

- A failed or lost session exits; it no longer waits for a signal. SIGTERM remains the fast,
  ordinary logout for disposable fixtures. `RUST_LOG` is honoured.

## Wire impact

Sections 5 and 6 add client packets only where an operation is invoked. Section 2 changes no
packets. Every Bead's PR states whether it changes what the client puts on the wire.
