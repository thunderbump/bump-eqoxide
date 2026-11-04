//! EQ login protocol: login server → world server → zone server handshake.
//!
//! Handles only the connection/authentication state machine.  All gameplay
//! packet effects (spawn registration, HP updates, etc.) are delegated to
//! `packet_handler::apply_packet` so there is no duplication with the render loop.

use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::time::{Duration, sleep};

use cbc::{Decryptor, Encryptor};
use des::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, block_padding::NoPadding};
use des::Des;

use eqoxide_core::charname::{check_name, describe_violation, normalize_name};
use eqoxide_core::config::{CharacterCreate, LoginConfig};
use crate::gameplay::{record_app_packet, run_gameplay_phase, GameplayLifecycle};
use crate::action_loop::{ActionLoop, ActionLoopSlots, publish_doors};
use crate::packet_handler::apply_packet;
use crate::protocol::*;
use crate::transport::{AppPacket, EqStream};
use eqoxide_core::game_state::GameState;

type DesCbcEnc = Encryptor<Des>;
type DesCbcDec = Decryptor<Des>;

// ── DES helpers ──────────────────────────────────────────────────────────────
// The EQEmu loginserver encrypts the OP_Login credential block and the OP_LoginAccepted
// reply with DES-CBC using an all-zero key AND all-zero IV (loginserver/encryption.cpp
// `eqcrypt_block`). This is shared by BOTH the Titanium and the SoD/RoF2 login listeners —
// SoD did NOT change the login encryption, only the response opcode numbers (#404). So these
// helpers are correct for the SoD handshake unchanged.

fn des_encrypt(data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    DesCbcEnc::new_from_slices(&[0u8; 8], &[0u8; 8])
        .expect("DES key/iv")
        .encrypt_padded_mut::<NoPadding>(&mut out, data.len())
        .expect("DES encrypt")
        .to_vec()
}

fn des_decrypt(data: &[u8]) -> Vec<u8> {
    let enc_len = (data.len() / 8) * 8;
    if enc_len == 0 { return Vec::new(); }
    let mut buf = data[..enc_len].to_vec();
    match DesCbcDec::new_from_slices(&[0u8; 8], &[0u8; 8])
        .expect("DES key/iv")
        .decrypt_padded_mut::<NoPadding>(&mut buf)
    {
        Ok(out) => out.to_vec(),
        Err(_)  => buf,
    }
}

// ── Public types ─────────────────────────────────────────────────────────────

/// Exact advertised world identity required by the selected login flow.
pub struct ExpectedWorld {
    pub name: String,
    pub host: String,
}

/// World-server credentials needed to reconnect after a zone change.
pub struct WorldCredentials {
    pub lsid:       i32,
    pub ls_key:     String,
    pub world_host: String,
    pub world_port: u16,
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Connect, authenticate, enter zone, then run the gameplay loop.
/// Retries up to `max_retries` times on transient failures.
pub async fn run_login_flow(
    config:      LoginConfig,
    max_retries: u32,
    slots:       ActionLoopSlots,
    lifecycle:   GameplayLifecycle,
) -> Result<(), String> {
    run_login_flow_with_world(config, max_retries, slots, lifecycle, None).await
}

/// Log in only when the complete server list contains exactly one expected world.
/// Selection failures stop immediately without sending a play request or reconnecting.
pub async fn run_login_flow_selected(
    config: LoginConfig,
    max_retries: u32,
    slots: ActionLoopSlots,
    lifecycle: GameplayLifecycle,
    expected: ExpectedWorld,
) -> Result<(), String> {
    run_login_flow_with_world(config, max_retries, slots, lifecycle, Some(expected)).await
}

async fn run_login_flow_with_world(
    config: LoginConfig,
    max_retries: u32,
    slots: ActionLoopSlots,
    lifecycle: GameplayLifecycle,
    expected: Option<ExpectedWorld>,
) -> Result<(), String> {
    for attempt in 1..=max_retries {
        if attempt > 1 {
            tracing::warn!("EQ: retry {}/{}", attempt, max_retries);
            sleep(Duration::from_secs(3)).await;
        }
        match run_login_phase(&config, &lifecycle.net_health, &slots.controller, &slots.interact.doors_shared, expected.as_ref()).await {
            // Creation rejections and world selection failures must surface immediately.
            Err(LoginError::Fatal(e)) => return Err(e),
            Err(LoginError::Retryable(e)) => tracing::warn!("EQ: login failed (attempt {}): {}", attempt, e),
            Ok((stream, net_rx, mut gs, world_creds)) => {
                if expected.is_some() {
                    // Selected-world CPU sessions are driven explicitly. A corpse received during
                    // the login burst must not start unsolicited gameplay when the loop begins.
                    gs.automatic_loot = false;
                    gs.automatic_zone_crossing = false;
                    gs.pending_loot.clear();
                    gs.loot_queued_at = None;
                }
                // Seed /entities map with everything discovered during login.
                {
                    // #643: this is the SECOND roster publisher (the action loop's `sync_entities` is
                    // the first). It goes through the same `WorldSlots::publish_entities` so it
                    // cannot seed positions without poses — which is precisely what it did do when
                    // `entity_poses` was added and only the other publisher was updated, leaving
                    // every `poses` key missing for the whole window between login and the first
                    // action loop tick. See that method's doc comment.
                    let n = slots.world.publish_entities(&gs.world.entities);
                    tracing::info!("NAV: entity map seeded with {} entities", n);
                }
                // Seed zone points (in case OP_SEND_ZONE_POINTS arrived during login phase).
                if !gs.world.zone_points.is_empty() {
                    *slots.world.zone_points.lock().unwrap() = gs.world.zone_points.clone();
                    tracing::info!("NAV: {} zone points seeded", gs.world.zone_points.len());
                }
                let char_name = config.character_name.clone();
                let action_loop = ActionLoop::new(slots);
                run_gameplay_phase(stream, net_rx, gs, char_name, action_loop, world_creds, lifecycle).await;
                return Ok(());
            }
        }
    }
    Err(format!("Login failed after {} attempts", max_retries))
}

// ── Login phase ───────────────────────────────────────────────────────────────

/// #419: the login handshake drain's per-packet liveness stamp. It MUST go through the canonical
/// `record_app_packet` recorder rather than writing `NetHealth::last_packet` directly, so there is a
/// single writer of that liveness field — the one that also clears the wedge-streak clock
/// (`first_unanswered_probe_sent`). This is defensive/latent hygiene, not an active-bug fix: no code
/// path re-enters login after gameplay today (so no stale streak can reach this drain), but a
/// bypassing raw write here would resurrect the #371 false-alive if a relogin-without-restart path is
/// ever added. Extracted into a named helper so `login_liveness_stamp_goes_through_canonical_recorder`
/// can pin it: reverting this body to a raw `last_packet = now` write makes that test RED.
fn record_login_liveness(net_health: &eqoxide_ipc::NetHealthShared, now: std::time::Instant) {
    record_app_packet(&mut net_health.lock().unwrap(), now);
}

/// Run the full handshake (login → world → zone entry).
/// Returns the live zone stream, its packet receiver, accumulated GameState, and world credentials.
///
/// `last_inbound` is bumped as real inbound packets are drained here, exactly like the gameplay
/// loop's drain does (gameplay.rs) — this handshake can legitimately run past CONN_STALE_SECS
/// (multiple server hops + a fresh char list), and without this the connection-health check
/// (`connected` in `/v1/observe/debug`) would falsely go stale/disconnected while login is healthy
/// and simply still in progress.
///
/// `doors` is the published `GET /v1/observe/doors` roster (#1022). It is a bare
/// [`eqoxide_ipc::DoorsShared`] rather than the whole `InteractSlots`, and NOT an `ActionLoop` —
/// the loop does not exist yet at this point in `run_login_flow`, which is precisely why this path
/// had no door publisher before. Same slot, same projection (`action_loop::publish_doors`) and same
/// per-drain-pass gate `run_zone_entry_handshake` uses on the re-zone path. [`run_login_handshake`]
/// publishes from TWO call expressions, not one: that gated call inside the drain, and an
/// unconditional one after the loop — `PhaseResult::Done` breaks out from INSIDE the inner drain,
/// so the gated call never runs for the pass that ends the handshake, and on a fast server that is
/// the pass the whole door burst lands on. Both are documented at their own sites.
///
/// This wrapper owns ONE thing beyond that state machine: a login attempt that FAILS must not
/// leave the door roster it published standing. That is the shape #1016 review B4 found on the
/// re-zone path, arriving on this one the moment #1022 gave it a publisher — a timeout, a rejected
/// credential or a dropped world/zone reconnect after even one `OP_SpawnDoor` had landed would
/// otherwise leave a partial, well-formed, readable roster of a zone the client never entered,
/// served by GET /v1/observe/doors and clickable through POST /v1/interact/click_door, for the
/// whole 3s backoff AND the entire next attempt (which starts from a fresh `GameState`, so nothing
/// republishes over the stale rows until that attempt drains its own first packet). Empty is the
/// honest reading of a failed login: no zone was entered, so no zone's doors are ours to report.
///
/// It is a wrapper rather than a `clear()` at each error exit because [`run_login_handshake`] has
/// six of them today — three explicit `return Err`s (`PhaseResult::Error`, `PhaseResult::Fatal`,
/// the `is_timed_out` arm) and three `?`s on `EqStream::connect` (login, world reconnect, zone
/// reconnect) — and a seventh added later must not be able to skip it. Pinned by
/// `run_login_phase_clears_the_roster_it_published_when_the_attempt_fails_1022`.
async fn run_login_phase(
    config: &LoginConfig,
    net_health: &eqoxide_ipc::NetHealthShared,
    controller: &eqoxide_ipc::ControllerSlots,
    doors: &eqoxide_ipc::DoorsShared,
    expected: Option<&ExpectedWorld>,
) -> Result<(EqStream, UnboundedReceiver<AppPacket>, GameState, WorldCredentials), LoginError> {
    let outcome = run_login_handshake(config, net_health, controller, doors, expected).await;
    if outcome.is_err() {
        doors.lock().unwrap().clear();
    }
    outcome
}

/// The login state machine itself — see [`run_login_phase`], the only caller, for what wraps it.
async fn run_login_handshake(
    config: &LoginConfig,
    net_health: &eqoxide_ipc::NetHealthShared,
    controller: &eqoxide_ipc::ControllerSlots,
    doors: &eqoxide_ipc::DoorsShared,
    expected: Option<&ExpectedWorld>,
) -> Result<(EqStream, UnboundedReceiver<AppPacket>, GameState, WorldCredentials), LoginError> {
    let (net_tx, mut net_rx) = mpsc::unbounded_channel::<AppPacket>();

    tracing::info!("EQ: connecting to login server {}:{}", config.login_host, config.login_port);
    let mut stream = EqStream::connect(&config.login_host, config.login_port, net_tx.clone(), net_health.clone())
        .await
        .map_err(|e| format!("Login server connection failed: {e}"))?;

    tracing::info!("EQ: login session established — waiting for handshake");
    stream.send_app_packet(OP_SESSION_READY, &2u32.to_le_bytes());

    let mut proto = match expected {
        Some(expected) => LoginProtocol::new_selected(config, expected),
        None => LoginProtocol::new(config),
    };
    let mut gs    = GameState::new();
    gs.player_name = config.character_name.clone();

    'login: loop {
        stream.poll_recv();
        stream.poll_resend(); // retransmit un-ACKed login reliables (#254)

        let mut drained_a_packet = false;
        while let Ok(packet) = net_rx.try_recv() {
            drained_a_packet = true;
            // Apply gameplay side effects.
            apply_packet(&mut gs, &packet);
            record_login_liveness(net_health, std::time::Instant::now());

            // Handle login-protocol state transitions.
            match proto.handle(&packet, &mut stream, &gs) {
                PhaseResult::Continue                => {}
                PhaseResult::Done                    => break 'login,
                PhaseResult::Error(e)                => return Err(LoginError::Retryable(e)),
                PhaseResult::Fatal(e)                => return Err(LoginError::Fatal(e)),
                PhaseResult::ReconnectWorld { host, port } => {
                    drop(stream);
                    sleep(Duration::from_millis(100)).await;
                    tracing::info!("EQ: connecting to world {}:{}", host, port);
                    stream = EqStream::connect(&host, port, net_tx.clone(), net_health.clone())
                        .await
                        .map_err(|e| format!("World connection failed: {e}"))?;
                    proto.on_world_connected(&mut stream);
                }
                PhaseResult::ReconnectZone { host, port } => {
                    drop(stream);
                    sleep(Duration::from_millis(800)).await;
                    tracing::info!("EQ: connecting to zone {}:{}", host, port);
                    stream = EqStream::connect(&host, port, net_tx.clone(), net_health.clone())
                        .await
                        .map_err(|e| format!("Zone connection failed: {e}"))?;
                    // Purge stale spawns/doors before zone entry so the OP_ZoneSpawns/OP_SpawnDoor
                    // stream repopulates fresh, and re-arm the once-per-zone-in OP_NewZone apply (#322).
                    //
                    // Through `ControllerSlots` for the same reason `run_zone_entry_handshake` does
                    // (#846 review B1) — the disclosure clear has to reach the view the mirror reads,
                    // or the next `stream_position` tick undoes it. Nothing has published into that
                    // view yet on THIS path (the render thread only marks it `initialized` after
                    // camera-init, which needs a `player_id` from a snapshot, and `run_login_phase`
                    // is handed no snapshot slot to publish one — it is a parameterless-by-signature
                    // fact, not a timing hope), so today this call is a no-op rather than a fix. It
                    // is written the same way anyway: the asymmetry "one of the two zone-in sites
                    // clears the view" is precisely the shape that let B1 through.
                    controller.begin_zone_in(&mut gs);
                    proto.on_zone_connected(&mut stream);
                }
            }
        }

        // #1022: publish the door roster from THIS drain too, on the same
        // `drained_a_packet` gate `run_zone_entry_handshake` uses for its own copy of this call
        // (#937/#1016 review B1) — `gs.world.doors` cannot have changed on a pass that applied
        // nothing, so an ungated call would be pure lock/clone churn on every 10ms tick of a
        // handshake that can run for a minute.
        //
        // Before this line the FIRST zone-in of a session had no door publisher at all. #937/#1016
        // gave `run_zone_entry_handshake` one, but that function is only reachable from
        // `run_gameplay_phase` — i.e. only on a RE-zone. The first zone-in runs this state machine
        // instead, and `apply_packet` above routes `OP_SPAWN_DOOR` into `gs.world.doors` exactly as
        // it does there, so the records were held, applied, and never published: GET
        // /v1/observe/doors served a fresh, well-formed, confident `[]` for a zone whose doors the
        // client had already parsed, and POST /v1/interact/click_door answered the empty-roster 404
        // for a door it knew about. That window ran from session start until `run_gameplay_phase`
        // drained its first packet, because `ActionLoop::sync_doors`' one call site is that drain
        // and it is unconditional per drained packet — so it was NOT bounded by door arrival, it
        // spanned the whole login handshake (connect, auth, char select, world reconnect, zone
        // connect, zone load).
        if drained_a_packet {
            publish_doors(&gs, doors);
        }

        sleep(Duration::from_millis(10)).await;

        if proto.is_timed_out() {
            return Err(LoginError::Retryable("Login timed out".to_string()));
        }
    }

    // #1022: `PhaseResult::Done` breaks 'login from INSIDE the inner drain, so the gated publish
    // above never runs for the pass that ended the handshake. Without this line every door record
    // applied on that final pass — which on a fast server is where the whole burst lands, since
    // EQEmu queues OP_SpawnDoor and the OP_SendExpZonein this state machine completes on from the
    // SAME connect-phase handler, doors first (EQEmu `zone/client_packet.cpp`,
    // `Handle_Connect_OP_ReqClientSpawn`) — would stay unpublished until `run_gameplay_phase`
    // drained its first packet. Unconditional rather than gated: this runs exactly once per
    // successful login, so there is no churn to gate away, and gating it on a flag scoped to the
    // last loop iteration would be a second, subtler way to miss the same pass.
    publish_doors(&gs, doors);

    let world_creds = WorldCredentials {
        lsid:       proto.lsid,
        ls_key:     proto.ls_key.clone(),
        world_host: proto.world_host.clone(),
        world_port: config.world_port,
    };
    Ok((stream, net_rx, gs, world_creds))
}

// ── Login protocol state machine ──────────────────────────────────────────────

enum PhaseResult {
    Continue,
    Done,
    Error(String),
    /// Unrecoverable handshake failure — retrying with the same config can't succeed (e.g. the
    /// server rejected character creation for an invalid combo). Stops the retry loop. (eqoxide#6)
    Fatal(String),
    ReconnectWorld { host: String, port: u16 },
    ReconnectZone  { host: String, port: u16 },
}

/// Outcome of one login attempt. `Retryable` errors (timeouts, transient connection drops) are
/// retried; `Fatal` errors (a server-rejected create or failed world selection) break the retry loop so the user
/// sees the real reason instead of an endless "Login timed out". `?` on a `String` maps to
/// `Retryable` via the `From` impl, so existing connection error sites are unchanged.
enum LoginError {
    Retryable(String),
    Fatal(String),
}

impl From<String> for LoginError {
    fn from(s: String) -> Self { LoginError::Retryable(s) }
}

impl std::fmt::Display for LoginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self { LoginError::Retryable(s) | LoginError::Fatal(s) => f.write_str(s) }
    }
}

/// Tracks only the login-protocol handshake state.
/// Game state (entities, positions) is maintained separately in GameState.
struct LoginProtocol<'a> {
    config:       &'a LoginConfig,
    expected:     Option<&'a ExpectedWorld>,
    // Credentials returned by the login server
    lsid:         i32,
    ls_key:       String,
    // World server info parsed from the server list
    world_host:   String,
    world_server_id: u32,
    // Handshake progress flags
    done_session_ready:    bool,
    done_login_accepted:   bool,
    done_server_list:      bool,
    done_play_everquest:   bool,
    done_char_info:        bool,
    // Character-creation handshake (only used when `config.create` is set and the
    // configured character is missing from the OP_SendCharInfo char-select list).
    awaiting_name_approval: bool,
    create_attempted:       bool,
    /// True after OP_CharacterCreate is sent, while we await the server's verdict. Success arrives
    /// as a fresh OP_SendCharInfo; FAILURE arrives as OP_ApproveName{0} (EQEmu world/client.cpp),
    /// which — since `awaiting_name_approval` is already false by then — would otherwise be ignored
    /// and the client would hang/timeout. This flag routes that reply to a Fatal error. (eqoxide#6)
    awaiting_create_result: bool,
    done_zone_server_info: bool,
    done_zone_entry:       bool,
    done_new_zone:         bool,
    done_client_ready:     bool,
    done_zone_weather:     bool,
    start_time: std::time::Instant,
}

impl<'a> LoginProtocol<'a> {
    fn new(config: &'a LoginConfig) -> Self {
        LoginProtocol {
            config,
            expected: None,
            lsid: 0, ls_key: String::new(),
            world_host: String::new(), world_server_id: 0,
            done_session_ready:    false,
            done_login_accepted:   false,
            done_server_list:      false,
            done_play_everquest:   false,
            done_char_info:        false,
            awaiting_name_approval: false,
            awaiting_create_result: false,
            create_attempted:       false,
            done_zone_server_info: false,
            done_zone_entry:       false,
            done_new_zone:         false,
            done_client_ready:     false,
            done_zone_weather:     false,
            start_time: std::time::Instant::now(),
        }
    }

    fn new_selected(config: &'a LoginConfig, expected: &'a ExpectedWorld) -> Self {
        Self { expected: Some(expected), ..Self::new(config) }
    }

    fn is_timed_out(&self) -> bool {
        self.start_time.elapsed() > Duration::from_secs(120)
    }

    fn on_world_connected(&self, stream: &mut EqStream) {
        let lsid_str  = format!("{}\0{}\0", self.lsid, self.ls_key);
        let mut login_info = vec![0u8; SIZE_LOGIN_INFO];
        let lsid_bytes = lsid_str.as_bytes();
        login_info[..lsid_bytes.len().min(64)].copy_from_slice(&lsid_bytes[..lsid_bytes.len().min(64)]);
        stream.send_app_packet(OP_SEND_LOGIN_INFO, &login_info);
        tracing::info!("EQ: sent login info to world");
    }

    fn on_zone_connected(&self, stream: &mut EqStream) {
        let mut cze = vec![0u8; SIZE_CLIENT_ZONE_ENTRY];
        let name_bytes = self.config.character_name.as_bytes();
        cze[4..4 + name_bytes.len().min(64)].copy_from_slice(&name_bytes[..name_bytes.len().min(64)]);
        stream.send_app_packet(OP_ZONE_ENTRY, &cze);
        tracing::info!("EQ: sent zone entry for '{}'", self.config.character_name);
    }

    /// Process one packet for login-protocol transitions.
    /// `gs` is passed read-only so handlers can read player_name / spawn state.
    fn handle(&mut self, packet: &AppPacket, stream: &mut EqStream, gs: &GameState) -> PhaseResult {
        match packet.opcode {
            OP_CHAT_MESSAGE if !self.done_session_ready => {
                self.done_session_ready = true;
                self.send_credentials(stream);
                PhaseResult::Continue
            }
            OP_LOGIN_ACCEPTED if !self.done_login_accepted => {
                self.done_login_accepted = true;
                if !self.parse_login_accepted(&packet.payload) {
                    return PhaseResult::Error("Login credentials rejected".to_string());
                }
                self.send_server_list_request(stream);
                PhaseResult::Continue
            }
            OP_SERVER_LIST_RESPONSE if !self.done_server_list => {
                if let Err(error) = self.parse_server_list(&packet.payload) {
                    return PhaseResult::Fatal(error);
                }
                self.done_server_list = true;
                self.send_play_everquest(stream);
                PhaseResult::Continue
            }
            OP_PLAY_EVERQUEST_RESP if !self.done_play_everquest => {
                self.done_play_everquest = true;
                self.world_reconnect()
            }
            OP_SEND_CHAR_INFO if !self.done_char_info => {
                let name = &self.config.character_name;
                if char_in_list(&packet.payload, name) {
                    // Character exists on the account — enter world as normal.
                    self.done_char_info = true;
                    self.awaiting_create_result = false; // create succeeded (char now in the list)
                    self.send_enter_world(stream);
                    PhaseResult::Continue
                } else if let Some(cc) = &self.config.create {
                    // Not on the account: run the create handshake, unless we
                    // already tried (and the fresh list still lacks the char).
                    if self.create_attempted {
                        return PhaseResult::Fatal(format!(
                            "character '{}' still missing after a create attempt — the server \
                             rejected creation. Check the character_create block in your config \
                             (valid race/class/deity combo, stats total, and start_zone = a zone_id).",
                            name));
                    }
                    // #1092: never put a name on the wire that we can already prove the server
                    // will reject. Config load checks this too, but the guard lives here as well
                    // because THIS is the send site — every path that reaches OP_ApproveName goes
                    // through it, whatever built the LoginConfig. It also underwrites the
                    // rejection message below: reaching that arm now implies these rules held.
                    if let Err(v) = check_name(name) {
                        return PhaseResult::Fatal(describe_violation(name, &v));
                    }
                    self.create_attempted = true;
                    self.awaiting_name_approval = true;
                    let pkt = build_approve_name(name, cc.race, cc.class);
                    stream.send_app_packet(OP_APPROVE_NAME, &pkt);
                    tracing::info!("EQ: char '{}' not found — requesting name approval (race={} class={})",
                        name, cc.race, cc.class);
                    PhaseResult::Continue
                } else {
                    PhaseResult::Error(format!(
                        "character '{}' not on account and no character_create config", name))
                }
            }
            OP_APPROVE_NAME if self.awaiting_name_approval => {
                self.awaiting_name_approval = false;
                let approved = packet.payload.first().copied().unwrap_or(0) == 1;
                if !approved {
                    // #1092: the locally-decidable rules were proven to hold on the exact bytes
                    // sent (the guard above), so the reason is necessarily one of the two the
                    // client cannot evaluate. The reply is still a bare 1-byte 0x00 with no
                    // reason field — we are narrowing the possibilities, not reading one off.
                    return PhaseResult::Error(format!(
                        "server rejected character name '{}'. The rules this client can check \
                         locally all held, so the reason is server-side only: the name matches \
                         the name_filter table, or it is already taken.",
                        self.config.character_name));
                }
                let cc = self.config.create.as_ref().expect("create set while awaiting approval");
                let pkt = build_char_create(cc);
                stream.send_app_packet(OP_CHARACTER_CREATE, &pkt);
                self.awaiting_create_result = true;
                tracing::info!("EQ: name approved — sent OP_CharacterCreate for '{}' (race={} class={} deity={} zone={})",
                    self.config.character_name, cc.race, cc.class, cc.deity, cc.start_zone);
                // Server replies with a fresh OP_SendCharInfo (success) or OP_ApproveName{0}
                // (failure) — the latter is caught by the awaiting_create_result arm below.
                PhaseResult::Continue
            }
            // Create verdict: after OP_CharacterCreate, an OP_ApproveName reply means FAILURE
            // (success comes as OP_SendCharInfo). EQEmu sends OP_ApproveName{0} when
            // CheckCharCreateInfoSoF rejects the combo. Surface a clear, actionable Fatal error
            // and stop retrying — the same config will be rejected every time. (eqoxide#6)
            OP_APPROVE_NAME if self.awaiting_create_result => {
                self.awaiting_create_result = false;
                PhaseResult::Fatal(format!(
                    "server rejected character creation for '{}' (race={} class={} deity={} \
                     start_zone={}). The combo is invalid, the stats don't total, or start_zone \
                     isn't a valid zone_id for it. Fix the character_create block in your config.",
                    self.config.character_name,
                    self.config.create.as_ref().map(|c| c.race).unwrap_or(0),
                    self.config.create.as_ref().map(|c| c.class).unwrap_or(0),
                    self.config.create.as_ref().map(|c| c.deity).unwrap_or(0),
                    self.config.create.as_ref().map(|c| c.start_zone).unwrap_or(0)))
            }
            OP_ZONE_SERVER_INFO if !self.done_zone_server_info => {
                self.done_zone_server_info = true;
                if packet.payload.len() >= SIZE_ZONE_SERVER_INFO {
                    let info     = unsafe { safe_read::<ZoneServerInfo_S>(&packet.payload) };
                    let zone_ip  = String::from_utf8_lossy(&info.ip).trim_end_matches('\0').to_string();
                    let zone_ip  = if zone_ip.is_empty() || zone_ip == "0.0.0.0" {
                        self.config.login_host.clone()
                    } else { zone_ip };
                    return PhaseResult::ReconnectZone { host: zone_ip, port: info.port };
                }
                PhaseResult::Continue
            }
            OP_ZONE_ENTRY if !self.done_zone_entry => {
                self.done_zone_entry = true;
                // Server echoes back the player's own spawn in RoF2 variable-length format.
                // apply_packet already handled OP_ZONE_ENTRY; just log the spawn for debugging.
                if let Some((info, _)) = parse_rof2_spawn(&packet.payload) {
                    if !info.name.is_empty() {
                        tracing::info!("EQ: server zone entry: spawn_id={} name={:?}",
                            info.spawn_id, info.name);
                    }
                }
                PhaseResult::Continue
            }
            // Only the FIRST OP_NewZone drives the handshake: the server sends a second copy in reply
            // to the OP_ReqNewZone below, and re-sending OP_ReqClientSpawn on it would make the
            // server re-issue the whole door/object/zone-point stream (#322).
            OP_NEW_ZONE if !self.done_new_zone => {
                self.done_new_zone = true;
                // apply_packet already updated gs.world.zone_name; just send protocol response.
                stream.send_app_packet(OP_REQ_CLIENT_SPAWN, &[]);
                tracing::info!("EQ: zone: {} — sent ReqClientSpawn", gs.world.zone_name);
                PhaseResult::Continue
            }
            OP_WEATHER if !self.done_zone_weather => {
                self.done_zone_weather = true;
                stream.send_app_packet(OP_REQ_NEW_ZONE, &[]);
                tracing::info!("EQ: zone weather received — sent ReqNewZone");
                PhaseResult::Continue
            }
            OP_SEND_EXP_ZONE_IN if !self.done_client_ready => {
                self.done_client_ready = true;
                stream.send_app_packet(OP_SEND_EXP_ZONE_IN, &[]);
                stream.send_app_packet(OP_CLIENT_READY, &[]);
                tracing::info!("EQ: zone entry complete — gameplay starts");
                PhaseResult::Done
            }
            op => {
                // Suppress known noise: keepalive, ground spawns, zone points
                // (handled by apply_packet), world-login flow opcodes.
                // RoF2 opcode values from patch_RoF2.conf.
                const SILENT: &[u16] = &[
                    // Char-select world chatter — informational packets sent to every client;
                    // this headless client renders no char-select/membership UI, so
                    // dropping them is correct (eqoxide#20). Values from patch_RoF2.conf.
                    // (OP_GuildsList 0x507a is NOT dropped — apply_packet parses it into the guild
                    // directory for /v1/guild/* and /observe/debug guild identity, #295.)
                    0x5475, // OP_SendMaxCharacters — character-slot count
                    0x7acc, // OP_SendMembership — gold/silver account status
                    0x057b, // OP_SendMembershipDetails — membership feature matrix
                    0x6fca, // OP_GroundSpawn (RoF2) — ground item drops, no action needed
                    OP_SEND_ZONE_POINTS, // handled by apply_packet
                    OP_SPECIAL_MESG,     // NPC dialogue, handled by apply_packet
                    OP_FORMATTED_MESSAGE, // eqstr text, handled by apply_packet
                    OP_SIMPLE_MESSAGE,   // eqstr text, handled by apply_packet
                    OP_EMOTE,            // world/NPC emote, handled by apply_packet
                    OP_ENTER_WORLD,      // world login flow
                    OP_POST_ENTER_WORLD,
                    OP_EXPANSION_INFO,
                    OP_LOG_SERVER,
                    OP_APPROVE_WORLD,
                    // SoD login listener sends this just before OP_LoginAccepted; it carries
                    // only expansion-offer data this headless client doesn't use (#404).
                    OP_LOGIN_EXPANSION_PACKET_DATA,
                ];
                if !SILENT.contains(&op) {
                    tracing::info!("EQ: unhandled opcode 0x{:04x} ({} bytes)", op, packet.payload.len());
                }
                PhaseResult::Continue
            }
        }
    }

    // ── Packet builders ───────────────────────────────────────────────────────

    fn send_credentials(&self, stream: &mut EqStream) {
        tracing::info!("EQ: sending login credentials");
        stream.send_app_packet(OP_LOGIN, &build_login_request(&self.config.username, &self.config.password));
    }

    fn send_server_list_request(&self, stream: &mut EqStream) {
        stream.send_app_packet(OP_SERVER_LIST_REQUEST, &4u32.to_le_bytes());
        tracing::info!("EQ: requested server list");
    }

    fn send_play_everquest(&self, stream: &mut EqStream) {
        stream.send_app_packet(OP_PLAY_EVERQUEST_REQ, &build_play_everquest(self.world_server_id));
        tracing::info!("EQ: sent play everquest request (server_id={})", self.world_server_id);
    }

    fn send_enter_world(&self, stream: &mut EqStream) {
        let mut buf = vec![0u8; SIZE_ENTER_WORLD];
        let name_bytes = self.config.character_name.as_bytes();
        buf[..name_bytes.len().min(64)].copy_from_slice(&name_bytes[..name_bytes.len().min(64)]);
        stream.send_app_packet(OP_ENTER_WORLD, &buf);
        stream.send_app_packet(OP_POST_ENTER_WORLD, &[]);
        tracing::info!("EQ: entering world as '{}'", self.config.character_name);
    }

    // ── Packet parsers ────────────────────────────────────────────────────────

    /// Returns false if the server rejected the credentials.
    fn parse_login_accepted(&mut self, payload: &[u8]) -> bool {
        match parse_login_accepted_payload(payload) {
            Some((lsid, key)) => {
                self.lsid   = lsid;
                self.ls_key = key;
                tracing::info!("EQ: login accepted");
                true
            }
            None => {
                tracing::error!("EQ: login rejected by server (success=0)");
                false
            }
        }
    }

    fn parse_server_list(&mut self, payload: &[u8]) -> Result<(), String> {
        let (server_id, host, name) = match self.expected {
            Some(expected) => select_expected_world(payload, expected)?,
            None => parse_server_list_payload(payload, &self.config.login_host),
        };
        tracing::info!("EQ: world server: id={} name={} host={}", server_id, name, host);
        self.world_server_id = server_id;
        self.world_host = host;
        Ok(())
    }

    fn world_reconnect(&self) -> PhaseResult {
        let host = match self.expected {
            Some(expected) => {
                if !self.done_server_list || self.world_host != expected.host {
                    return PhaseResult::Fatal("Expected world was not selected; refusing world reconnect".into());
                }
                expected.host.clone()
            }
            None if self.world_host.is_empty() => self.config.login_host.clone(),
            None => self.world_host.clone(),
        };
        PhaseResult::ReconnectWorld { host, port: self.config.world_port }
    }
}

// ── SoD login packet builders / parsers (pure, unit-tested) ────────────────────
//
// The SoD (RoF2) login uses the SAME packet layouts and DES-CBC zero-key encryption as the
// legacy Titanium login; only the response opcode numbers differ (see protocol.rs). Ground
// truth for these layouts: EQEmu loginserver/login_types.h (LoginBaseMessage,
// LoginBaseReplyMessage, PlayerLoginReply, ServerListReply) and loginserver/client.cpp +
// world_server_manager.cpp (byte-for-byte serialization). #404.

/// Build the OP_Login credential packet: a 10-byte unencrypted `LoginBaseMessage`
/// (sequence=3 "login", encrypt_type=2 "DES") followed by the DES-CBC(zero-key) encryption
/// of `"user\0pass\0"`, zero-padded to an 8-byte boundary (server rejects non-multiples of 8).
fn build_login_request(user: &str, pass: &str) -> Vec<u8> {
    let creds = format!("{user}\0{pass}\0");
    let padded_len = creds.len().div_ceil(8) * 8;
    let mut creds_bytes = creds.into_bytes();
    creds_bytes.resize(padded_len, 0);
    let encrypted = des_encrypt(&creds_bytes);
    // LoginBaseMessage: sequence=3 (i32 LE), compressed=0, encrypt_type=2, unk3=0 (i32 LE).
    let mut payload = vec![3u8, 0, 0, 0,  0,  2,  0, 0, 0, 0];
    payload.extend_from_slice(&encrypted);
    payload
}

/// Build the OP_PlayEverquestRequest packet: a 10-byte `LoginBaseMessage` (sequence=5)
/// followed by the u32 world server id to join.
fn build_play_everquest(server_id: u32) -> Vec<u8> {
    let mut payload = vec![5u8, 0, 0, 0,  0,  0,  0, 0, 0, 0];
    payload.extend_from_slice(&server_id.to_le_bytes());
    payload
}

/// Parse an OP_LoginAccepted payload. Returns `Some((lsid, session_key))` on success, or
/// `None` if the server explicitly rejected the credentials (decrypted `success` byte == 0).
///
/// Layout: `[LoginBaseMessage: 10 bytes][DES-CBC(zero-key) PlayerLoginReply]`. The decrypted
/// PlayerLoginReply (login_types.h) is packed: success@0, error_str_id@1..5, str[1]@5,
/// unk1@6, unk2@7, lsid@8..12, key[11]@12 (client reads to the NUL).
///
/// Malformed-but-not-rejected cases (too short to hold a reply) preserve the historical
/// permissive behavior: assume success with a placeholder (lsid=1, key="0"). Only an explicit
/// success=0 is treated as a rejection, so a real failure is never silently reported as success.
fn parse_login_accepted_payload(payload: &[u8]) -> Option<(i32, String)> {
    const PLACEHOLDER: (i32, &str) = (1, "0");
    if payload.len() < 10 { return Some((PLACEHOLDER.0, PLACEHOLDER.1.to_string())); }
    let encrypted = &payload[10..];
    if encrypted.is_empty() { return Some((PLACEHOLDER.0, PLACEHOLDER.1.to_string())); }
    let dec = des_decrypt(encrypted);
    if dec.len() < 12 { return Some((PLACEHOLDER.0, PLACEHOLDER.1.to_string())); }
    if dec[0] == 0 { return None; } // explicit rejection (success=0)
    let lsid = i32::from_le_bytes([dec[8], dec[9], dec[10], dec[11]]);
    let key_end = dec[12..].iter().position(|&b| b == 0)
        .map(|p| p + 12).unwrap_or(dec.len());
    let key = String::from_utf8_lossy(&dec[12..key_end]).to_string();
    let key = if key.is_empty() { "0".to_string() } else { key };
    Some((lsid, key))
}

/// Parse an OP_ServerListResponse payload. Returns `(world_server_id, world_host, world_name)`,
/// falling back to `fallback_host` when the advertised host is empty/`0.0.0.0`.
///
/// Layout (world_server_manager.cpp CreateServerListPacket): `LoginBaseMessage` (10) +
/// `LoginBaseReplyMessage` (success@10, error_str_id@11..15, empty str NUL@15) + server_count
/// (i32)@16 + entries. Each entry: `ip\0` + server_type(i32) + server_id(u32) + `name\0` + ...
/// (Titanium and SoD entries are identical — only cv_larion differs in world_server.cpp.)
fn parse_server_list_payload(payload: &[u8], fallback_host: &str) -> (u32, String, String) {
    let fb = || fallback_host.to_string();
    let mut offset = 16usize;
    if payload.len() < offset + 4 { offset = 15; } // defensive fallback for a shorter prefix
    if payload.len() < offset + 4 {
        return (1, fb(), String::new());
    }
    let count = i32::from_le_bytes([payload[offset], payload[offset+1], payload[offset+2], payload[offset+3]]);
    offset += 4;
    if count <= 0 || offset >= payload.len() {
        return (1, fb(), String::new());
    }

    let mut server_id = 0u32;
    let mut host = String::new();
    let mut name = String::new();
    // First entry: ip_str\0 + server_type(4) + server_id(4) + name_str\0 + ...
    if let Some(ip_end) = payload[offset..].iter().position(|&b| b == 0) {
        let world_host = String::from_utf8_lossy(&payload[offset..offset + ip_end]).to_string();
        offset += ip_end + 1;
        if offset + 8 <= payload.len() {
            server_id = u32::from_le_bytes([
                payload[offset+4], payload[offset+5], payload[offset+6], payload[offset+7],
            ]);
            offset += 8;
            let name_end = payload[offset..].iter().position(|&b| b == 0)
                .unwrap_or(payload.len() - offset);
            name = String::from_utf8_lossy(&payload[offset..offset + name_end]).to_string();
            host = if world_host.is_empty() || world_host == "0.0.0.0" { fb() } else { world_host };
        }
    }
    if server_id == 0 { server_id = 1; }
    if host.is_empty() { host = fb(); }
    (server_id, host, name)
}

/// Validate every advertised entry before accepting a unique exact name/host match.
/// Unlike the graphical compatibility parser, this never substitutes a host or server id.
fn select_expected_world(payload: &[u8], expected: &ExpectedWorld) -> Result<(u32, String, String), String> {
    const MALFORMED: &str = "Malformed or truncated world server list";

    fn take<'a>(remaining: &mut &'a [u8], len: usize) -> Result<&'a [u8], String> {
        let bytes = remaining.get(..len).ok_or(MALFORMED)?;
        *remaining = &remaining[len..];
        Ok(bytes)
    }

    fn read_u32(remaining: &mut &[u8]) -> Result<u32, String> {
        let bytes = take(remaining, 4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_string<'a>(remaining: &mut &'a [u8]) -> Result<&'a str, String> {
        let len = remaining.iter().position(|&byte| byte == 0).ok_or(MALFORMED)?;
        let bytes = take(remaining, len + 1)?;
        std::str::from_utf8(&bytes[..len]).map_err(|_| MALFORMED.to_string())
    }

    if expected.name.is_empty() || expected.host.is_empty() || expected.host == "0.0.0.0" {
        return Err("Expected world must have a name and an explicit host".into());
    }
    if payload.len() < 20 || payload[10] != 1 || payload[15] != 0 {
        return Err(MALFORMED.into());
    }
    let mut remaining = &payload[16..];
    let count = read_u32(&mut remaining)?;
    // Four string terminators and four u32 fields are the minimum entry size.
    // Bound the loop by actual bytes, including when the signed wire count is negative.
    if count > i32::MAX as u32 || count as usize > remaining.len() / 20 {
        return Err(MALFORMED.into());
    }
    let mut selected = None;
    let mut matches = 0;
    for _ in 0..count {
        let host = read_string(&mut remaining)?;
        let _server_type = read_u32(&mut remaining)?;
        let server_id = read_u32(&mut remaining)?;
        let name = read_string(&mut remaining)?;
        let _country = read_string(&mut remaining)?;
        let _language = read_string(&mut remaining)?;
        let _status = read_u32(&mut remaining)?;
        let _players_online = read_u32(&mut remaining)?;
        if name == expected.name && host == expected.host {
            matches += 1;
            selected = Some((server_id, host.to_string(), name.to_string()));
        }
    }
    if !remaining.is_empty() {
        return Err(MALFORMED.into());
    }
    match (matches, selected) {
        (1, Some((0, _, _))) => Err("Expected world advertised an invalid server id".into()),
        (1, Some(world)) => Ok(world),
        (0, _) => Err("Expected world name and host did not match any server list entry".into()),
        _ => Err("Expected world name and host matched multiple server list entries".into()),
    }
}

// ── Character-creation helpers ─────────────────────────────────────────────────

/// True if `name` appears as a null-terminated entry in an OP_SendCharInfo
/// payload (CharacterSelect_Struct holds up to 10 fixed-width name buffers).
/// Case-insensitive; the trailing NUL guards against prefix collisions.
fn char_in_list(payload: &[u8], name: &str) -> bool {
    let needle = name.as_bytes();
    if needle.is_empty() { return false; }
    payload.windows(needle.len() + 1)
        .any(|w| w[needle.len()] == 0 && w[..needle.len()].eq_ignore_ascii_case(needle))
}

/// Build the 72-byte NameApproval_Struct (OP_ApproveName): name[64] + race u32 + class u32.
fn build_approve_name(name: &str, race: u32, class: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 72];
    let nm = normalize_name(name);
    let b  = nm.as_bytes();
    let n  = b.len().min(63); // leave room for the NUL terminator
    buf[..n].copy_from_slice(&b[..n]);
    buf[64..68].copy_from_slice(&race.to_le_bytes());
    buf[68..72].copy_from_slice(&class.to_le_bytes());
    buf
}

/// Build the 96-byte RoF2 CharCreate_Struct (OP_CharacterCreate): 24 little-endian
/// u32 fields in wire order. No name field — the name was sent via OP_ApproveName.
fn build_char_create(cc: &CharacterCreate) -> Vec<u8> {
    // RoF2 CharCreate_Struct = 96 bytes (24 x u32). The server negotiates RoF2 (we parse RoF2
    // spawns/items) and rejects the old Titanium 80-byte layout for wrong size. Field order is
    // from EQEmu common/patches/rof2_structs.h CharCreate_Struct — DIFFERENT from Titanium:
    // drakkin_* and unknown0092 are new, and the stats move after the appearance block.
    let fields: [u32; 24] = [
        cc.gender,     // 0x00 gender
        cc.race,       // 0x04 race
        cc.class,      // 0x08 class_
        cc.deity,      // 0x0C deity
        cc.start_zone, // 0x10 start_zone — a ZONE_ID (not a Titanium StartZoneIndex). RoF2's
                       // CheckCharCreateInfoSoF matches this against char_create_combinations.start_zone
                       // (zone_ids); sending a 0..13 index is rejected → silent create loop. See eqoxide#5.
        cc.haircolor,  // 0x14 haircolor
        cc.beard,      // 0x18 beard
        cc.beardcolor, // 0x1C beardcolor
        cc.hairstyle,  // 0x20 hairstyle
        cc.face,       // 0x24 face
        cc.eyecolor1,  // 0x28 eyecolor1
        cc.eyecolor2,  // 0x2C eyecolor2
        0,             // 0x30 drakkin_heritage (non-Drakkin: 0)
        0,             // 0x34 drakkin_tattoo
        0,             // 0x38 drakkin_details
        cc.str_,       // 0x3C STR
        cc.sta,        // 0x40 STA
        cc.agi,        // 0x44 AGI
        cc.dex,        // 0x48 DEX
        cc.wis,        // 0x4C WIS
        cc.int_,       // 0x50 INT
        cc.cha,        // 0x54 CHA
        0,             // 0x58 tutorial (0 = normal)
        0,             // 0x5C unknown0092
    ];
    let mut buf = Vec::with_capacity(96);
    for f in fields { buf.extend_from_slice(&f.to_le_bytes()); }
    buf
}

#[cfg(test)]
mod charcreate_tests {
    use super::*;

    fn de_sk() -> CharacterCreate {
        // Dark Elf (6) Shadow Knight (5), validated stats summing to 582.
        // start_zone is a ZONE_ID: 42 = neriakc (a valid Dark Elf start city); a StartZoneIndex
        // such as 5 would be rejected by RoF2's CheckCharCreateInfoSoF. See eqoxide#5.
        CharacterCreate {
            race: 6, class: 5, gender: 0, deity: 206, start_zone: 42,
            str_: 70, sta: 70, agi: 90, dex: 75, wis: 83, int_: 129, cha: 65,
            face: 0, hairstyle: 0, haircolor: 0, beard: 0, beardcolor: 0,
            eyecolor1: 0, eyecolor2: 0,
        }
    }

    #[test]
    fn approve_name_layout() {
        let pkt = build_approve_name("mordeth", 6, 5);
        assert_eq!(pkt.len(), 72);
        assert_eq!(&pkt[..7], b"Mordeth");      // normalized capitalization
        assert_eq!(pkt[7], 0);                  // NUL-terminated
        assert_eq!(u32::from_le_bytes(pkt[64..68].try_into().unwrap()), 6);
        assert_eq!(u32::from_le_bytes(pkt[68..72].try_into().unwrap()), 5);
    }

    #[test]
    fn char_create_layout_and_stat_total() {
        // RoF2 layout: 96 bytes (24 x u32), order gender,race,class,deity,start_zone, hair/face
        // block, drakkin_*, then STR..CHA, tutorial, unknown0092.
        let cc = de_sk();
        let pkt = build_char_create(&cc);
        assert_eq!(pkt.len(), 96, "RoF2 CharCreate_Struct must be 96 bytes");
        let f = |i: usize| u32::from_le_bytes(pkt[i*4..i*4+4].try_into().unwrap());
        assert_eq!(f(0), 0);    // gender
        assert_eq!(f(1), 6);    // race (Dark Elf)
        assert_eq!(f(2), 5);    // class (Shadow Knight)
        assert_eq!(f(3), 206);  // deity (Innoruuk)
        assert_eq!(f(4), 42);   // start_zone = zone_id (42 = neriakc), NOT a StartZoneIndex
        assert_eq!(f(12), 0);   // drakkin_heritage (non-Drakkin)
        assert_eq!(f(22), 0);   // tutorial
        assert_eq!(f(23), 0);   // unknown0092
        // Stats occupy fields 15..=21 and must total exactly 582 for DE SK.
        let total: u32 = (15..=21).map(f).sum();
        assert_eq!(total, 582);
    }

    #[test]
    fn char_in_list_matches_exact_only() {
        // Two fixed-width name buffers: "Durgan\0..." then "Mordeth\0...".
        let mut payload = vec![0u8; 128];
        payload[..6].copy_from_slice(b"Durgan");
        payload[64..71].copy_from_slice(b"Mordeth");
        assert!(char_in_list(&payload, "Mordeth"));
        assert!(char_in_list(&payload, "mordeth"));   // case-insensitive
        assert!(char_in_list(&payload, "Durgan"));
        assert!(!char_in_list(&payload, "Mord"));     // prefix must not match
        assert!(!char_in_list(&payload, "Katie"));
    }

    #[test]
    fn login_error_from_string_is_retryable() {
        // The `?` operator on connection/timeout error sites yields String -> LoginError via this
        // From impl; it MUST map to Retryable so transient failures still retry. Only an explicit
        // PhaseResult::Fatal (server-rejected create) stops the loop. (eqoxide#6)
        let e: super::LoginError = "World connection failed".to_string().into();
        assert!(matches!(e, super::LoginError::Retryable(_)));
        assert_eq!(e.to_string(), "World connection failed");
        assert_eq!(super::LoginError::Fatal("rejected".into()).to_string(), "rejected");
    }
}

// ── SoD login handshake tests (#404) ──────────────────────────────────────────
// Exercise the SoD login packet builders/parsers against the exact byte layouts from EQEmu's
// loginserver source (login_opcodes_sod.conf, login_types.h, client.cpp,
// world_server_manager.cpp). Each test is written to go RED if the SoD format is reverted to
// Titanium or the field offsets drift. Live round-trip (server accepts the SoD handshake and
// hands off to world) is the orchestrator's build-and-verify gate.
#[cfg(test)]
mod liveness_stamp_tests {
    use super::record_login_liveness;
    use std::time::{Duration, Instant};

    /// Pins the #419 fix at the login call site: the handshake drain's liveness stamp must route
    /// through `record_app_packet` (the single canonical writer), which clears the wedge-streak clock
    /// `first_unanswered_probe_sent` in addition to bumping `last_packet`. We seed an outstanding
    /// streak (as if a probe were in flight), call the exact helper `run_login_phase` calls, and
    /// assert BOTH effects. Reverting `record_login_liveness`'s body to a raw
    /// `net_health.lock().unwrap().last_packet = now` write leaves the streak set → this goes RED,
    /// so a future revert of the fix cannot pass unnoticed (the defect the reviewer flagged).
    #[test]
    fn login_liveness_stamp_goes_through_canonical_recorder() {
        let base = Instant::now();
        let health = std::sync::Arc::new(std::sync::Mutex::new(eqoxide_ipc::NetHealth {
            last_datagram: base, last_packet: base, last_tick: base,
            last_probe_sent: Some(base), last_probe_reply: None,
            first_unanswered_probe_sent: Some(base),
            ..eqoxide_ipc::NetHealth::default()
        }));

        let stamp_at = base + Duration::from_secs(65);
        record_login_liveness(&health, stamp_at);

        let h = health.lock().unwrap();
        assert_eq!(h.last_packet, stamp_at, "the stamp must update last_packet");
        assert!(h.first_unanswered_probe_sent.is_none(),
            "the login liveness stamp must go through record_app_packet, which clears the wedge-streak \
             clock — a raw `last_packet = now` write would leave it set (the #419 seam)");
    }
}

#[cfg(test)]
mod sod_login_tests {
    use super::*;

    /// The migration itself: eqoxide must use the SoD (RoF2) login opcodes, NOT Titanium's.
    /// Mutation-check: revert any SoD value to its Titanium value (0x0017→0x0016, 0x0018→0x0017,
    /// 0x0019→0x0018, 0x0022→0x0021) and this goes RED.
    #[test]
    fn login_opcodes_are_sod_not_titanium() {
        // C→S request opcodes — identical in both listeners.
        assert_eq!(OP_SESSION_READY, 0x0001);
        assert_eq!(OP_LOGIN, 0x0002);
        assert_eq!(OP_SERVER_LIST_REQUEST, 0x0004);
        assert_eq!(OP_PLAY_EVERQUEST_REQ, 0x000d);
        // S→C response opcodes — SoD shifts each up from the Titanium value.
        assert_eq!(OP_CHAT_MESSAGE, 0x0017);          // Titanium 0x0016
        assert_eq!(OP_LOGIN_ACCEPTED, 0x0018);        // Titanium 0x0017
        assert_eq!(OP_SERVER_LIST_RESPONSE, 0x0019);  // Titanium 0x0018
        assert_eq!(OP_PLAY_EVERQUEST_RESP, 0x0022);   // Titanium 0x0021
        assert_eq!(OP_LOGIN_EXPANSION_PACKET_DATA, 0x0031); // SoD-only
    }

    /// OP_Login: 10-byte LoginBaseMessage (seq=3, encrypt_type=2 DES) + DES(zero-key) of
    /// "user\0pass\0" zero-padded to an 8-byte boundary. Verified by DES round-trip.
    #[test]
    fn login_request_header_and_credential_roundtrip() {
        let pkt = build_login_request("bob", "secret");
        // LoginBaseMessage: sequence=3, compressed=0, encrypt_type=2, unk3=0.
        assert_eq!(&pkt[..10], &[3u8, 0, 0, 0, 0, 2, 0, 0, 0, 0]);
        let enc = &pkt[10..];
        // "bob\0secret\0" = 11 bytes → padded to 16; server rejects non-multiples of 8.
        assert_eq!(enc.len(), 16);
        assert_eq!(enc.len() % 8, 0);
        let dec = des_decrypt(enc);
        assert_eq!(&dec[..11], b"bob\0secret\0");
    }

    /// OP_PlayEverquestRequest: 10-byte LoginBaseMessage (seq=5) + u32 world server id.
    #[test]
    fn play_everquest_request_layout() {
        let pkt = build_play_everquest(0x1234_5678);
        assert_eq!(pkt.len(), 14);
        assert_eq!(&pkt[..10], &[5u8, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(u32::from_le_bytes(pkt[10..14].try_into().unwrap()), 0x1234_5678);
    }

    // Build a wire-accurate OP_LoginAccepted payload: 10-byte LoginBaseMessage + DES(PlayerLoginReply).
    fn make_login_accepted(success: u8, lsid: i32, key: &[u8]) -> Vec<u8> {
        let mut plain = vec![0u8; 32]; // multiple of 8; big enough for lsid@8 + key@12
        plain[0] = success;                                  // base_reply.success
        plain[1..5].copy_from_slice(&101i32.to_le_bytes());  // base_reply.error_str_id
        // str[1]@5, unk1@6, unk2@7 stay 0
        plain[8..12].copy_from_slice(&lsid.to_le_bytes());   // lsid
        plain[12..12 + key.len()].copy_from_slice(key);      // key[11], NUL-terminated by zero fill
        let enc = des_encrypt(&plain);
        let mut payload = vec![0u8; 10];                     // LoginBaseMessage header
        payload.extend_from_slice(&enc);
        payload
    }

    #[test]
    fn login_accepted_parses_lsid_and_session_key() {
        let payload = make_login_accepted(1, 4242, b"ABCDE12345");
        let (lsid, key) = parse_login_accepted_payload(&payload).expect("valid reply accepted");
        assert_eq!(lsid, 4242);
        assert_eq!(key, "ABCDE12345");
    }

    #[test]
    fn login_accepted_rejects_on_success_zero() {
        // success byte == 0 is an explicit credential rejection; must NOT be reported as success
        // (agent-honesty: never turn a real failure into a confident false OK).
        let payload = make_login_accepted(0, 0, b"");
        assert!(parse_login_accepted_payload(&payload).is_none());
    }

    // Build a wire-accurate OP_ServerListResponse per world_server_manager.cpp CreateServerListPacket.
    fn make_server_list(ip: &[u8], server_type: i32, server_id: u32, name: &[u8], count: i32) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&4i32.to_le_bytes()); // LoginBaseMessage.sequence
        p.push(0);                                 // compressed
        p.push(0);                                 // encrypt_type
        p.extend_from_slice(&0i32.to_le_bytes());  // unk3  → 10-byte header
        p.push(1);                                 // base_reply.success @10
        p.extend_from_slice(&0x65i32.to_le_bytes());// base_reply.error_str_id @11
        p.push(0);                                 // empty str NUL @15
        p.extend_from_slice(&count.to_le_bytes()); // server_count @16
        // first entry
        p.extend_from_slice(ip);
        p.push(0);
        p.extend_from_slice(&server_type.to_le_bytes());
        p.extend_from_slice(&server_id.to_le_bytes());
        p.extend_from_slice(name);
        p.push(0);
        p.extend_from_slice(b"us\0");
        p.extend_from_slice(b"en\0");
        p.extend_from_slice(&0i32.to_le_bytes()); // status
        p.extend_from_slice(&3u32.to_le_bytes()); // players_online
        p
    }

    #[test]
    fn server_list_parses_first_entry() {
        let p = make_server_list(b"192.168.1.5", 1, 7, b"MyServer", 1);
        let (id, host, name) = parse_server_list_payload(&p, "fallback");
        assert_eq!(id, 7);
        assert_eq!(host, "192.168.1.5");
        assert_eq!(name, "MyServer");
    }

    #[test]
    fn server_list_uses_fallback_host_for_zero_ip() {
        let p = make_server_list(b"0.0.0.0", 1, 9, b"S", 1);
        let (id, host, _name) = parse_server_list_payload(&p, "myfallback");
        assert_eq!(id, 9);
        assert_eq!(host, "myfallback");
    }

    fn expected_world() -> ExpectedWorld {
        ExpectedWorld { name: "Bump Staging".into(), host: "192.168.1.220".into() }
    }

    fn login_config() -> LoginConfig {
        LoginConfig {
            login_host: "127.0.0.1".into(), login_port: 5999, world_port: 9000,
            username: "tester".into(), password: "test-only".into(),
            character_name: "Tester".into(), create: None,
        }
    }

    fn world_list(entries: &[(&[u8], u32, &[u8])]) -> Vec<u8> {
        let mut payload = make_server_list(b"", 1, 1, b"", entries.len() as i32);
        payload.truncate(20);
        for &(host, id, name) in entries {
            let entry = make_server_list(host, 1, id, name, 1);
            payload.extend_from_slice(&entry[20..]);
        }
        payload
    }

    #[tokio::test]
    async fn selected_world_matches_both_fields_across_all_entries_and_reconnects_to_expected_host() {
        let config = login_config();
        let expected = expected_world();
        let mut proto = LoginProtocol::new_selected(&config, &expected);
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let gs = GameState::new();
        let payload = world_list(&[
            (b"192.168.1.220", 7, b"Other World"),
            (b"192.168.1.220", 42, b"Bump Staging"),
            (b"192.168.1.221", 9, b"Bump Staging"),
        ]);
        assert!(matches!(proto.handle(
            &AppPacket { opcode: OP_SERVER_LIST_RESPONSE, payload }, &mut stream, &gs,
        ), PhaseResult::Continue));
        assert_eq!(proto.world_server_id, 42);
        assert_eq!(proto.world_host, expected.host);
        assert_eq!(stream.sent_app_packets(), vec![(OP_PLAY_EVERQUEST_REQ, build_play_everquest(42))]);
        match proto.handle(
            &AppPacket { opcode: OP_PLAY_EVERQUEST_RESP, payload: Vec::new() }, &mut stream, &gs,
        ) {
            PhaseResult::ReconnectWorld { host, port } => {
                assert_eq!(host, expected.host);
                assert_eq!(port, config.world_port);
            }
            _ => panic!("expected selected world reconnect"),
        }
    }

    #[tokio::test]
    async fn selected_world_handler_fails_before_sending_play_for_invalid_lists() {
        let config = login_config();
        let expected = expected_world();
        let gs = GameState::new();
        let duplicate = world_list(&[
            (b"192.168.1.220", 42, b"Bump Staging"),
            (b"192.168.1.220", 43, b"Bump Staging"),
        ]);
        let mut truncated = world_list(&[(b"192.168.1.220", 42, b"Bump Staging")]);
        truncated.pop();
        for payload in [Vec::new(), world_list(&[]), duplicate, truncated] {
            let mut proto = LoginProtocol::new_selected(&config, &expected);
            let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
            assert!(matches!(proto.handle(
                &AppPacket { opcode: OP_SERVER_LIST_RESPONSE, payload }, &mut stream, &gs,
            ), PhaseResult::Fatal(_)));
            assert!(!proto.done_server_list);
            assert!(proto.world_host.is_empty());
            assert!(stream.sent_app_packets().is_empty());
        }
    }

    #[tokio::test]
    async fn selected_world_preserves_normal_character_creation_handshake() {
        let mut config = login_config();
        config.create = Some(CharacterCreate { race: 6, class: 5, ..Default::default() });
        let expected = expected_world();
        let mut proto = LoginProtocol::new_selected(&config, &expected);
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let gs = GameState::new();
        for (opcode, payload) in [
            (OP_SEND_CHAR_INFO, Vec::new()),
            (OP_APPROVE_NAME, vec![1]),
            (OP_SEND_CHAR_INFO, b"Tester\0".to_vec()),
        ] {
            assert!(matches!(proto.handle(&AppPacket { opcode, payload }, &mut stream, &gs), PhaseResult::Continue));
        }
        let packets = stream.sent_app_packets();
        assert_eq!(packets.iter().map(|(opcode, _)| *opcode).collect::<Vec<_>>(),
            [OP_APPROVE_NAME, OP_CHARACTER_CREATE, OP_ENTER_WORLD, OP_POST_ENTER_WORLD]);
        assert_eq!(packets[1].1, build_char_create(config.create.as_ref().unwrap()));
        assert!(proto.done_char_info);
        assert!(!proto.awaiting_create_result);
    }

    #[test]
    fn selected_world_rejects_mismatches_without_host_fallback() {
        let expected = expected_world();
        for (host, name) in [
            (b"192.168.1.221".as_slice(), b"Bump Staging".as_slice()),
            (b"192.168.1.220", b"bump staging"),
            (b"192.168.1.220", b"Bump Staging Extra"),
            (b"", b"Bump Staging"),
            (b"0.0.0.0", b"Bump Staging"),
        ] {
            let payload = world_list(&[(host, 42, name)]);
            assert!(select_expected_world(&payload, &expected).unwrap_err().contains("did not match"));
        }
        assert!(select_expected_world(&world_list(&[]), &expected).unwrap_err().contains("did not match"));
    }

    #[test]
    fn selected_world_rejects_ambiguous_entries_even_with_identical_ids() {
        for second_id in [42, 43] {
            let payload = world_list(&[
                (b"192.168.1.220", 42, b"Bump Staging"),
                (b"192.168.1.220", second_id, b"Bump Staging"),
            ]);
            assert!(select_expected_world(&payload, &expected_world()).unwrap_err().contains("multiple"));
        }
    }

    #[test]
    fn selected_world_rejects_every_truncation_including_entries_after_a_match() {
        let expected = expected_world();
        let payload = world_list(&[
            (b"192.168.1.220", 42, b"Bump Staging"),
            (b"192.168.1.221", 7, b"Other World"),
        ]);
        assert!(select_expected_world(&payload, &expected).is_ok());
        for len in 0..payload.len() {
            assert!(select_expected_world(&payload[..len], &expected).is_err(), "accepted length {len}");
        }
    }

    #[test]
    fn selected_world_rejects_malformed_counts_strings_headers_and_trailing_bytes() {
        let expected = expected_world();
        let valid = world_list(&[(b"192.168.1.220", 42, b"Bump Staging")]);
        for count in [0i32, -1, 2, i32::MAX] {
            let mut payload = valid.clone();
            payload[16..20].copy_from_slice(&count.to_le_bytes());
            assert!(select_expected_world(&payload, &expected).is_err());
        }
        for index in [10, 15, 20, 42, valid.len() - 14] {
            let mut payload = valid.clone();
            payload[index] = 0xff;
            assert!(select_expected_world(&payload, &expected).is_err());
        }
        let mut payload = valid;
        payload.push(0);
        assert!(select_expected_world(&payload, &expected).is_err());
        assert!(select_expected_world(
            &world_list(&[(b"192.168.1.220", 0, b"Bump Staging")]), &expected,
        ).is_err());
    }

    #[test]
    fn selected_world_reconnect_requires_successful_selection_and_preserves_expected_host() {
        let config = login_config();
        let expected = expected_world();
        let mut proto = LoginProtocol::new_selected(&config, &expected);
        assert!(matches!(proto.world_reconnect(), PhaseResult::Fatal(_)));
        proto.world_host = expected.host.clone();
        assert!(matches!(proto.world_reconnect(), PhaseResult::Fatal(_)));
        proto.done_server_list = true;
        proto.world_host = config.login_host.clone();
        assert!(matches!(proto.world_reconnect(), PhaseResult::Fatal(_)));
    }

    #[test]
    fn graphical_login_keeps_first_entry_and_fallback_compatibility() {
        let config = login_config();
        let mut proto = LoginProtocol::new(&config);
        let payload = world_list(&[
            (b"0.0.0.0", 7, b"Other World"),
            (b"192.168.1.220", 42, b"Bump Staging"),
        ]);
        proto.parse_server_list(&payload).unwrap();
        assert_eq!(proto.world_server_id, 7);
        assert_eq!(proto.world_host, config.login_host);
        proto.parse_server_list(&[]).unwrap();
        assert_eq!(proto.world_server_id, 1);
        assert_eq!(proto.world_host, config.login_host);
        assert!(matches!(proto.world_reconnect(), PhaseResult::ReconnectWorld { host, .. }
            if host == config.login_host));
    }
}

// ── #1022: the login path's door publisher ────────────────────────────────────
#[cfg(test)]
mod login_door_publish_tests {
    use super::*;

    /// One RoF2 `OP_SpawnDoor` wire record. 100 bytes, NOT the 80-byte server struct — EQEmu's
    /// RoF2 patch re-serialises 1:1 into 100-byte records, and `apply_spawn_doors` strides by 100.
    /// Only the fields this test reads back are set; the rest stay zero.
    fn door_record(door_id: u8, name: &str) -> Vec<u8> {
        let mut r = vec![0u8; 100];
        r[..name.len()].copy_from_slice(name.as_bytes()); // name[32], NUL-padded
        r[60] = door_id;
        r[61] = 58; // opentype
        r
    }

    /// A datagram carrying one reliable application packet, framed exactly as the server frames it
    /// for a session that negotiated `crc_bytes = 0` and no encode passes (which is what the
    /// all-zero `OP_SessionResponse` body below negotiates):
    /// `[0x00, OP_PACKET, seq(BE u16), opcode(LE u16), payload…]`.
    fn reliable(seq: u16, opcode: u16, payload: &[u8]) -> Vec<u8> {
        let mut d = vec![0x00u8, OP_PACKET];
        d.extend_from_slice(&seq.to_be_bytes());
        d.extend_from_slice(&opcode.to_le_bytes());
        d.extend_from_slice(payload);
        d
    }

    /// #1022 — the door roster must be published from `run_login_phase`'s OWN drain, not only from
    /// `run_gameplay_phase`'s.
    ///
    /// # Why this test drives the real function over a real socket
    ///
    /// The defect is a REACH defect: the projection (`action_loop::publish_doors`) was already
    /// correct and already tested; what was missing was any call to it on the login path. A unit
    /// test of a helper would pin that the publish is *written*, which is exactly the kind of
    /// evidence #799 records eight measured evasions of. So this runs the production
    /// `run_login_phase` against a fake login server on loopback and observes `doors_shared` from
    /// outside it, while it is still running.
    ///
    /// The fake server needs to be almost nothing, because an all-zero `OP_SessionResponse` body
    /// negotiates `crc_bytes = 0`, `encode_pass1 = encode_pass2 = ENCODE_NONE` and
    /// `encode_key = 0` (`EqStream::handle_session_response`), so the door datagram is plaintext
    /// and unchecksummed. `OP_SPAWN_DOOR` (0x7291) is far outside the login opcode space
    /// (0x0001..0x0031), so `LoginProtocol::handle` takes its default arm and returns `Continue` —
    /// the login state machine keeps waiting for `OP_ChatMessage` and never completes, which is
    /// what lets this observe the MID-handshake window the defect lives in rather than the state
    /// after it.
    ///
    /// # MUTATION CHECK
    ///
    /// - DELETE `publish_doors(&gs, doors);` from the gated call in the drain loop → RED (the poll
    ///   below runs out its budget; nothing else on this path ever writes `doors_shared`).
    /// - WRAP it as `if std::hint::black_box(false) { publish_doors(&gs, doors); }` → also RED, by
    ///   the same timeout. Per #1016 review B3's retraction this is NOT independent evidence of the
    ///   DELETE result on a line that is already inside an `if` — `black_box(false)` simply never
    ///   takes the branch — but it is run and recorded because the WRAP is the mutation that
    ///   catches an inert fix, and "present but unreachable" is the failure mode this whole test
    ///   exists to exclude.
    ///
    /// The post-loop unconditional `publish_doors(&gs, doors);` (the `PhaseResult::Done` escape) is
    /// NOT covered here — reaching it requires driving the full login → world → zone handshake to
    /// completion, which needs a fake world server and a fake zone server, not just a fake login
    /// listener. That gap is stated in the PR rather than papered over.
    #[tokio::test]
    async fn run_login_phase_publishes_doors_applied_during_the_login_handshake_1022() {
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = server.local_addr().unwrap().port();

        let doors: eqoxide_ipc::DoorsShared = Default::default();
        let doors_bg = doors.clone();

        let config = LoginConfig {
            login_host:     "127.0.0.1".to_string(),
            login_port:     port,
            world_port:     port,
            username:       "tester".to_string(),
            password:       "secret".to_string(),
            character_name: "Tester".to_string(),
            create:         None,
        };

        let handle = tokio::spawn(async move {
            let _ = run_login_phase(
                &config,
                &eqoxide_ipc::NetHealthShared::default(),
                &eqoxide_ipc::ControllerSlots::default(),
                &doors_bg,
                None,
            ).await;
        });

        // 1. Answer the session handshake. `connect` re-sends OP_SessionRequest on its own retry
        //    cadence, so the first datagram to arrive is one of those; any of them will do.
        let mut buf = [0u8; 4096];
        let (_n, client_addr) = tokio::time::timeout(
            Duration::from_secs(10), server.recv_from(&mut buf),
        ).await.expect("no OP_SessionRequest from run_login_phase within 10s").unwrap();

        let mut session_response = vec![0x00u8, OP_SESSION_RESPONSE];
        session_response.extend_from_slice(&[0u8; 15]); // crc_bytes = 0, both encode passes NONE
        server.send_to(&session_response, client_addr).await.unwrap();

        // 2. Deliver one door record on the very first reliable sequence, exactly as EQEmu does
        //    during the connect phase, and let the drain apply it.
        server.send_to(&reliable(0, OP_SPAWN_DOOR, &door_record(66, "CRATEB")), client_addr)
              .await.unwrap();

        // 3. Observe the PUBLISHED roster from outside, while `run_login_phase` is still running.
        //    `LoginProtocol` times out at 120s and this test never lets it get near that; the
        //    budget below is what fails, promptly, if nothing publishes.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            {
                let published = doors.lock().unwrap();
                if let Some(d) = published.first() {
                    assert_eq!(d.door_id, 66);
                    assert_eq!(d.name, "CRATEB");
                    assert_eq!(published.len(), 1);
                    break;
                }
            }
            assert!(std::time::Instant::now() < deadline,
                "an OP_SpawnDoor applied during the LOGIN handshake was never published to \
                 `doors_shared` — GET /v1/observe/doors serves that slot verbatim, so it answers a \
                 fresh, well-formed, confident `[]` for a zone whose doors this client has already \
                 parsed into `gs.world.doors`, indistinguishable from a genuinely doorless zone, \
                 and POST /v1/interact/click_door answers the empty-roster 404 for a door it knows \
                 about (#1022)");
            sleep(Duration::from_millis(10)).await;
        }

        handle.abort();
    }

    /// #1022, the #1016-review-B4 shape arriving on this path with the publisher: a login attempt
    /// that PUBLISHED a door roster and then FAILED must not leave that roster standing.
    ///
    /// Failure is driven the fastest honest way this state machine offers — an `OP_LoginAccepted`
    /// whose decrypted `success` byte is 0, which `parse_login_accepted_payload` reads as an
    /// explicit credential rejection (never as an OK — see
    /// `login_accepted_rejects_on_success_zero`), giving `PhaseResult::Error` and a `return Err`
    /// out of `run_login_handshake`. That is one of the six error exits the wrapper funnels.
    ///
    /// The premise is OBSERVED, not assumed: the roster is polled to NON-EMPTY before the rejection
    /// is sent. Without that step this test would pass identically if the door had never been
    /// published at all, which is the exact non-vacuity defect #1016's round-2 reviewer found in
    /// `zone_entry_handshake_clears_published_doors_on_timeout_after_one_arrived_1016`.
    ///
    /// MUTATION CHECK: DELETE `doors.lock().unwrap().clear();` from `run_login_phase`'s wrapper →
    /// RED (the roster still holds door 66 after the attempt returned Err). WRAP it as
    /// `if std::hint::black_box(false) { doors.lock().unwrap().clear(); }` → also RED.
    #[tokio::test]
    async fn run_login_phase_clears_the_roster_it_published_when_the_attempt_fails_1022() {
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = server.local_addr().unwrap().port();

        let doors: eqoxide_ipc::DoorsShared = Default::default();
        let doors_bg = doors.clone();

        let config = LoginConfig {
            login_host:     "127.0.0.1".to_string(),
            login_port:     port,
            world_port:     port,
            username:       "tester".to_string(),
            password:       "secret".to_string(),
            character_name: "Tester".to_string(),
            create:         None,
        };

        let handle = tokio::spawn(async move {
            run_login_phase(
                &config,
                &eqoxide_ipc::NetHealthShared::default(),
                &eqoxide_ipc::ControllerSlots::default(),
                &doors_bg,
                None,
            ).await.err().map(|e| e.to_string())
        });

        let mut buf = [0u8; 4096];
        let (_n, client_addr) = tokio::time::timeout(
            Duration::from_secs(10), server.recv_from(&mut buf),
        ).await.expect("no OP_SessionRequest from run_login_phase within 10s").unwrap();

        let mut session_response = vec![0x00u8, OP_SESSION_RESPONSE];
        session_response.extend_from_slice(&[0u8; 15]);
        server.send_to(&session_response, client_addr).await.unwrap();
        server.send_to(&reliable(0, OP_SPAWN_DOOR, &door_record(66, "CRATEB")), client_addr)
              .await.unwrap();

        // PREMISE, observed: the roster really did get published before the failure.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while doors.lock().unwrap().is_empty() {
            assert!(std::time::Instant::now() < deadline,
                "the door never reached `doors_shared`, so this test's premise — that a FAILING \
                 attempt had something published to leave behind — was never established (#1022)");
            sleep(Duration::from_millis(10)).await;
        }

        // Now reject the credentials: 10-byte LoginBaseMessage + DES(plain) with plain[0] == 0.
        let mut rejected = vec![0u8; 10];
        rejected.extend_from_slice(&des_encrypt(&[0u8; 32]));
        server.send_to(&reliable(1, OP_LOGIN_ACCEPTED, &rejected), client_addr).await.unwrap();

        let err = tokio::time::timeout(Duration::from_secs(10), handle)
            .await.expect("run_login_phase did not return within 10s of the credential rejection")
            .unwrap();
        assert_eq!(err.as_deref(), Some("Login credentials rejected"),
            "the attempt must fail for the reason this test drives, not some other one");

        assert!(doors.lock().unwrap().is_empty(),
            "a FAILED login attempt left the door roster it had published standing — GET \
             /v1/observe/doors then serves a partial, well-formed roster of a zone this client \
             never entered, and POST /v1/interact/click_door resolves its ids, for the whole \
             backoff and the entire next attempt (#1022; the #1016 review B4 shape on this path)");
    }
}
