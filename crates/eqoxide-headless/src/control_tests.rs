use super::*;
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use tower::ServiceExt;

fn control(capacity: usize, events: usize) -> Control {
    let control = Control::new(
        "session-1".into(),
        "private-test-token".into(),
        capacity,
        events,
    )
    .unwrap();
    control
        .publish_state(4, json!({"phase":"ready","inventory":{"revision":7}}))
        .unwrap();
    control.set_ready(true);
    control
}

fn request(operation: Value) -> Value {
    json!({"session":"session-1","zone_epoch":4,"operation":operation})
}

fn movement() -> Value {
    request(json!({"kind":"move_for","east":1,"north":0,"duration_ms":300}))
}

async fn http(
    control: &Control,
    method: &str,
    path: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        builder = builder.header("authorization", token);
    }
    let body = match body {
        Some(body) => {
            builder = builder.header("content-type", "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let response = control
        .router()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn authorized(
    control: &Control,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    http(
        control,
        method,
        path,
        body,
        Some("Bearer private-test-token"),
    )
    .await
}

#[tokio::test]
async fn all_endpoints_require_exact_bearer_authentication() {
    let control = control(10, 10);
    for path in [
        "/v2/player/state",
        "/v2/player/events",
        "/v2/player/packets",
        "/v2/player/commands/id",
    ] {
        for token in [None, Some("Bearer wrong"), Some("private-test-token")] {
            assert_eq!(
                http(&control, "GET", path, None, token).await.0,
                StatusCode::UNAUTHORIZED
            );
        }
    }
    assert_eq!(
        http(
            &control,
            "PUT",
            "/v2/player/commands/id",
            Some(movement()),
            None
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert!(control.drain_actions().is_empty());
    assert_eq!(
        authorized(&control, "GET", "/v2/player/state", None)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        authorized(&control, "GET", "/v2/player/commands/missing", None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn lost_response_retry_returns_receipt_without_dispatching_or_renewing_motion() {
    let control = control(10, 20);
    let path = "/v2/player/commands/walk-1";
    assert_eq!(
        authorized(&control, "PUT", path, Some(movement())).await.0,
        StatusCode::ACCEPTED
    );
    let actions = control.drain_actions();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].id, "walk-1");
    assert!(matches!(
        actions[0].request.operation,
        Operation::MoveFor {
            duration_ms: 300,
            ..
        }
    ));
    control
        .publish_outcome(
            "walk-1",
            CommandStatus::AppliedLocal,
            None,
            Some(json!({"kind":"physical_controller_armed","deadline_ms":300})),
        )
        .unwrap();
    let (status, mut body) = authorized(&control, "PUT", path, Some(movement())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "applied_local");
    assert!(body["event_cursor"].is_string(), "a PUT reports the cursor after admission");
    assert!(control.drain_actions().is_empty());
    body.as_object_mut().unwrap().remove("event_cursor");
    assert_eq!(authorized(&control, "GET", path, None).await.1, body);
    let mut changed = movement();
    changed["operation"]["duration_ms"] = json!(301);
    assert_eq!(
        authorized(&control, "PUT", path, Some(changed)).await.0,
        StatusCode::CONFLICT
    );
    // These numbers round to the same f32. Identity comparison must precede conversion.
    let mut changed = movement();
    changed["operation"]["east"] = json!(0.99999999999_f64);
    assert_eq!(
        authorized(&control, "PUT", path, Some(changed)).await.0,
        StatusCode::CONFLICT
    );
}

/// central-6ly1.1: terminal receipts are evicted past the retention bound, but an evicted ID is
/// never reusable: a replay answers 410 with its final status, a changed payload 409.
#[tokio::test]
async fn terminal_receipts_are_evicted_but_their_ids_never_return() {
    let control = control(2, 50);
    for id in ["a", "b", "c"] {
        control.admit(id.into(), movement()).unwrap();
        control.drain_actions();
        control
            .publish_outcome(id, CommandStatus::AppliedLocal, None, Some(json!({"kind":"controller_intent"})))
            .unwrap();
    }
    assert!(control.command("a").is_none(), "the oldest terminal receipt was evicted");
    assert!(control.command("c").is_some());
    let (status, body) = authorized(&control, "GET", "/v2/player/commands/a", None).await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(body, json!({"error":"command_id_expired","status":"applied_local","reason":null}));
    let (status, _) = authorized(&control, "PUT", "/v2/player/commands/a", Some(movement())).await;
    assert_eq!(status, StatusCode::GONE, "a replay of an expired ID never runs again");
    assert!(control.drain_actions().is_empty());
    let mut changed = movement();
    changed["operation"]["duration_ms"] = json!(301);
    let (status, body) = authorized(&control, "PUT", "/v2/player/commands/a", Some(changed)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::CONFLICT, Some("command_id_reused_with_changed_payload")));
}

/// central-6ly1.1: a refusal at admission consumes an identity but no live slot; once the
/// identity space is gone a 503 consumes nothing, and each safety verb has its own reserve.
/// central-6ly1.7: a readiness drop or zone change fences only live receipts; an earlier
/// `applied_local` stays exactly as it was.
#[test]
fn fences_never_rewrite_terminal_receipts() {
    let control = control(10, 50);
    control.admit("walk".into(), movement()).unwrap();
    control.drain_actions();
    control
        .publish_outcome("walk", CommandStatus::AppliedLocal, None, Some(json!({"kind":"controller_intent"})))
        .unwrap();
    let before = control.command("walk").unwrap();
    control.set_ready(false);
    control.publish_state(5, json!({})).unwrap();
    assert_eq!(control.command("walk").unwrap(), before);
}

#[test]
fn identity_space_and_safety_reserves() {
    let control = control(2, 500); // 32 identities, a reserve of one per safety verb
    for n in 0..32 {
        let mut stale = movement();
        stale["session"] = json!("someone-else");
        let (code, receipt, _) = control.admit(format!("r{n}"), stale).unwrap();
        assert_eq!((code, receipt.reason.as_deref(), receipt.admitted), (StatusCode::CONFLICT, Some("stale_session"), Some(false)));
    }
    let full = control.admit("next".into(), movement()).unwrap_err();
    assert_eq!((full.0, full.1.as_str()), (StatusCode::SERVICE_UNAVAILABLE, "session_id_space_exhausted"));
    let again = control.admit("next".into(), movement()).unwrap_err();
    assert_eq!(again.0, StatusCode::SERVICE_UNAVAILABLE, "the 503 consumed no identity");
    assert_eq!(control.admit("stop".into(), request(json!({"kind":"stop"}))).unwrap().0, StatusCode::ACCEPTED);
    assert_eq!(control.admit("off".into(), request(json!({"kind":"attack_off"}))).unwrap().0, StatusCode::ACCEPTED);
    let spent = control.admit("stop-2".into(), request(json!({"kind":"stop"}))).unwrap_err();
    assert_eq!((spent.0, spent.1.as_str()), (StatusCode::SERVICE_UNAVAILABLE, "safety_reserve_exhausted"));
    assert_eq!(control.admit("stop".into(), request(json!({"kind":"stop"}))).unwrap().0, StatusCode::OK,
        "a known ID is still answered when every limit is reached");
}

#[test]
fn busy_refusal_is_retained_and_priority_commands_bypass_busy() {
    let control = control(10, 20);
    control
        .admit("chat".into(), request(json!({"kind":"say","text":"hello"})))
        .unwrap();
    let (_, receipt, _) = control
        .admit("sit".into(), request(json!({"kind":"sit"})))
        .unwrap();
    assert_eq!(receipt.status, CommandStatus::Refused);
    assert_eq!(receipt.reason.as_deref(), Some("busy"));
    control
        .admit("off".into(), request(json!({"kind":"attack_off"})))
        .unwrap();
    let actions = control.drain_actions();
    assert_eq!(
        actions.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
        vec!["off", "chat"]
    );
    control
        .publish_outcome(
            "chat",
            CommandStatus::Sent,
            None,
            Some(json!({"kind":"network_queue_accepted"})),
        )
        .unwrap();
    assert_eq!(
        control
            .admit("stand".into(), request(json!({"kind":"stand"})))
            .unwrap()
            .1
            .reason
            .as_deref(),
        Some("busy")
    );
    control
        .publish_outcome(
            "chat",
            CommandStatus::Unknown,
            Some("no_delivery_receipt".into()),
            None,
        )
        .unwrap();
    assert_eq!(
        control
            .admit("sit".into(), request(json!({"kind":"sit"})))
            .unwrap()
            .1
            .status,
        CommandStatus::Refused
    );
    assert_eq!(
        control
            .admit("stand-new".into(), request(json!({"kind":"stand"})))
            .unwrap()
            .1
            .status,
        CommandStatus::Accepted
    );
}

#[test]
fn session_and_epoch_fences_apply_before_any_new_dispatch() {
    let control = control(20, 30);
    let mut stale_session = movement();
    stale_session["session"] = json!("old-process");
    assert_eq!(
        control
            .admit("old-session".into(), stale_session)
            .unwrap()
            .1
            .reason
            .as_deref(),
        Some("stale_session")
    );
    let mut stale_zone = movement();
    stale_zone["zone_epoch"] = json!(3);
    assert_eq!(
        control
            .admit("old-zone".into(), stale_zone)
            .unwrap()
            .1
            .reason
            .as_deref(),
        Some("stale_zone_epoch")
    );
    assert!(control.drain_actions().is_empty());
    control.admit("queued".into(), movement()).unwrap();
    control.publish_state(5, json!({"phase":"zoning"})).unwrap();
    assert!(control.drain_actions().is_empty());
    assert_eq!(
        control.command("queued").unwrap().status,
        CommandStatus::Refused
    );
    // Retained old identity remains a replay, even though its epoch is now stale.
    assert_eq!(
        control.admit("queued".into(), movement()).unwrap().1.status,
        CommandStatus::Refused
    );
    assert!(control.publish_state(4, json!({})).is_err());
    let mut current = movement();
    current["zone_epoch"] = json!(5);
    control.admit("sent".into(), current).unwrap();
    control.drain_actions();
    control
        .publish_outcome(
            "sent",
            CommandStatus::Sent,
            None,
            Some(json!({"kind":"packet_enqueued"})),
        )
        .unwrap();
    control.publish_state(6, json!({"phase":"zoning"})).unwrap();
    assert_eq!(
        control.command("sent").unwrap().status,
        CommandStatus::Unknown
    );
}

#[test]
fn lost_readiness_fences_work_but_still_admits_safety_commands() {
    let control = control(10, 20);
    control.admit("move".into(), movement()).unwrap();
    control.set_ready(false);
    assert!(control.drain_actions().is_empty());
    assert_eq!(
        control
            .admit("sit".into(), request(json!({"kind":"sit"})))
            .unwrap()
            .1
            .reason
            .as_deref(),
        Some("not_ready")
    );
    assert_eq!(
        control
            .admit("stop".into(), request(json!({"kind":"stop"})))
            .unwrap()
            .1
            .status,
        CommandStatus::Accepted
    );
}

#[test]
fn outcomes_require_evidence_and_cannot_invent_or_regress_success() {
    let control = control(10, 20);
    control.admit("move".into(), movement()).unwrap();
    for status in [
        CommandStatus::Sent,
        CommandStatus::AppliedLocal,
        CommandStatus::Confirmed,
    ] {
        assert!(control.publish_outcome("move", status, None, None).is_err());
        assert!(control
            .publish_outcome("move", status, None, Some(json!({})))
            .is_err());
    }
    assert!(control
        .publish_outcome("move", CommandStatus::Unknown, None, None)
        .is_err());
    assert_eq!(
        control.command("move").unwrap().status,
        CommandStatus::Accepted
    );
    control
        .publish_outcome(
            "move",
            CommandStatus::AppliedLocal,
            None,
            Some(json!({"kind":"physical_controller_armed"})),
        )
        .unwrap();
    assert!(control
        .publish_outcome(
            "move",
            CommandStatus::Sent,
            None,
            Some(json!({"kind":"queue_accepted"}))
        )
        .is_err());
    // A terminal receipt never changes (central-6ly1.7): not to unknown, not to confirmed.
    assert!(control
        .publish_outcome("move", CommandStatus::Unknown, Some("connection_lost".into()), None)
        .is_err());
    assert!(control
        .publish_outcome("move", CommandStatus::Confirmed, None, Some(json!({"kind":"server_position_update"})))
        .is_err());
    let receipt = control.command("move").unwrap();
    assert_eq!((receipt.status, receipt.terminal), (CommandStatus::AppliedLocal, true));
}

#[test]
fn validation_rejects_unbounded_actions_and_bad_payloads() {
    let invalid = [
        json!({"kind":"move_for","east":1,"north":0,"duration_ms":0}),
        json!({"kind":"move_for","east":1,"north":0,"duration_ms":2001}),
        json!({"kind":"move_for","east":2,"north":0,"duration_ms":100}),
        json!({"kind":"attack","lease_ms":0}),
        json!({"kind":"attack","lease_ms":5001}),
        json!({"kind":"buy","slot":1,"quantity":2}),
        json!({"kind":"sell","slot":1,"quantity":0}),
        json!({"kind":"inventory_move","from_slot":1,"to_slot":1,"quantity":1}),
        json!({"kind":"say","text":"bad\ntext"}),
        json!({"kind":"tell","recipient":"a b","text":"hello"}),
        json!({"kind":"target","entity_id":0}),
    ];
    let control = control(30, 30);
    for (i, operation) in invalid.into_iter().enumerate() {
        assert_eq!(
            control
                .admit(format!("bad-{i}"), request(operation))
                .unwrap()
                .1
                .status,
            CommandStatus::Refused
        );
    }
    assert!(control.drain_actions().is_empty());
    assert!(Operation::MoveFor {
        east: f32::NAN,
        north: 0.0,
        duration_ms: 100
    }
    .validate()
    .is_err());
    assert!(Operation::MoveFor {
        east: 0.0,
        north: f32::INFINITY,
        duration_ms: 100
    }
    .validate()
    .is_err());
    assert!(Operation::MoveFor {
        east: 1.0,
        north: 1.0,
        duration_ms: 2000
    }
    .validate()
    .is_ok());
    assert!(Operation::Attack { lease_ms: 5000 }.validate().is_ok());
    let mut unknown = movement();
    unknown["operation"]["extra"] = json!(true);
    assert_eq!(
        control.admit("unknown".into(), unknown).unwrap_err().0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[test]
fn revision_fences_are_preserved_for_host_admission_and_command_identity() {
    let control = control(10, 10);
    let mut buy = request(json!({"kind":"buy","slot":4,"quantity":1}));
    buy["inventory_revision"] = json!(7);
    buy["merchant_revision"] = json!(9);
    control.admit("buy".into(), buy.clone()).unwrap();
    let action = control.drain_actions().pop().unwrap();
    assert_eq!(action.request.inventory_revision, Some(7));
    assert_eq!(action.request.merchant_revision, Some(9));
    buy["merchant_revision"] = json!(10);
    assert_eq!(
        control.admit("buy".into(), buy).unwrap_err().0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn event_gaps_and_process_identity_are_explicit() {
    let control = control(10, 2);
    control.publish_event(json!({"kind":"chat","text":"zeroth"}));
    control.publish_event(json!({"kind":"chat","text":"first"}));
    control.publish_event(json!({"kind":"chat","text":"second"}));
    let (status, batch) =
        authorized(&control, "GET", "/v2/player/events?after=session-1:0", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(batch["gap"], true);
    assert_eq!(batch["events"].as_array().unwrap().len(), 2);
    assert_eq!(batch["cursor"], "session-1:3");
    assert_eq!(batch["events"][0]["event"]["text"], "first");
    assert!(batch["events"][0]["at_ms"].is_u64(), "each event carries the host clock");
    let state = authorized(&control, "GET", "/v2/player/state", None)
        .await
        .1;
    assert_eq!(state["session"], "session-1");
    assert_eq!(state["zone_epoch"], 4);
    assert_eq!(state["event_cursor"], batch["cursor"]);
    for (query, code) in [
        ("after=prior-process:3", StatusCode::CONFLICT),
        ("after=session-1:999", StatusCode::BAD_REQUEST),
        ("after=bad", StatusCode::BAD_REQUEST),
        ("wait=31", StatusCode::BAD_REQUEST),
        ("wait=-1", StatusCode::BAD_REQUEST),
    ] {
        assert_eq!(
            authorized(&control, "GET", &format!("/v2/player/events?{query}"), None)
                .await
                .0,
            code
        );
    }
}

#[tokio::test]
async fn long_poll_ignores_state_publication_and_wakes_for_events() {
    let control = control(10, 10);
    // central-6ly1.1: state publication is not an event. An idle poll waits its full timeout.
    let started = std::time::Instant::now();
    let idle = control.clone();
    let task = tokio::spawn(async move {
        authorized(&idle, "GET", "/v2/player/events?after=session-1:0&wait=0.3", None).await
    });
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        control.publish_state(4, json!({"phase":"ready"})).unwrap();
    }
    let (_, batch) = task.await.unwrap();
    assert!(batch["events"].as_array().unwrap().is_empty(), "no state_changed events");
    assert!(started.elapsed() >= Duration::from_millis(300), "the poll waited its full timeout");
    let waiting = control.clone();
    let task = tokio::spawn(async move {
        authorized(
            &waiting,
            "GET",
            "/v2/player/events?after=session-1:0&wait=1",
            None,
        )
        .await
    });
    tokio::task::yield_now().await;
    control.publish_event(json!({"kind":"received_chat","text":"nonce"}));
    let (_, batch) = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(batch["events"][0]["event"]["text"], "nonce");
    assert_eq!(batch["gap"], false);
    let (_, batch) = authorized(
        &control,
        "GET",
        "/v2/player/events?after=session-1:1&wait=0.001",
        None,
    )
    .await;
    assert!(batch["events"].as_array().unwrap().is_empty());
    assert_eq!(batch["cursor"], "session-1:1");
}

#[tokio::test]
async fn simultaneous_same_id_admission_enqueues_once() {
    let control = control(10, 10);
    let first = authorized(
        &control,
        "PUT",
        "/v2/player/commands/concurrent",
        Some(movement()),
    );
    let second = authorized(
        &control,
        "PUT",
        "/v2/player/commands/concurrent",
        Some(movement()),
    );
    let (first, second) = tokio::join!(first, second);
    let mut codes = vec![first.0.as_u16(), second.0.as_u16()];
    codes.sort();
    assert_eq!(codes, vec![200, 202]);
    assert_eq!(control.drain_actions().len(), 1);
}

#[tokio::test]
async fn fence_after_dispatch_is_unknown_and_a_late_receipt_cannot_change_it() {
    let control = control(8, 16);
    let body = request(json!({"kind":"buy","slot":1,"quantity":1}));
    let (_, receipt) = authorized(&control, "PUT", "/v2/player/commands/buy-1", Some(body)).await;
    assert_eq!(receipt["status"], "accepted");
    control.drain_actions();
    control.mark_dispatched("buy-1").unwrap();
    control.set_ready(false);
    assert_eq!(
        control.command("buy-1").unwrap().status,
        CommandStatus::Unknown
    );
    // central-6ly1.7: the fenced receipt is terminal; late evidence cannot rewrite it.
    assert!(control
        .publish_outcome(
            "buy-1",
            CommandStatus::Confirmed,
            None,
            Some(json!({"kind":"server_buy_receipt"})),
        )
        .is_err());
    assert_eq!(
        control.command("buy-1").unwrap().status,
        CommandStatus::Unknown
    );
}

#[tokio::test]
async fn safety_supersession_does_not_promise_a_dispatched_attack_was_refused() {
    let control = control(8, 16);
    authorized(
        &control,
        "PUT",
        "/v2/player/commands/attack",
        Some(request(json!({"kind":"attack","lease_ms":1000}))),
    )
    .await;
    control.drain_actions();
    control.mark_dispatched("attack").unwrap();
    authorized(
        &control,
        "PUT",
        "/v2/player/commands/off",
        Some(request(json!({"kind":"attack_off"}))),
    )
    .await;
    assert_eq!(
        control.command("attack").unwrap().status,
        CommandStatus::Unknown
    );
}

#[tokio::test]
async fn packet_trace_orders_and_filters_decoded_packets() {
    use eqoxide_net::{
        action_loop::encode_client_position_update,
        packet_telemetry::Dir,
        protocol::{OP_CLIENT_UPDATE, OP_DAMAGE, OP_TARGET_COMMAND, OP_TARGET_MOUSE},
    };

    let now = Instant::now();
    let trace = SessionTrace::new(now - Duration::from_millis(42));
    let control = control(10, 10).with_packet_trace(trace.clone());

    let position = encode_client_position_update(0x9a02, 12, [15.0, -20.0, 0.0], [1.0, 2.0, 3.0], 400, 17);
    trace.capture(Dir::Out, OP_CLIENT_UPDATE, &position, None, None);
    trace.capture(Dir::Out, OP_TARGET_MOUSE, &41u32.to_le_bytes(), Some(7), Some("target-a".into()));
    trace.capture(Dir::Out, OP_TARGET_MOUSE, &42u32.to_le_bytes(), Some(8), Some("target-b".into()));
    trace.capture(Dir::Out, OP_TARGET_COMMAND, &43u32.to_le_bytes(), Some(9), Some("target-b".into()));
    let mut damage = [0; 13];
    damage[0..2].copy_from_slice(&12u16.to_le_bytes());
    damage[2..4].copy_from_slice(&42u16.to_le_bytes());
    damage[5..9].copy_from_slice(&21u32.to_le_bytes());
    damage[9..13].copy_from_slice(&(-5i32).to_le_bytes());
    trace.capture(Dir::In, OP_DAMAGE, &damage, Some(4), None);
    trace.capture(Dir::In, OP_TARGET_COMMAND, &0u32.to_le_bytes(), Some(5), None);
    trace.record_send(false);
    trace.record_send(true);
    trace.record_receive(Instant::now());
    trace.record_sequence_gap();
    trace.record_drop(5);

    let (status, first) = authorized(&control, "GET", "/v2/player/packets?limit=2", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["next"], 2);
    assert_eq!(first["gap"], false);
    let pos = &first["packets"][0];
    assert_eq!(pos["n"], 1);
    assert!(pos["at_ms"].as_u64().unwrap() >= 42);
    assert_eq!(pos["opcode"], OP_CLIENT_UPDATE);
    assert_eq!(pos["dir"], "out");
    assert_eq!(pos["len"], 48);
    assert_eq!(pos["reliable"], false);
    assert_eq!(pos["wire_seq"], Value::Null);
    assert_eq!(pos["command_id"], Value::Null);
    assert_eq!(pos["fields"], json!({
        "sequence": 0x9a02,
        "position": [15.0, -20.0, eqoxide_core::coord::WIRE_Z_OFFSET],
        "delta": [1.0, 2.0, 3.0], "animation": 17, "heading": 400,
    }));
    assert_eq!(first["packets"][1]["n"], 2);

    let (_, rest) = authorized(&control, "GET", "/v2/player/packets?after=2", None).await;
    let packets = rest["packets"].as_array().unwrap();
    assert_eq!(packets.iter().map(|p| p["n"].as_u64().unwrap()).collect::<Vec<_>>(), [3, 4, 5, 6]);
    assert_eq!(rest["next"], 6);
    assert_eq!(packets[2]["dir"], "in");
    assert_eq!(packets[2]["command_id"], Value::Null);
    assert_eq!(packets[2]["fields"], json!({"source":42, "target":12, "type":0, "damage":-5, "spell":21}));
    assert_eq!(packets[3]["fields"], json!({"target_id":0}));
    assert_eq!(rest["counters"], json!({
        "sent":2, "resent":1, "received":1, "dropped_unretried":5, "sequence_gaps":1,
    }));

    let (_, selected) = authorized(
        &control, "GET", "/v2/player/packets?opcode=OP_TARGET_MOUSE&command=target-b", None,
    ).await;
    assert_eq!(selected["next"], 6);
    let packets = selected["packets"].as_array().unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0], json!({
        "n":3, "at_ms":rest["packets"][0]["at_ms"], "dir":"out",
        "opcode":OP_TARGET_MOUSE, "opcode_name":"OP_TARGET_MOUSE", "len":6, "reliable":true,
        "wire_seq":8, "arrival_n":null, "arrived_at_ms":null, "acked_at_ms":null,
        "ack_seq":null, "ack_exact":false, "payload_truncated":false, "command_id":"target-b", "fields":{"target_id":42},
    }));
}

#[tokio::test]
async fn packet_trace_gaps_follow_eviction_even_with_filters() {
    use eqoxide_net::packet_telemetry::Dir;

    let control = control(1, 1);
    for _ in 0..20_002 {
        control.packet_trace.capture(Dir::In, 0xfffe, &[], None, None);
    }
    let (_, page) = authorized(&control, "GET", "/v2/player/packets?after=1&limit=2", None).await;
    assert_eq!(page["gap"], true);
    assert_eq!(page["packets"][0]["n"], 3);
    assert_eq!(page["packets"][0]["fields"], json!({}));
    assert_eq!(page["packets"][1]["n"], 4);
    assert_eq!(page["next"], 4);

    let (_, page) = authorized(&control, "GET", "/v2/player/packets?after=2&limit=1", None).await;
    assert_eq!(page["gap"], false);
    assert_eq!(page["next"], 3);

    let (_, page) = authorized(&control, "GET", "/v2/player/packets?command=missing", None).await;
    assert_eq!(page["gap"], true);
    assert_eq!(page["next"], 20_002);
    assert_eq!(page["packets"], json!([]));
}

#[tokio::test]
async fn packet_trace_rejects_unknown_queries_and_out_of_range_limits() {
    let control = control(1, 1);
    for query in ["since=2", "limit=0", "limit=2001"] {
        let request = Request::get(format!("/v2/player/packets?{query}"))
            .header("authorization", "Bearer private-test-token")
            .body(Body::empty())
            .unwrap();
        let response = control.router().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
    }
}

// Payload bodies from the Qeynos2 stop-timing-v2 journey on 2026-11-27.

// Keep the captured spawn layout and set a nonzero raw heading.
const SPAWN_BODY: &str = concat!(
    "436c69656e7461620001000000010000a0400000000000000000000000000000010100000064",
    "000000000000000000000000000000000000000000000000c040001f85eb3e3333333f010000",
    "00008c010000ffffffff00000000070064000000000000000000000000000000000000000000",
    "000000000000000000ffffffffffffffff000000000000000000000000000000000000000000",
    "0000000000000000000000000000000000000000000000000000000000000000000000000000",
    "0000000000000000000000000000000000000000000000000000000000000000000000000000",
    "0000000000000000000000000000000000000000000000000000000000000000000000000000",
    "0000000000000000000000000000000000000000000000000000000000000000000000000000",
    "0000000000000000000000000000000000000000000000000000000000000000000000000000",
    "0000000000000055020000000018e30f18006400000000000000000000000000000030303030",
    "3030303030303030303030300000000000000000000000000000000000000000000000000000",
    "000000000000000000000000",
);

#[tokio::test]
async fn packet_trace_decodes_zone_entry_wire_payload() {
    assert_inbound_wire_fields(
        eqoxide_net::protocol::OP_ZONE_ENTRY,
        "OP_ZONE_ENTRY",
        SPAWN_BODY,
        json!({"entity":1, "name":"Clientab", "position":[-925.0,1194.0,3.125], "heading":769}),
    ).await;
}

#[tokio::test]
async fn packet_trace_decodes_new_spawn_wire_fields() {
    let mut body = SPAWN_BODY.to_owned();
    // Change the NPC and HP bytes. Race 1 uses the same equipment layout for this fixture.
    body.replace_range(2 * 18..2 * 19, "01");
    body.replace_range(2 * 37..2 * 38, "51");
    assert_inbound_wire_fields(
        eqoxide_net::protocol::OP_NEW_SPAWN,
        "OP_NEW_SPAWN",
        &body,
        json!({
            "entity": 1, "name": "Clientab", "npc": 1, "hp_percent": 81,
            "position": [-925.0,1194.0,3.125], "heading": 769,
        }),
    ).await;
}

#[tokio::test]
async fn packet_trace_decodes_inbound_position_wire_payload() {
    assert_inbound_wire_fields(
        eqoxide_net::protocol::OP_CLIENT_UPDATE,
        "OP_CLIENT_UPDATE",
        "010000000000550200000000e1e207300064000000000000",
        json!({"entity":1, "position":[-931.875,1194.0,3.125], "heading":1536}),
    ).await;
}

#[tokio::test]
async fn packet_trace_decodes_target_command_wire_payload() {
    assert_inbound_wire_fields(
        eqoxide_net::protocol::OP_TARGET_COMMAND,
        "OP_TARGET_COMMAND",
        "02000000",
        json!({"target_id":2}),
    ).await;
}

#[tokio::test]
async fn packet_trace_decodes_damage_wire_payload() {
    assert_inbound_wire_fields(
        eqoxide_net::protocol::OP_DAMAGE,
        "OP_DAMAGE",
        "020001000445230100020000000000000000000000000000000000000000",
        json!({"source":1,"target":2,"type":4,"spell":74565,"damage":2}),
    ).await;
}

#[tokio::test]
async fn packet_trace_decodes_mob_health_wire_payload() {
    assert_inbound_wire_fields(
        eqoxide_net::protocol::OP_MOB_HEALTH,
        "OP_MOB_HEALTH",
        "020051",
        json!({"entity":2,"percent":81}),
    ).await;
}

#[tokio::test]
async fn packet_trace_decodes_delete_spawn_id() {
    assert_inbound_wire_fields(
        eqoxide_net::protocol::OP_DELETE_SPAWN,
        "OP_DELETE_SPAWN",
        "2345010000",
        json!({"entity":83235}),
    ).await;
}

#[tokio::test]
async fn packet_trace_raw_payload_is_opt_in_and_truncated() {
    use eqoxide_net::packet_telemetry::Dir;
    let control = control(1, 1);
    control.packet_trace.capture(Dir::Out, 0xfffe, &[0xab; 513], None, None);
    control.packet_trace.capture(Dir::Out, 0xfffe, &[0xcd; 512], None, None);
    control.packet_trace.capture(Dir::Out, 0xfffe, &[0x00, 0xde], None, None);

    let (_, default) = authorized(&control, "GET", "/v2/player/packets", None).await;
    assert!(default["packets"][0].get("payload_hex").is_none());
    let (status, raw) = authorized(&control, "GET", "/v2/player/packets?raw=1", None).await;
    assert_eq!(status, StatusCode::OK);
    let packets = raw["packets"].as_array().unwrap();
    assert_eq!(packets[0]["payload_hex"], "ab".repeat(512));
    assert_eq!(packets[0]["payload_truncated"], true);
    assert_eq!(packets[0]["len"], 515);
    assert_eq!(packets[1]["payload_hex"], "cd".repeat(512));
    assert_eq!(packets[1]["payload_truncated"], false);
    assert_eq!(packets[1]["len"], 514);
    assert_eq!(packets[2]["payload_hex"], "00de");
    assert_eq!(packets[2]["payload_truncated"], false);
    assert_eq!(
        authorized(&control, "GET", "/v2/player/packets?raw=2", None).await.0,
        StatusCode::BAD_REQUEST,
    );
}

async fn assert_inbound_wire_fields(opcode: u16, name: &str, hex: &str, expected: Value) {
    use eqoxide_net::{testkit::test_stream_with_peer, transport::with_packet_trace};

    let control = control(1, 1);
    let (mut stream, mut rx, peer, addr) = with_packet_trace(
        control.packet_trace.clone(),
        test_stream_with_peer(Default::default()),
    ).await;
    // Place the captured app body in a plain OP_Packet frame with sequence zero.
    let mut datagram = vec![0, 0x09, 0, 0];
    datagram.extend_from_slice(&opcode.to_le_bytes());
    datagram.extend(hex.as_bytes().chunks_exact(2)
        .map(|byte| u8::from_str_radix(std::str::from_utf8(byte).unwrap(), 16).unwrap()));
    peer.send_to(&datagram, addr).await.unwrap();
    let received = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            assert!(stream.poll_recv());
            if let Ok(packet) = rx.try_recv() {
                break packet;
            }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    assert_eq!(received.opcode, opcode);
    let (status, result) = authorized(
        &control, "GET", &format!("/v2/player/packets?opcode={name}&raw=1"), None,
    ).await;
    assert_eq!(status, StatusCode::OK);
    let rows = result["packets"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["opcode_name"], name);
    assert_eq!(rows[0]["wire_seq"], 0);
    assert_eq!(rows[0]["arrival_n"], 1);
    assert!(rows[0]["arrived_at_ms"].is_u64());
    assert!(rows[0]["acked_at_ms"].is_u64());
    assert_eq!(rows[0]["payload_hex"], hex);
    assert_eq!(rows[0]["fields"], expected);
}

/// central-6ly1.1 acceptance: an hour at five commands a second keeps admitting work and loses no
/// receipt event. Compressed in time, at production capacities, past the 65 536 retention bound so
/// eviction and tombstones run, with game events interleaved, and checked per command ID.
#[test]
fn one_hour_at_five_commands_per_second_keeps_admitting_and_loses_no_receipt() {
    let control = Control::new("session-1".into(), "private-test-token".into(), 65_536, 32_768).unwrap();
    control.publish_state(4, json!({"phase":"ready"})).unwrap();
    control.set_ready(true);
    let mut cursor = 0u64;
    let mut seen: std::collections::HashMap<String, (u32, u32)> = std::collections::HashMap::new();
    const COMMANDS: usize = 70_000;
    // Design A4 load: about 60 events/s at 5 commands/s, i.e. 12 per command including the two
    // receipt events. The reader falls the promised 120 s (600 commands, 7200 events) behind.
    const EXTRA_EVENTS: usize = 10;
    const READ_EVERY: usize = 600;
    for n in 0..COMMANDS {
        let id = format!("cmd-{n}");
        let (code, _, _) = control.admit(id.clone(), movement()).unwrap();
        assert_eq!(code, StatusCode::ACCEPTED, "command {n} was not admitted");
        assert_eq!(control.drain_actions().len(), 1);
        for k in 0..EXTRA_EVENTS {
            control.publish_event(json!({"kind":"combat","n":n,"k":k}));
        }
        control
            .publish_outcome(&id, CommandStatus::AppliedLocal, None, Some(json!({"kind":"controller_intent"})))
            .unwrap();
        if n % READ_EVERY == READ_EVERY - 1 || n == COMMANDS - 1 {
            let batch = control.event_batch(cursor).unwrap();
            assert!(!batch.gap, "the reader lost events at command {n}");
            let mut expected = cursor + 1;
            for event in &batch.events {
                let n: u64 = event.cursor.rsplit_once(':').unwrap().1.parse().unwrap();
                assert_eq!(n, expected, "cursors are contiguous");
                expected += 1;
                if event.event["kind"] != "command" { continue; }
                let receipt = &event.event["command"];
                let entry = seen.entry(receipt["id"].as_str().unwrap().to_owned()).or_default();
                match receipt["status"].as_str().unwrap() {
                    "accepted" => entry.0 += 1,
                    "applied_local" => entry.1 += 1,
                    other => panic!("unexpected status {other}"),
                }
            }
            cursor = batch.cursor.rsplit_once(':').unwrap().1.parse().unwrap();
        }
    }
    assert_eq!(seen.len(), COMMANDS);
    assert!(seen.values().all(|counts| *counts == (1, 1)),
        "every command has exactly one admission and one terminal event");
    assert!(control.command("cmd-0").is_none(), "the oldest receipts were evicted");
}

/// central-6ly1.1 review: the cursor a PUT returns is the one right after its own admission event,
/// so a wait from it sees the terminal receipt; a replay returns the same cursor.
#[tokio::test]
async fn put_cursor_precedes_the_commands_own_outcome() {
    let control = control(10, 100);
    let path = "/v2/player/commands/walk-1";
    let (_, body) = authorized(&control, "PUT", path, Some(movement())).await;
    let after = body["event_cursor"].as_str().unwrap().to_owned();
    control.drain_actions();
    control
        .publish_outcome("walk-1", CommandStatus::AppliedLocal, None, Some(json!({"kind":"controller_intent"})))
        .unwrap();
    let (_, batch) = authorized(&control, "GET", &format!("/v2/player/events?after={after}"), None).await;
    assert_eq!(batch["events"][0]["event"]["command"]["status"], "applied_local");
    let (_, replay) = authorized(&control, "PUT", path, Some(movement())).await;
    assert_eq!(replay["event_cursor"].as_str(), Some(after.as_str()));
}

/// central-6ly1.1 review: payload equality is the same before and after eviction (`-0.0` == `0.0`).
#[tokio::test]
async fn an_equal_payload_replays_the_same_way_after_eviction() {
    let control = control(1, 100);
    let mut negative = movement();
    negative["operation"]["north"] = json!(-0.0);
    for (id, body) in [("a", negative.clone()), ("b", movement())] {
        control.admit(id.into(), body).unwrap();
        control.drain_actions();
        control
            .publish_outcome(id, CommandStatus::AppliedLocal, None, Some(json!({"kind":"controller_intent"})))
            .unwrap();
    }
    assert!(control.command("a").is_none() && control.command("b").is_some(), "one retained: b");
    let mut positive = movement();
    positive["operation"]["north"] = json!(0.0);
    let (status, _) = authorized(&control, "PUT", "/v2/player/commands/a", Some(positive)).await;
    assert_eq!(status, StatusCode::GONE, "0.0 equals -0.0, so this is a replay of an expired ID");
}

/// central-6ly1.1 review: the live limit counts every live receipt, safety ones included, while
/// safety commands themselves stay exempt.
#[test]
fn the_live_limit_counts_pending_safety_commands() {
    let control = control(10, 1000);
    for n in 0..64 {
        control.admit(format!("off-{n}"), request(json!({"kind":"attack_off"}))).unwrap();
    }
    let blocked = control.admit("walk".into(), movement()).unwrap_err();
    assert_eq!((blocked.0, blocked.1.as_str()), (StatusCode::SERVICE_UNAVAILABLE, "live_command_limit"));
    let before = control.identities_for_test();
    for _ in 0..5 {
        assert!(control.admit("walk".into(), movement()).is_err());
    }
    assert_eq!(control.identities_for_test(), before, "a 503 consumes no identity");
    assert!(control.command("walk").is_none());
    assert_eq!(control.admit("off-64".into(), request(json!({"kind":"attack_off"}))).unwrap().0,
        StatusCode::ACCEPTED, "a safety command is still admitted");
}

/// central-6ly1.1 review: a body that is not JSON, or a unit operation with extra fields, is 422
/// and consumes no identity.
#[tokio::test]
async fn malformed_bodies_are_422_and_consume_nothing() {
    let control = control(2, 100);
    let path = "/v2/player/commands/x";
    let truncated = Request::builder().method("PUT").uri(path)
        .header("authorization", "Bearer private-test-token").header("content-type", "application/json")
        .body(Body::from("{\"session\":")).unwrap();
    let response = control.router().oneshot(truncated).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let extra = request(json!({"kind":"attack_off","lease_ms":5}));
    let (status, body) = authorized(&control, "PUT", path, Some(extra)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid_command_payload")));
    let (status, _) = authorized(&control, "PUT", path,
        Some(json!(["session-1", 4, null, null, {"kind":"attack_off","lease_ms":5}]))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "a positional array is not a command");
    let bad_id = Request::builder().method("PUT").uri("/v2/player/commands/bad%20id")
        .header("authorization", "Bearer private-test-token").header("content-type", "application/json")
        .body(Body::from("{")).unwrap();
    assert_eq!(control.router().oneshot(bad_id).await.unwrap().status(), StatusCode::BAD_REQUEST,
        "the ID is checked before the body");
    let plain = Request::builder().method("PUT").uri(path)
        .header("authorization", "Bearer private-test-token").header("content-type", "text/plain")
        .body(Body::from(movement().to_string())).unwrap();
    assert_eq!(control.router().oneshot(plain).await.unwrap().status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    for content_type in ["application/json, application/problem+json", "application/json; garbage", "text/x+json"] {
        let listed = Request::builder().method("PUT").uri(path)
            .header("authorization", "Bearer private-test-token").header("content-type", content_type)
            .body(Body::from(movement().to_string())).unwrap();
        assert_eq!(control.router().oneshot(listed).await.unwrap().status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    let vendor = Request::builder().method("PUT").uri("/v2/player/commands/vendor")
        .header("authorization", "Bearer private-test-token").header("content-type", "application/vnd.eqoxide+json")
        .body(Body::from(request(json!({"kind":"stop"})).to_string())).unwrap();
    assert_eq!(control.router().oneshot(vendor).await.unwrap().status(), StatusCode::ACCEPTED);
    let quoted = Request::builder().method("PUT").uri("/v2/player/commands/quoted")
        .header("authorization", "Bearer private-test-token").header("content-type", "application/json; profile=\"a,b\"")
        .body(Body::from(request(json!({"kind":"stop"})).to_string())).unwrap();
    assert_eq!(control.router().oneshot(quoted).await.unwrap().status(), StatusCode::ACCEPTED,
        "a quoted parameter containing a comma is one media type");
    assert_eq!(authorized(&control, "PUT", path, Some(movement())).await.0, StatusCode::ACCEPTED,
        "the same ID is still free");
}

/// central-6ly1.1 review: fences, safety supersession and refusals all retire their receipts, so
/// no amount of them fills the live limit or keeps refusals from eviction.
#[test]
fn fences_supersessions_and_refusals_all_retire() {
    let control = control(16, 2000);
    for n in 0..70 {
        control.admit(format!("walk-{n}"), movement()).unwrap();
        control.set_ready(false); // fences it while still accepted
        control.set_ready(true);
    }
    let fenced = control.command("walk-69").unwrap();
    assert_eq!((fenced.status, fenced.terminal, fenced.reason.as_deref()), (CommandStatus::Refused, true, Some("not_ready")));
    for n in 0..70 {
        control.admit(format!("queued-{n}"), movement()).unwrap();
        control.admit(format!("stop-{n}"), request(json!({"kind":"stop"}))).unwrap();
        control.drain_actions();
        control
            .publish_outcome(&format!("stop-{n}"), CommandStatus::AppliedLocal, None, Some(json!({"kind":"controller_stopped"})))
            .unwrap();
    }
    assert_eq!(control.command("queued-69").unwrap().reason.as_deref(), Some("superseded_by_safety_command"));
    assert_eq!(control.admit("walk".into(), movement()).unwrap().0, StatusCode::ACCEPTED,
        "the live set holds no leaked receipts");
    let mut bad = movement();
    bad["operation"]["duration_ms"] = json!(0);
    let (_, refused, _) = control.admit("bad".into(), bad).unwrap();
    assert_eq!((refused.reason.as_deref(), refused.detail.is_some()), (Some("invalid_operation"), true));
    let control = self::control(2, 100);
    for n in 0..3 {
        let mut stale = movement();
        stale["session"] = json!("other");
        control.admit(format!("stale-{n}"), stale).unwrap();
    }
    assert!(control.command("stale-0").is_none(), "refusals are evicted like any terminal receipt");
    assert!(control.command("stale-1").is_some() && control.command("stale-2").is_some(),
        "exactly the retention bound is kept");
}

