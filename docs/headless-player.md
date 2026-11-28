# CPU player candidate

`eqoxide-headless` connects through the ordinary RoF2 login, world, character creation and zone
protocol. It runs the same physical controller as the graphical application on a 20ms CPU clock.
It loads native EQEmu `.map` v2 and `.wtr` v1/v2 files; it needs no display, GPU, textures, display
models, asset service or game string/spell files. Upstream source baseline is
`4bf520f2151976177a757428628995b33a3a3ee9`; the preparation appendix and check script are retained.

This is a candidate for staging qualification. Build and synthetic tests do not prove live play.
The authoritative selection and live results belong to the Operations project; this file documents
the executable and wire format.

## Build and launch

Run `scripts/build-headless` from this checkout. Its pinned Rust container uses the existing
locked dependency cache; `--online` permits missing dependencies to download. The output prints
`HEADLESS_BINARY=...`. Native Rust builds can use `cargo build -p eqoxide-headless --locked` after
disabling this repository's optional sccache/mold configuration if those tools are unavailable.
`scripts/check-headless-crates` runs the CPU tests without connecting a player.

Launch `eqoxide-headless --profile /absolute/private-profile.json`. The profile must be a regular
file with no group/other permissions, at most 64KiB. Invalid JSON errors never echo its contents.
Keep the real account password and control token outside this repository. The required JSON shape:

```json
{
  "login": {
    "login_host": "192.168.1.230", "login_port": 5999, "world_port": 9000,
    "username": "eqoxidepoc", "password": "PRIVATE",
    "character_name": "Eqoxidepoc",
    "character_create": {
      "race": 1, "class": 7, "gender": 0, "deity": 396, "start_zone": 2,
      "str": 80, "sta": 80, "agi": 105, "dex": 85, "wis": 75, "int": 75, "cha": 75
    }
  },
  "expected_world": { "name": "Bump Staging", "host": "192.168.1.220" },
  "api_port": 18765,
  "control_token": "PRIVATE-GENERATED-TOKEN-AT-LEAST-32-CHARACTERS",
  "maps_dir": "/private/native-maps"
}
```

`character_create` is optional if the named character exists. When absent, creation is refused;
when present, ordinary server name/combination/stat validation still applies. Cosmetics `face`,
`hairstyle`, `haircolor`, `beard`, `beardcolor`, `eyecolor1`, `eyecolor2` default to zero.
The example is the selected ordinary human monk combination on staging, not a general creation preset.

The listener binds exactly the specified loopback port before login. Port conflicts abort before
connecting. World selection requires exactly one advertised entry matching **both** name and host.
Login is attempted once; a failure remains readable through the API. No first-world fallback runs.
SIGINT/SIGTERM request normal logout. `camp` starts the ordinary approximately 30-second camp,
then normal logout and process exit; neither operation force-saves or edits the account externally.
A failed session remains observable until the process is signalled.

## HTTP control

Every route requires `Authorization: Bearer <control_token>`. There are no legacy mutation routes.
Read `GET /v2/player/state` first. Use its exact `session` and `zone_epoch` in each request:

```http
PUT /v2/player/commands/walk-1
Authorization: Bearer PRIVATE
Content-Type: application/json

{"session":"FROM-STATE","zone_epoch":1,"operation":{"kind":"move_for","east":1,"north":0,"duration_ms":300}}
```

Query `GET /v2/player/commands/walk-1`; repeat the identical PUT after losing a response.
An identical retry returns the retained result and never renews movement. A changed payload under
the same ID conflicts (409). A PUT response also carries `event_cursor`, the newest event cursor
once the command was recorded; pass it as `after` to wait for that command's events.

A command ID is never reusable in a session, including an ID refused at admission:
- The newest 65 536 terminal receipts are kept in full.
- An older ID answers GET and an identical PUT with 410 `command_id_expired` and its final
  `status` and `reason`. It never runs again.
- A session issues at most 1 048 576 identities. Past that, a new ID gets 503
  `session_id_space_exhausted`. `stop` and `attack_off` each keep a reserve of 1024 more, then get
  503 `safety_reserve_exhausted`.
- A 503 consumes no identity, so the same ID can be retried.
- A refusal at admission returns 409 with the receipt and `"admitted": false`.
- A body sent without `Content-Type: application/json` is 415 and consumes nothing.
- A server's refusal of an awaited command (merchant open, buy) has reason `server_refused`, with
  the server's text in `detail`.
- An awaited command's receipt can be visible shortly before state shows the inventory or merchant
  revision it changed: the network owner resolves the request before it publishes the new state.
  Applying the outcome forces a state publication. Before the next transaction, wait until state
  shows the changed revision (central-6ly1.7 will close this window).
- An awaited outcome still pending when the session becomes not ready or changes zone ends
  `unknown`, and transactions stay blocked until relog.
- A `Content-Type` of `application/json` or `application/*+json` is accepted.
- A malformed body consumes nothing and returns 422 `invalid_command_payload`. That covers a body
  that is not JSON, a payload of the wrong shape, and a unit operation such as `stop` with extra
  fields.
- While 64 or more receipts are live (non-terminal), ordinary commands get 503
  `live_command_limit`. `stop` and `attack_off` are still admitted, and count toward the 64.
- Payloads compare as parsed JSON values, the same way before and after eviction: `-0.0` equals
  `0.0`, but `1` and `1.0` differ.

Do not retry an uncertain transaction under a new ID.

The CPU owner expires movement after 1–2000ms and attack after a 1–5000ms lease independently of
caller silence. A new `attack` command renews the lease. Movement uses the shared 35 units/sec
intent and real collision; a local result does not prove the server accepted the resulting position.
Stop, attack-off, death, connection loss, shutdown and a new zone entry stop applicable control.
`stop` stops movement; use `attack_off` separately to stop attacking.

Attack-off on the wire.

- **Implicit stops.** These are lease expiry, readiness loss, death, zone entry, `sit` and `camp`. They send `OP_AutoAttack 0` only while the last attack packet the client sent was on; with attack already off, they send nothing, as a real client does. A zone entry resets that state to off, because the new zone's server starts with attack off.
- **Explicit `attack_off`.** It always sends its own `OP_AutoAttack 0`, attributed to it in `/v2/player/packets?command=<id>`, even when the client believes attack is off. Two explicit offs accepted before one send are two packets. Its receipt stays `accepted` until the network owner settles it, normally within one network tick:
  - `applied_local` with evidence `{"kind":"attack_off_sent"}` once its packet is queued into the reliable window, which retransmits it until acknowledged (the trace row's `acked_at_ms` shows acknowledgement);
  - `refused` with reason `stale_zone_epoch_before_dispatch` and evidence `{"kind":"attack_off_not_sent","packets":[]}` when the zone it was accepted in ended first. Such an off is not sent into the next zone.

  A readiness drop (death, link loss, geometry loading) does not fence an `attack_off`: it is the safety stop for that moment. When the session ends (including a kick or a logout deadline), a queued off that no network owner will send ends `refused` with reason `not_ready`, or `stale_zone_epoch_before_dispatch` if its zone had already ended. From the moment shutdown begins, `attack_off` commands are refused at admission. Every refused `attack_off` carries the `attack_off_not_sent` evidence, whichever step refused it. A burst of explicit offs goes out at most 8 per network tick, in order.
- **An implicit stop queued with an explicit off** adds no packet.
- **Known windows, until central-6ly1.7's claim slot.** In this build an `attack_off` receipt can settle wrongly, or late, in these cases. The packet trace (`/v2/player/packets?command=<id>`) is the authority for what was actually sent.
  - **Kick or other parked shutdown.** An off admitted and dispatched just before shutdown can stay `accepted` for up to 8 s. It is then `refused` / `not_ready` as the API closes.
  - **Host exit with the off already claimed.** If the network owner has taken an off but not yet sent it when the host exits (logout deadline, HTTP end), the receipt is `unknown` with no evidence.
  - **Same-iteration terminal and zone change.** The network owner can publish a new zone and end in the same host iteration. The host rereads its snapshot so the zone's reason wins, but that ordering is not under test.
  - **v1 backlog.** A v1 or UI off reports its outcome only when the whole explicit-off backlog has been sent, 8 per network tick.
  - **Failed zone handoff.** A zone crossing whose world reconnect fails ends a queued off `refused` / `not_ready`, not as a zone end.
  - **Lease event after a v1 off.** A v1 or UI off over a queued v2 `attack` can still be followed by an `attack_lease_expired` event; nothing is sent.
  - **Host-side failures.** A failed state publication, or the control server ending, can leave a queued off `accepted`, or settle it from a stale zone.
- **v1 and UI requests.** v1 `DELETE /v1/combat/attack` and UI toggles also always send, even over a queued off or a queued on (which they replace), but they carry no packet attribution and no zone.

| Operation kind | Fields / limits |
| --- | --- |
| `move_for` | `east`, `north` in [-1,1], `duration_ms` 1–2000; normalized direction, stand first |
| `stop` | none; priority movement stop |
| `target` | `entity_id` from the current received roster |
| `attack` | `lease_ms` 1–5000; live received NPC target within 30 units |
| `attack_off` | none; priority attack stop; always sends its own `OP_AutoAttack 0` while its zone lasts |
| `loot` | `entity_id` of a received dead NPC within 20 units; ordinary loot protocol |
| `sit`, `stand` | none; sit stops movement and attack (attack-off only if attack is on) |
| `say` | `text`, maximum 512 bytes |
| `tell` | `recipient`, `text` |
| `merchant_open` | `entity_id`, received within 30 units |
| `merchant_close` | none |
| `buy` | catalog `slot`, `quantity:1`; request also needs current `inventory_revision`, `merchant_revision` |
| `sell` | inventory `slot`, positive `quantity`; both revision fields required |
| `inventory_move` | `from_slot`, `to_slot` 0–32, `quantity:1`; inventory revision required |
| `camp` | none; ordinary camp and process shutdown |

Receipts distinguish `accepted`, `sent`, `applied_local`, `confirmed`, `refused`, `unknown`.
Each receipt carries `terminal`, `accepted_at_ms` and `updated_at_ms` on the host clock. `reason` is
always a snake_case code; a validation failure is `invalid_operation`, with prose in `detail`. Only
`accepted` and `sent` are non-terminal, and a terminal receipt never changes. A readiness drop or
zone change fences only non-terminal receipts: never-dispatched work becomes `refused`, and
dispatched work becomes `unknown`. Late server evidence for a fenced receipt is not applied.
This host leaves queued network work accepted until an actual drain outcome or correlated reply;
it does not manufacture a send receipt. Buy and merchant-open confirmation comes from the real
server echo. Most other network commands can report `unknown` after dispatch because the protocol
provides no correlated receipt. Local attack toggles do not prove landed melee; local chat echoes
do not prove delivery. Sell/inventory writes block further transactions until a fresh process;
an ambiguous buy does too. A buy that exceeds the eight-second receipt wait stays blocked
until a fresh process, even if a later zone fence proves that no packet was sent. Late in-process
transaction recovery is unsupported in this POC. Use normal camp/save/relog to reconcile. This
process provides no exactly-once guarantee across a crash.

State contains one published revision, native geometry digest, network tick/inbound/link ages,
predicted feet position, vitals, inventory and catalog revisions. The roster is received knowledge
within 500 units of the predicted position, nearest first (ties by spawn ID) and capped at 256.
Each entity carries `distance` from that position, `heading` and the nine `equipment` material IDs
as received. Every range check on a command measures from the self position in the last published
state, so a script reads the origin the host decides with. The entity's position is its current one,
which can have moved since that publication. Ordinary commands are not admitted until a publication
has shown a self position.
`complete:false` discloses that it is not an authoritative world roster; truncation is separate.
LOS is a local static native-map query. Unknown instance ID is null. Inventory includes local
prediction and does not prove a save.

Beside the prediction, `self` carries what the server said:
- `server_position`: the last position the server asserted for self, or null. Its fields are
  `position` (feet datum), `heading`, `source` and `at_ms`. `source` is one of:
  - `server_spawn`: the self spawn at zone entry;
  - `server_relocation`: a bind respawn in this zone, or a same-zone teleport, with the heading the
    packet carried. A bind into another zone is recorded by that zone's self spawn;
  - `server_correction`: a self position update more than 12 units from the client's mirrored
    position in the horizontal plane, the test that increments `corrections.count`;
  - `server_update`: any other self position update.

  The server does not echo a client's ordinary movement back to it, so in normal play this field
  holds the last server-originated placement, not the latest step. Cleared at zone entry, and null
  until the server's `OP_NewZone` for that zone arrives (the self spawn comes first). The host also
  waits for it before loading geometry and placing the body.
- `corrections`: `count` for the session, and `last` (`from`, `to`, `at_ms`), which is cleared at
  zone entry.
- `level`.
- `mana` and `endurance`, each `{value, max, known}`. Mana `max` is the highest mana the client
  has observed, which can be below the true maximum early in a session. `endurance.confirmed` is true once a server
  update has arrived.
- `xp`: `{pct}`, progress through the current level as the server reports it.
- `buffs`: `{slot, spell_id, ticks_remaining}`. `ticks_remaining` is the count in the last buff
  packet, not a live countdown. A negative value is a non-countdown buff, such as permanent
  (-1000) or an aura.
- `skills`.
- `dead`.
- `killer`: `{id, name}` after a death. `id` comes from the death packet and is kept even when
  `name` is unknown, in which case `name` is null. A self-inflicted death names the player.

Each received `OP_Damage` is also published as a `combat` event: `record_id` (monotonic),
`zone_epoch`, `source_id`, `target_id`, `source_name` and `target_name` (resolved when the packet
arrived, null if unknown then), `amount` (the damage on a `hit`, else 0), `spell_id` (null for melee),
`damage_type` (EQEmu's raw damage-type code, not a skill ID) and `result`. `result` is one of `hit`, `miss`, `block`, `parry`, `riposte`, `dodge`,
`invulnerable`, `rune`, `landed` (a spell action with zero damage) or `other`. A death of self is
published once as `died`, with the same `killer` and the session's `death_count`.

Anything lost before the host forwards it is reported as `source_events_dropped` with `source` and
`count`, so no loss is silent. The host forwards every 20 ms tick, and the sources are bounded as
follows:
- `game_event`: 200 events;
- `self_echo`: 32 echoes;
- `combat`: 512 records;
- `death`: reported when two deaths fall between two ticks.

### Self-position updates

The client streams `OP_ClientUpdate` at most every 280 ms while moving and every 1300 ms
otherwise. On the first physics step that shows the body at rest after it moved, it attempts one
update with the resting position, zero delta and animation 0, outside that throttle. The attempt
follows that step by one network tick (nominally 10 ms). A `stop` that arrives after the host has
drained commands for a tick waits for the next 20 ms step, so it can take two steps to show.
The rest decision is made per physics step, not per network tick. A step that covers at most
0.01 units is at rest; one over 0.1 units is motion that makes the next stop immediate. Rest updates
for a creep or a turn in place come no closer than 280 ms apart. While a server correction
is waiting for the controller to apply it, no rest update is sent. The graphical client does not
publish the step signal and keeps the plain cadence.

`self.last_position_send` is the last update the client attempted, or null before the first:
`sequence` (the wire sequence), `position` (feet datum; the wire z is 3.1 units higher),
`delta` and `animation` exactly as sent, `heading` in the state's convention, `reason`
(`moving`, `keepalive`, `rest` or `correction`), `sent_at_ms`, `age_ms`, and
`delivery_confirmed:false`. The update is an unreliable datagram; this proves the attempt, not
receipt. Top-level `clock_ms` is the host's monotonic clock, starting just after the control
port is bound; every
`*_at_ms` field uses it.

`GET /v2/player/events?after=SESSION:CURSOR&wait=5` long-polls for at most 5 seconds. Use the
state's `event_cursor` verbatim. Gaps are explicit; reread state and outstanding commands after one.
Each event is `{cursor, at_ms, event}`, and the buffer keeps the newest 32 768. State publication is
not an event, so an idle long-poll waits its full timeout. Read state with `GET /v2/player/state`.
State's `clock_ms` is read when the state is read, so no `*_at_ms` is newer than it.
Received chat (including NPC say) carries packet provenance; other game events are labeled client events.
Outbound local messages are not receipt evidence. `received_self_echo` records a server-returned
self message with channel/sender/text and `recipient_delivery_confirmed:false`; it does not confirm
delivery to another player. Legacy self-echo filtering remains unchanged. Received spawn class is exposed as nullable `class_id`.

### Packet trace

`GET /v2/player/packets?after=N&limit=L&opcode=NAME&command=ID&raw=1` uses the same bearer
token as state, commands, and events. Each session automatically keeps its newest 20,000
application packets from zone connections, including zone entry. Start with `after=0`, then
pass the response's `next` as `after` on the next request with the same filters. The cursor
is a number without a session ID. Reset it to 0 when the state's `session` changes.
The response pages in increasing `n` order. `limit` defaults to 500 and accepts 1–2000.
`opcode` matches the exact trace name, such as `OP_TARGET_MOUSE`. `command` matches the
command ID from the command URL. Unknown query parameters return HTTP 400.
`gap:true` means some indices after `after` were evicted, regardless of filters.

Each row has a delivery index `n`, capture time `at_ms` on the state's `clock_ms`
clock, direction `dir` (`in` or `out`), numeric `opcode`, `opcode_name`, `len` in bytes
including the opcode, `reliable`, `wire_seq`, `command_id`, and decoded `fields`.
`wire_seq` is null for unreliable packets. For fragmented packets it identifies the first
outbound fragment or the completing inbound fragment.

Inbound rows carry `arrived_at_ms` on the host clock and a session-wide `arrival_n`
assigned when the client reads the datagram from the socket, before reordering. Rows
from the same combined datagram share these values. Fragmented rows use the arrival of
the completing fragment. A retransmit or duplicate of an inbound reliable packet
does not create a second row. A datagram carrying a duplicate still consumes an
arrival index. If the sequence is still buffered, a duplicate can replace the
buffered payload. When the copies differ, the delivered row pairs the later bytes
with the first copy's `arrival_n` and `arrived_at_ms`. Socket reads that produce no
zone row also consume indices. Outbound rows have null arrival fields. Page by `n` and read `arrival_n`
to recover datagram arrival order.

For an inbound reliable row, `acked_at_ms` records when the socket accepts the first
cumulative ACK datagram covering its sequence. For a row awaiting ACK, queuing a
deferred datagram leaves the timestamp null until a successful socket send.
`ack_seq` is the actual sequence on that ACK, and `ack_exact` is true if it equals `wire_seq`. An immediate ACK is sent before the row
is captured, so `acked_at_ms` can be at or before `at_ms`. A missing ACK leaves
`acked_at_ms` and `ack_seq` null and `ack_exact` false. Poll the same cursor range to
check for an ACK on an existing row. Stop waiting if the stream ends before a covering
ACK is sent, since the fields can stay null forever. Outbound and unreliable rows have
the same null ACK fields and false `ack_exact`.

Use `raw=1` to include `payload_hex`, the lowercase hex of the decoded application body
without the opcode. `payload_truncated` is true whenever the body exceeded the retained
512-byte prefix. The 20,000-row ring retains at most 10,240,000 raw payload bytes
(about 9.77 MiB), plus record metadata and allocation overhead. A 2,000-row response
contains at most 2,048,000 hex characters.

Decoded `fields` use raw wire values. Positions are in x/y/z order and keep wire-datum z.
Headings use the raw protocol units.

| Opcode | Direction | Fields |
| --- | --- | --- |
| `OP_ZONE_ENTRY` (0x5089) | Inbound | `entity`, `name`, `position`, `heading` |
| `OP_NEW_SPAWN` (0x6097) | Inbound | `entity`, `name`, `npc`, `hp_percent`, `position`, `heading` |
| `OP_CLIENT_UPDATE` (0x7dfc) | Inbound | `entity`, `position`, `heading` |
| `OP_CLIENT_UPDATE` (0x7dfc) | Outbound | `sequence`, `position`, `delta`, `animation`, `heading`, `reason` |
| `OP_TARGET_COMMAND` (0x58e2) | Both | `target_id` |
| `OP_TARGET_MOUSE` | Outbound | `target_id` |
| `OP_DAMAGE` (0x6f15) | Inbound | `target`, `source`, `type`, `spell`, signed `damage` |
| `OP_MOB_HEALTH` (0x37b1) | Inbound | `entity`, `percent` |
| `OP_DELETE_SPAWN` (0x7280) | Inbound | `entity` |

Outbound `OP_CLIENT_UPDATE` `reason` is why the client sent it: `moving` (the 280 ms throttle
while moving), `keepalive` (1300 ms without another send), `rest` (owed when the body comes to rest
after moving or turning in place: zero delta and animation 0) or `correction` (re-streaming a server correction). It is the sender's own record and
is not on the wire; it matches `self.last_position_send.reason` for the same `sequence`.
Outbound `OP_CLIENT_UPDATE` uses the payload's own 16-bit `sequence` counter.
Its unreliable transport `wire_seq` is null. The `OP_NEW_SPAWN` `npc` byte uses
0 for a player, 1 for an NPC, 2 for a player's corpse, and 3 for an NPC's corpse.
Other opcodes have empty `fields`.

The network owner tags outbound packets emitted by a command drain, including
awaited merchant open and buy. The explicit loot command ID
follows its corpse queue entry and tags the open, item takes, and close. A movement
command tags the position updates it drives, including keepalives while it is active
and the rest update at its end. Correction re-streams and position windows influenced
by multiple motion commands have a null `command_id`, as do unowned position updates,
automatic loot, and inbound packets. Outbound rows record send attempts. Reliable retransmits reuse the original wire bytes and increment
transport counters without adding another application row.

The response's `counters` cover login, world, and zone transport. They come from one
snapshot of the trace. `sent` counts successful UDP sends, including control traffic.
`resent` counts successful repeat sends of datagrams that already reached the socket.
`received` counts UDP datagrams before decoding. `dropped_unretried` counts terminal
send failures, deferred control packets discarded by the queue or at teardown, and
reliable datagrams abandoned at teardown that never reached the socket.
`sequence_gaps` counts runs of missing incoming reliable sequences when they first
appear. Contiguous future packets and later fills of an existing hole keep the same count.

### Loot evidence

Automatic corpse looting and automatic zone-line requests are disabled for CPU sessions.
Only explicit `loot` requests begin looting, including for the character's own kills. The ordinary
flow requests each received corpse catalog slot once, one item at a time. An unacknowledged take
is never retried automatically. `confirmed` with `corpse_opened` proves the open handshake only.
Corpse catalog packets and take acknowledgements never credit player inventory; acquisition needs
a separate server inventory packet, and persistence still needs normal save/relog evidence.

## Limits

Initial qualification covers one dry Qeynos2 route. Dynamic doors, native climb metadata,
automatic route planning/zoning, spells, P2P trade, corpse retrieval and graphical visibility are
outside this POC. Native map v1 is rejected. Missing or invalid collision prevents movement/readiness;
water-data failure is disclosed independently. Geometry uses bounded decompression, model/placement
expansion and grid construction. The runtime never accepts privileged observer facts as perception.
