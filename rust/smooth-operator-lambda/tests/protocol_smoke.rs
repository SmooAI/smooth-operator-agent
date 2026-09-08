//! E2E for the AWS-serverless flavor's protocol path, without AWS.
//!
//! # Why this exists
//!
//! The serverless flavor had no test of any kind. The k8s flavor is gated by a
//! kind cluster in CI and the local flavor by the web-chat smoke, but the
//! Lambda could only be exercised by deploying it — so its protocol handling,
//! the one thing that differs from the reference server, was unverified on
//! every PR.
//!
//! # What it covers, and what it deliberately doesn't
//!
//! [`dispatch::handle_frame`] takes `&Arc<dyn StorageAdapter>`, so this drives
//! real frames against the **in-memory** adapter and reads the replies out of a
//! capturing poster. That isolates the Lambda-specific seam — frame parsing,
//! action dispatch, and post-back — from storage.
//!
//! DynamoDB correctness is NOT retested here: the adapter has its own
//! conformance suite against `dynamodb-local`. Duplicating it would mean this
//! test needs Docker, and a test that needs Docker is a test that gets skipped.
//!
//! What remains genuinely untestable without AWS is the transport itself (API
//! Gateway invoking the function, and `PostToConnection` delivering a frame).
//! That is what `serverless-flavor-smoke.yml` covers, against a real ephemeral
//! stage — the two are complements, not alternatives.

use std::sync::Arc;

use serde_json::{json, Value};
use smooth_operator::adapter::StorageAdapter;
use smooth_operator::auth::{AuthConfig, AuthVerifier};
use smooth_operator_adapter_memory::InMemoryStorageAdapter;

use smooai_smooth_operator_lambda::config::LambdaConfig;
use smooai_smooth_operator_lambda::dispatch;
use smooai_smooth_operator_lambda::poster::ConnectionPoster;

/// A config with no gateway key: LLM turns are unavailable, which is exactly
/// the posture we want for a keyless CI probe — every action below is
/// protocol-only.
fn test_config() -> LambdaConfig {
    LambdaConfig {
        table: "test-table".into(),
        gateway_url: "http://127.0.0.1:1".into(),
        gateway_key: None,
        model: "test-model".into(),
        max_iterations: 1,
        max_tokens: 16,
        org_id: "test-org".into(),
        vector_bucket: None,
        vector_index_prefix: "test".into(),
    }
}

fn harness() -> (
    Arc<dyn StorageAdapter>,
    LambdaConfig,
    Arc<dyn AuthVerifier>,
    ConnectionPoster,
) {
    let storage: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::new());
    let auth: Arc<dyn AuthVerifier> =
        Arc::from(AuthConfig::from_env().expect("auth disabled by default with no env set"));
    (storage, test_config(), auth, ConnectionPoster::capturing())
}

/// Every event's `type`, in the order it was posted back.
fn types(poster: &ConnectionPoster) -> Vec<String> {
    poster
        .captured()
        .iter()
        .filter_map(|e| e.get("type").and_then(Value::as_str).map(str::to_string))
        .collect()
}

#[tokio::test]
async fn ping_is_answered_with_pong() {
    let (storage, config, auth, poster) = harness();
    dispatch::handle_frame(&storage, &config, &auth, &poster, r#"{"action":"ping"}"#)
        .await
        .expect("ping must not error");
    assert_eq!(
        types(&poster),
        vec!["pong"],
        "ping should produce exactly one pong"
    );
}

#[tokio::test]
async fn create_conversation_session_returns_a_session_id() {
    let (storage, config, auth, poster) = harness();
    let frame = json!({
        "action": "create_conversation_session",
        "requestId": "req-1",
        "agentId": "agent-1",
        "userName": "smoke",
    })
    .to_string();

    dispatch::handle_frame(&storage, &config, &auth, &poster, &frame)
        .await
        .expect("create_conversation_session must not error");

    let captured = poster.captured();
    let session_id = captured
        .iter()
        .find_map(|e| e.pointer("/data/sessionId").and_then(Value::as_str))
        .expect("a sessionId must come back, nested under `data`");
    assert!(!session_id.is_empty(), "sessionId must not be blank");
}

/// A malformed frame must come back as a protocol `error` event — never as a
/// hard Lambda error, which API Gateway would turn into a dropped connection.
#[tokio::test]
async fn invalid_json_is_a_protocol_error_not_a_lambda_failure() {
    let (storage, config, auth, poster) = harness();
    let result = dispatch::handle_frame(&storage, &config, &auth, &poster, "{not json").await;
    assert!(result.is_ok(), "a bad frame must not fail the invocation");
    assert_eq!(types(&poster), vec!["error"]);
}

/// An unknown action is likewise a protocol error, not a crash.
#[tokio::test]
async fn unknown_action_is_a_protocol_error() {
    let (storage, config, auth, poster) = harness();
    let frame = json!({ "action": "definitely_not_an_action", "requestId": "req-2" }).to_string();
    let result = dispatch::handle_frame(&storage, &config, &auth, &poster, &frame).await;
    assert!(
        result.is_ok(),
        "an unknown action must not fail the invocation"
    );
    assert_eq!(types(&poster), vec!["error"]);
}

/// `list_conversations` must be SERVED, not refused.
///
/// This is the regression the serverless smoke actually hit: the transport
/// implemented four protocol actions and answered `UNSUPPORTED_ACTION` for the
/// rest, so a client that had connected, created a session and was about to
/// send a message fell over on the sidebar query. It is now delegated to the
/// reference server's handler, which owns the org + per-user read predicate.
///
/// The list comes back empty — the session has no messages yet, and the server
/// filters empty conversations out of the sidebar — so what is asserted is the
/// shape: a successful response carrying a `conversations` array, never an
/// error.
#[tokio::test]
async fn list_conversations_is_delegated_not_refused() {
    let (storage, config, auth, poster) = harness();
    let create = json!({
        "action": "create_conversation_session",
        "requestId": "req-c",
        "agentId": "agent-1",
        "userName": "smoke",
    })
    .to_string();
    dispatch::handle_frame(&storage, &config, &auth, &poster, &create)
        .await
        .expect("create_conversation_session must not error");

    let (storage2, config2, auth2, poster2) =
        (storage.clone(), config, auth, ConnectionPoster::capturing());
    let frame = json!({ "action": "list_conversations", "requestId": "req-l" }).to_string();
    dispatch::handle_frame(&storage2, &config2, &auth2, &poster2, &frame)
        .await
        .expect("list_conversations must not error");

    assert_ne!(
        types(&poster2),
        vec!["error"],
        "list_conversations must not come back as UNSUPPORTED_ACTION"
    );
    let captured = poster2.captured();
    assert!(
        captured.iter().any(|e| e
            .pointer("/data/conversations")
            .is_some_and(Value::is_array)),
        "a `conversations` array must come back nested under `data`, got {captured:?}"
    );
}

/// The negative control for the delegation, and the reason it is an allowlist
/// rather than a catch-all.
///
/// `confirm_tool_action` resumes a turn parked in connection-local state. A
/// Lambda invocation that already returned holds no such turn, so delegating it
/// would produce a handler that looks wired and silently never completes. It
/// must keep answering `UNSUPPORTED_ACTION` — an honest "this transport
/// cannot". If someone widens `DELEGATED_ACTIONS` to everything the server
/// handles, this test is what fails.
#[tokio::test]
async fn connection_stateful_actions_stay_unsupported() {
    for action in ["confirm_tool_action", "verify_otp", "submit_interaction"] {
        let (storage, config, auth, poster) = harness();
        let frame = json!({ "action": action, "requestId": "req-x" }).to_string();
        dispatch::handle_frame(&storage, &config, &auth, &poster, &frame)
            .await
            .expect("must not fail the invocation");
        assert_eq!(
            types(&poster),
            vec!["error"],
            "{action} must stay unsupported on the lambda transport"
        );
        let captured = poster.captured();
        assert_eq!(
            captured[0]
                .pointer("/data/error/code")
                .and_then(Value::as_str),
            Some("UNSUPPORTED_ACTION"),
            "{action} must be refused as UNSUPPORTED_ACTION, got {captured:?}"
        );
    }
}
