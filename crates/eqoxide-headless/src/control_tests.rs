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
    let (status, body) = authorized(&control, "PUT", path, Some(movement())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "applied_local");
    assert!(control.drain_actions().is_empty());
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

#[test]
fn journal_never_evicts_and_has_a_separate_reserve_for_each_safety_verb() {
    let control = control(1, 20);
    control.admit("walk".into(), movement()).unwrap();
    assert_eq!(
        control
            .admit("new".into(), request(json!({"kind":"sit"})))
            .unwrap_err()
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    control
        .admit("stop".into(), request(json!({"kind":"stop"})))
        .unwrap();
    control
        .admit("off".into(), request(json!({"kind":"attack_off"})))
        .unwrap();
    let actions = control.drain_actions();
    assert_eq!(
        actions.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
        vec!["stop", "off"]
    );
    assert_eq!(
        control.command("walk").unwrap().status,
        CommandStatus::Refused
    );
    assert_eq!(
        control
            .admit("stop-again".into(), request(json!({"kind":"stop"})))
            .unwrap_err()
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        control.admit("walk".into(), movement()).unwrap().1.status,
        CommandStatus::Refused
    );
    assert_eq!(
        control
            .admit("off".into(), request(json!({"kind":"attack_off"})))
            .unwrap()
            .0,
        StatusCode::OK
    );
    assert!(control.drain_actions().is_empty());
}

#[test]
fn busy_refusal_is_retained_and_priority_commands_bypass_busy() {
    let control = control(10, 20);
    control
        .admit("chat".into(), request(json!({"kind":"say","text":"hello"})))
        .unwrap();
    let (_, receipt) = control
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
    control
        .publish_outcome(
            "move",
            CommandStatus::Unknown,
            Some("connection_lost".into()),
            None,
        )
        .unwrap();
    // Late genuine server evidence can reconcile uncertainty.
    control
        .publish_outcome(
            "move",
            CommandStatus::Confirmed,
            None,
            Some(json!({"kind":"server_position_update"})),
        )
        .unwrap();
    assert!(control
        .publish_outcome("move", CommandStatus::Unknown, Some("timeout".into()), None)
        .is_err());
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
    control.publish_event(json!({"kind":"chat","text":"first"}));
    control.publish_event(json!({"kind":"chat","text":"second"}));
    let (status, batch) =
        authorized(&control, "GET", "/v2/player/events?after=session-1:0", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(batch["gap"], true);
    assert_eq!(batch["events"].as_array().unwrap().len(), 2);
    assert_eq!(batch["cursor"], "session-1:3");
    assert_eq!(batch["events"][0]["event"]["text"], "first");
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
async fn long_poll_wakes_for_publication_and_returns_empty_on_timeout() {
    let control = control(10, 10);
    let waiting = control.clone();
    let task = tokio::spawn(async move {
        authorized(
            &waiting,
            "GET",
            "/v2/player/events?after=session-1:1&wait=1",
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
        "/v2/player/events?after=session-1:2&wait=0.001",
        None,
    )
    .await;
    assert!(batch["events"].as_array().unwrap().is_empty());
    assert_eq!(batch["cursor"], "session-1:2");
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
async fn fence_after_dispatch_is_unknown_and_accepts_a_late_server_receipt() {
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
    control
        .publish_outcome(
            "buy-1",
            CommandStatus::Confirmed,
            None,
            Some(json!({"kind":"server_buy_receipt"})),
        )
        .unwrap();
    assert_eq!(
        control.command("buy-1").unwrap().status,
        CommandStatus::Confirmed
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
