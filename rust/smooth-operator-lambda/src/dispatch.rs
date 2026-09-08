//! Per-invocation action dispatch for the API Gateway WebSocket Lambda.
//!
//! Unlike the reference axum server — one long-lived socket, one in-process
//! session registry, one outbound `mpsc` sink — API Gateway WebSocket invokes
//! this Lambda **once per inbound frame** with no socket and no in-process state
//! carried across invocations. So this module:
//!
//! 1. Keeps **no** session map in memory: sessions are created/read straight
//!    from the DynamoDB [`StorageAdapter`] (`create_session` / `get_session`),
//!    which is the durable source of truth across invocations.
//! 2. Posts events **back** through the API Gateway Management API
//!    ([`ConnectionPoster`]) instead of writing to a socket sink.
//! 3. Reuses the reference server's wire-protocol builders
//!    ([`smooth_operator_server::protocol`]) and its streaming, memory-
//!    carrying turn runner ([`smooth_operator_server::runner`]) verbatim —
//!    only the transport differs, so the protocol bytes and the turn semantics
//!    are identical to the server's.
//!
//! ### Streaming bridge
//! `runner::run_streaming_turn` emits events through an
//! `UnboundedSender<Value>` sink. We give it the sender half of a channel and,
//! concurrently, drain the receiver half — `post`ing each event to the
//! connection as it arrives. That preserves real-time token streaming over a
//! transport that has no socket: the runner fills the channel while the drain
//! task forwards to the Management API.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::StorageAdapter;
use smooth_operator::auth::AuthVerifier;
use smooth_operator::domain::{
    Conversation, Participant, ParticipantType, Platform, Session, SessionStatus,
};
use smooth_operator_server::handler::{self, UserScope};
use smooth_operator_server::runner::TurnRequest;
use smooth_operator_server::state::AppState;
use smooth_operator_server::{config::ServerConfig, protocol, runner};

use crate::config::LambdaConfig;
use crate::poster::ConnectionPoster;

/// The agent's display name.
const AGENT_NAME: &str = "smooth-agent";

/// Actions this transport does not implement itself but CAN serve, by handing
/// the frame to the reference server's own [`handler::handle_frame`].
///
/// Why delegate rather than reimplement: these actions are gated by
/// `may_read_conversation`, the org+per-user predicate that decides who may see
/// which conversation. A second copy of that predicate living here is precisely
/// the defect class that produced this repo's cross-tenant P0s — two guards
/// that drift until one of them checks the wrong thing. One predicate, both
/// transports.
///
/// The set is deliberately narrow: every member is connection-INDEPENDENT and
/// spawns no turn, which is what makes it safe on a transport with no socket
/// and no state between invocations. Notably absent, and staying absent:
///
///   - `confirm_tool_action` resumes a turn parked in connection-local state.
///     There is no parked turn in a Lambda that already returned.
///   - `verify_otp` / `submit_interaction` resolve against the in-process
///     interaction registry, which does not survive an invocation either.
///
/// Those three still answer `UNSUPPORTED_ACTION` — an honest "this transport
/// cannot", rather than a handler that would half-work.
const DELEGATED_ACTIONS: &[&str] = &[
    "get_conversation_messages",
    "list_conversations",
    "rename_conversation",
];

/// Handle one inbound `$default` / route-key frame: parse the action envelope,
/// run it against DynamoDB, and post every produced event back to the
/// connection via `poster`.
///
/// Returns `Ok(())` regardless of protocol-level failures — those surface as
/// `error` events to the client, never as a hard Lambda error (which would
/// drop the connection / retry).
pub async fn handle_frame(
    storage: &Arc<dyn StorageAdapter>,
    config: &LambdaConfig,
    auth: &Arc<dyn AuthVerifier>,
    poster: &ConnectionPoster,
    raw: &str,
) -> Result<()> {
    let parsed: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            poster
                .post(&protocol::error(
                    None,
                    "VALIDATION_ERROR",
                    &format!("invalid JSON frame: {e}"),
                ))
                .await?;
            return Ok(());
        }
    };

    let action = parsed.get("action").and_then(Value::as_str);
    let request_id = parsed.get("requestId").and_then(Value::as_str);

    match action {
        Some("ping") => {
            poster.post(&protocol::pong(request_id)).await?;
        }
        Some("create_conversation_session") => {
            create_session(storage, config, auth, poster, &parsed, request_id).await?;
        }
        Some("get_session") => {
            get_session(storage, auth, poster, &parsed, request_id).await?;
        }
        Some("send_message") => {
            send_message(storage, config, auth, poster, &parsed, request_id).await?;
        }
        Some(other) if DELEGATED_ACTIONS.contains(&other) => {
            delegate_to_server(storage, config, auth, poster, &parsed, raw).await?;
        }
        Some(other) => {
            poster
                .post(&protocol::error(
                    request_id,
                    "UNSUPPORTED_ACTION",
                    &format!("action '{other}' is not supported"),
                ))
                .await?;
        }
        None => {
            poster
                .post(&protocol::error(
                    request_id,
                    "VALIDATION_ERROR",
                    "missing 'action' field",
                ))
                .await?;
        }
    }
    Ok(())
}

/// `create_conversation_session` — create a conversation + user/agent
/// participants + a session in DynamoDB, then reply with an
/// `immediate_response` carrying the session descriptor.
async fn create_session(
    storage: &Arc<dyn StorageAdapter>,
    config: &LambdaConfig,
    auth: &Arc<dyn AuthVerifier>,
    poster: &ConnectionPoster,
    parsed: &Value,
    request_id: Option<&str>,
) -> Result<()> {
    // agentId is REQUIRED by the Request schema and the generated client type is non-optional,
    // so absent-or-blank is a malformed request, not an agentless session. Reject it: the old
    // code fabricated a UUID, and th-68897a's first pass silently stored NULL — both skip the
    // validation that belongs at this boundary. The column stays nullable for rows that predate
    // this check; it is just no longer reachable from the create path.
    let Some(agent_id) = parsed
        .get("agentId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        poster
            .post(&protocol::error(
                request_id,
                "VALIDATION_ERROR",
                "missing 'agentId'",
            ))
            .await?;
        return Ok(());
    };

    let user_name = parsed
        .get("userName")
        .and_then(Value::as_str)
        .unwrap_or("Visitor")
        .to_string();
    let user_email = parsed
        .get("userEmail")
        .and_then(Value::as_str)
        .map(str::to_string);
    let browser_fingerprint = parsed
        .get("browserFingerprint")
        .and_then(Value::as_str)
        .map(str::to_string);

    let now = chrono::Utc::now();
    // Derive the session's org from the frame's authenticated JWT principal when
    // present; otherwise fall back to the lambda's configured org. The lambda
    // transport has no persistent socket, so the bearer token rides on the frame
    // (same as `send_message`). A missing/unverifiable token leaves the
    // configured-org behavior unchanged.
    let principal = resolve_frame_principal(auth, parsed);
    let org_id = principal
        .as_ref()
        .map(|p| p.org_id.clone())
        .unwrap_or_else(|| config.org_id.clone());
    // The authenticated principal's email wins over the frame's `userEmail`:
    // the frame field is caller-controlled, so honoring it would stamp a
    // conversation with someone else's identity — and the server's per-user
    // conversation scoping reads exactly this participant email back.
    let user_email = principal
        .as_ref()
        .and_then(|p| p.email.clone())
        .or(user_email);

    let conversation_id = uuid::Uuid::new_v4().to_string();
    let session_id = uuid::Uuid::new_v4().to_string();
    let user_participant_id = uuid::Uuid::new_v4().to_string();
    let agent_participant_id = uuid::Uuid::new_v4().to_string();

    let conversation = Conversation {
        id: conversation_id.clone(),
        platform: Platform::Web,
        name: format!("Session {session_id}"),
        organization_id: org_id.clone(),
        idempotency_key: session_id.clone(),
        metadata_json: parsed.get("metadata").cloned(),
        analytics_json: None,
        created_at: now,
        updated_at: now,
    };

    let user_participant = Participant {
        id: user_participant_id.clone(),
        conversation_id: conversation_id.clone(),
        organization_id: org_id.clone(),
        participant_type: ParticipantType::User,
        external_id: None,
        internal_id: None,
        browser_fingerprint,
        browser_info: None,
        name: user_name,
        email: user_email,
        phone: None,
        crm_contact_id: None,
        metadata_json: None,
        created_at: now,
        updated_at: now,
    };

    let agent_participant = Participant {
        id: agent_participant_id.clone(),
        conversation_id: conversation_id.clone(),
        organization_id: org_id.clone(),
        participant_type: ParticipantType::AiAgent,
        external_id: None,
        internal_id: Some(agent_id.clone()),
        browser_fingerprint: None,
        browser_info: None,
        name: AGENT_NAME.to_string(),
        email: None,
        phone: None,
        crm_contact_id: None,
        metadata_json: None,
        created_at: now,
        updated_at: now,
    };

    let session = Session {
        session_id: session_id.clone(),
        conversation_id: conversation_id.clone(),
        organization_id: org_id.clone(),
        agent_id: Some(agent_id.clone()),
        agent_name: AGENT_NAME.to_string(),
        user_participant_id: user_participant_id.clone(),
        agent_participant_id: agent_participant_id.clone(),
        // The thread id is the conversation id: per-session memory is carried by
        // replaying this conversation's persisted message log (see runner).
        thread_id: conversation_id.clone(),
        status: Some(SessionStatus::Active),
        token_count: Some(0),
        message_count: Some(0),
        metadata: None,
        created_at: Some(now),
        updated_at: Some(now),
        ended_at: None,
        last_activity_at: Some(now),
    };

    // Persist to DynamoDB; any failure surfaces as a clean error event.
    if let Err(e) = storage.create_conversation(conversation).await {
        return poster
            .post(&protocol::error(
                request_id,
                "INTERNAL_ERROR",
                &format!("create conversation failed: {e}"),
            ))
            .await
            .map(|_| ());
    }
    if let Err(e) = storage.add_participant(user_participant).await {
        return poster
            .post(&protocol::error(
                request_id,
                "INTERNAL_ERROR",
                &format!("add user participant failed: {e}"),
            ))
            .await
            .map(|_| ());
    }
    if let Err(e) = storage.add_participant(agent_participant).await {
        return poster
            .post(&protocol::error(
                request_id,
                "INTERNAL_ERROR",
                &format!("add agent participant failed: {e}"),
            ))
            .await
            .map(|_| ());
    }
    if let Err(e) = storage.create_session(session).await {
        return poster
            .post(&protocol::error(
                request_id,
                "INTERNAL_ERROR",
                &format!("create session failed: {e}"),
            ))
            .await
            .map(|_| ());
    }

    let data = json!({
        "sessionId": session_id,
        "conversationId": conversation_id,
        "agentId": agent_id,
        "agentName": AGENT_NAME,
        "userParticipantId": user_participant_id,
        "agentParticipantId": agent_participant_id,
    });
    poster
        .post(&protocol::immediate_response(
            request_id,
            200,
            "Session created",
            data,
        ))
        .await?;
    Ok(())
}

/// `get_session` — read the session snapshot straight from DynamoDB.
async fn get_session(
    storage: &Arc<dyn StorageAdapter>,
    auth: &Arc<dyn AuthVerifier>,
    poster: &ConnectionPoster,
    parsed: &Value,
    request_id: Option<&str>,
) -> Result<()> {
    let Some(session_id) = parsed.get("sessionId").and_then(Value::as_str) else {
        poster
            .post(&protocol::error(
                request_id,
                "VALIDATION_ERROR",
                "missing 'sessionId'",
            ))
            .await?;
        return Ok(());
    };

    match storage.get_session(session_id).await {
        Ok(Some(s)) if !same_org(frame_org(auth, parsed).as_deref(), &s.organization_id) => {
            // Another tenant's session — answer exactly as for an unknown id so
            // the caller cannot distinguish "not yours" from "never existed".
            poster
                .post(&protocol::error(
                    request_id,
                    "SESSION_NOT_FOUND",
                    &format!("session '{session_id}' not found"),
                ))
                .await?;
        }
        Ok(Some(s)) => {
            let data = json!({
                "sessionId": s.session_id,
                "conversationId": s.conversation_id,
                "agentId": s.agent_id,
                "agentName": s.agent_name,
                "userParticipantId": s.user_participant_id,
                "agentParticipantId": s.agent_participant_id,
                "threadId": s.thread_id,
                "status": s.status.map_or("active", |st| match st {
                    SessionStatus::Active => "active",
                    SessionStatus::Idle => "idle",
                    SessionStatus::Ended => "ended",
                }),
            });
            poster
                .post(&protocol::immediate_response(
                    request_id, 200, "Session", data,
                ))
                .await?;
        }
        Ok(None) => {
            poster
                .post(&protocol::error(
                    request_id,
                    "SESSION_NOT_FOUND",
                    &format!("session '{session_id}' not found"),
                ))
                .await?;
        }
        Err(e) => {
            poster
                .post(&protocol::error(
                    request_id,
                    "INTERNAL_ERROR",
                    &format!("get session failed: {e}"),
                ))
                .await?;
        }
    }
    Ok(())
}

/// `send_message` — ack with `immediate_response` (202), run a streaming
/// knowledge-grounded turn over the DynamoDB adapter (reusing the server's
/// runner), forward `stream_token` / `stream_chunk` to the connection as they
/// happen, and finish with `eventual_response` (200).
/// Resolve the requester's document-level [`AccessContext`] from the frame's
/// bearer `token` field (the lambda transport has no persistent socket, so the
/// token rides on the `send_message` frame).
///
/// **Fail closed for ACL'd content**: no `token`, an unconfigured/disabled
/// verifier, or a token that fails to verify all yield
/// [`AccessContext::anonymous`] — org-public knowledge only, never every
/// document. A valid token yields the principal's full entitlement (user id +
/// groups). Verification failures are logged (never the token) and degrade to
/// anonymous so the no-auth/dev case still serves org-public knowledge.
/// Resolve the authenticated org from the frame's bearer `token`, when present
/// and valid. Returns `Some(org_id)` only for a token that verifies to a JWT
/// principal; `None` for no token, an unconfigured/disabled verifier, or a token
/// that fails to verify — so the caller falls back to the configured org and the
/// no-auth/dev behavior is unchanged. Verification failures are logged (never the
/// token).
/// The org a frame is authenticated to, when it carries a verifying token.
/// `None` for no token / an unconfigured verifier / a token that fails to
/// verify — which keeps the no-auth and dev paths unchanged.
fn frame_org(auth: &Arc<dyn AuthVerifier>, parsed: &Value) -> Option<String> {
    resolve_frame_principal(auth, parsed).map(|p| p.org_id)
}

/// Whether a frame authenticated to `auth_org` may touch a row owned by
/// `row_org`. `None` (unauthenticated) keeps the pre-existing behavior; an
/// authenticated frame is pinned to its principal's tenant.
///
/// Without this the lambda transport had **no** check at all on `sessionId`:
/// `get_session` and `send_message` acted on whatever `get_session` returned, so
/// any caller who knew or guessed a session id could read another org's session
/// snapshot and drive turns in its conversation. Feature gap G7.
fn same_org(auth_org: Option<&str>, row_org: &str) -> bool {
    auth_org.is_none_or(|o| o == row_org)
}

fn resolve_frame_principal(
    auth: &Arc<dyn AuthVerifier>,
    parsed: &Value,
) -> Option<smooth_operator::auth::Principal> {
    let token = parsed
        .get("token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())?;
    match auth.verify(token) {
        Ok(principal) => Some(principal),
        Err(e) => {
            tracing::warn!(
                auth_mode = auth.mode(),
                error = %e,
                "create_session token failed verification; using configured org"
            );
            None
        }
    }
}

fn resolve_frame_access(auth: &Arc<dyn AuthVerifier>, parsed: &Value) -> AccessContext {
    let Some(token) = parsed
        .get("token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return AccessContext::anonymous();
    };
    match auth.verify(token) {
        Ok(principal) => principal.access_context(),
        Err(e) => {
            tracing::warn!(
                auth_mode = auth.mode(),
                error = %e,
                "send_message token failed verification; serving org-public knowledge only (anonymous)"
            );
            AccessContext::anonymous()
        }
    }
}

async fn send_message(
    storage: &Arc<dyn StorageAdapter>,
    config: &LambdaConfig,
    auth: &Arc<dyn AuthVerifier>,
    poster: &ConnectionPoster,
    parsed: &Value,
    request_id: Option<&str>,
) -> Result<()> {
    // requestId is load-bearing for streaming correlation; require it.
    let Some(request_id) = request_id else {
        poster
            .post(&protocol::error(
                None,
                "VALIDATION_ERROR",
                "send_message requires a 'requestId'",
            ))
            .await?;
        return Ok(());
    };

    let Some(session_id) = parsed.get("sessionId").and_then(Value::as_str) else {
        poster
            .post(&protocol::error(
                Some(request_id),
                "VALIDATION_ERROR",
                "missing 'sessionId'",
            ))
            .await?;
        return Ok(());
    };

    let message = match parsed.get("message").and_then(Value::as_str) {
        Some(m) if !m.trim().is_empty() => m.to_string(),
        _ => {
            poster
                .post(&protocol::error(
                    Some(request_id),
                    "VALIDATION_ERROR",
                    "missing or empty 'message'",
                ))
                .await?;
            return Ok(());
        }
    };

    let session = match storage.get_session(session_id).await {
        Ok(Some(s)) if !same_org(frame_org(auth, parsed).as_deref(), &s.organization_id) => {
            // Another tenant's session — indistinguishable from an unknown id.
            poster
                .post(&protocol::error(
                    Some(request_id),
                    "SESSION_NOT_FOUND",
                    &format!("session '{session_id}' not found"),
                ))
                .await?;
            return Ok(());
        }
        Ok(Some(s)) => s,
        Ok(None) => {
            poster
                .post(&protocol::error(
                    Some(request_id),
                    "SESSION_NOT_FOUND",
                    &format!("session '{session_id}' not found"),
                ))
                .await?;
            return Ok(());
        }
        Err(e) => {
            poster
                .post(&protocol::error(
                    Some(request_id),
                    "INTERNAL_ERROR",
                    &format!("get session failed: {e}"),
                ))
                .await?;
            return Ok(());
        }
    };

    // No gateway key → can't run an LLM turn. Clean error; the handler stays
    // usable for protocol-only checks.
    let Some(llm) = config.llm_config() else {
        poster
            .post(&protocol::error(
                Some(request_id),
                "LLM_UNAVAILABLE",
                "SMOOAI_GATEWAY_KEY is not configured; this handler cannot serve LLM turns.",
            ))
            .await?;
        return Ok(());
    };

    // Ack: processing started.
    poster
        .post(&protocol::immediate_response(
            Some(request_id),
            202,
            "Processing your request...",
            json!({}),
        ))
        .await?;

    // Bridge the runner's sink to the Management API: the runner fills `tx`
    // with protocol events; the drain task forwards each to the connection in
    // real time. Run both concurrently.
    let (tx, rx): (UnboundedSender<Value>, UnboundedReceiver<Value>) = mpsc::unbounded_channel();
    let poster_for_drain = poster.clone();
    let drain = tokio::spawn(forward_events(rx, poster_for_drain));

    // Resolve the requester's entitlement from the frame's bearer token, then
    // run the turn through the ACL-enforcing knowledge path. Carry the turn's org
    // on the AccessContext so a multi-tenant host adapter's `knowledge_for_access`
    // can scope RAG to this tenant: the authed-token path already stamps its own
    // org, so only fall back to the session's persisted org for an anonymous /
    // no-org frame. Behavior-preserving for the single-tenant default (the
    // built-in ACL ignores the org).
    let access = {
        let resolved = resolve_frame_access(auth, parsed);
        if resolved.organization_id.is_some() {
            resolved
        } else {
            resolved.with_organization_id(session.organization_id.clone())
        }
    };

    let result = runner::run_streaming_turn(
        TurnRequest {
            storage: storage.clone(),
            llm,
            max_iterations: config.max_iterations,
            conversation_id: &session.conversation_id,
            request_id,
            user_message: &message,
            model_max_output: None,
            access,
            llm_provider: None,
            executor: None,
            // Opt-in rerank stage (feature gap G8): `None` unless the operator
            // enabled it via `SMOOTH_AGENT_RERANK`. Default-off keeps retrieval
            // behavior unchanged.
            reranker: smooth_operator_server::reranker::build_reranker(
                &smooth_operator_server::reranker::RerankerConfig::from_gateway(
                    config.gateway_url.clone(),
                    config.gateway_key.clone(),
                ),
            ),
            // The Lambda path is request/response over API Gateway management,
            // not a persistent socket, so there is no same-connection reader to
            // receive a later `confirm_tool_action` frame. Write-confirmation
            // HITL pause/resume does not apply here; leave it disabled.
            confirmation: None,
            // Rich Interactions are a WS-server seam; the lambda flavor doesn't
            // wire them (no interaction tools registered — unchanged behavior).
            interactions: None,
            // Injection seams (server `AppState::with_tools` / per-org persona):
            // the AWS lambda flavor installs neither, so both stay default —
            // built-in tools only and the runner's const prompt. `org_id` is
            // threaded through so a future host provider could scope per-org.
            tool_provider: None,
            system_prompt: None,
            org_id: Some(config.org_id.clone()),
            // The lambda flavor installs no tool provider, but thread the
            // configured gateway key through so a future host provider could
            // scope per-org (mirrors `org_id`).
            gateway_key: config.gateway_key.clone(),
            // The lambda flavor does not resolve per-agent config; a conversation
            // workflow (if any) is driven by the WS server path. Freeform here.
            workflow: None,
            judge: None,
            greeting_section: None,
            // Per-agent skill + host tool hooks are WS-server seams (persona
            // resolution / AppState); the lambda flavor installs neither.
            skill_section: None,
            tool_hooks: vec![],
            enabled_tools: None,
            auth_gate: None,
            tool_configs: None,
            extensions: None,
            // The lambda flavor is text-only; no multimodal attachments.
            images: vec![],
            files: vec![],
            request_metadata: None,
            // The seeded-demo flavor is a reference-server-only affordance; the
            // AWS lambda path never registers the demo write tool.
            demo_tools: false,
        },
        &tx,
    )
    .await;

    // Closing the sender ends the drain task once it has flushed everything.
    drop(tx);
    let _ = drain.await;

    match result {
        Ok(turn) => {
            let response =
                runner::general_agent_response(&turn.reply, &turn.suggested_next_actions);
            poster
                .post(&protocol::eventual_response(
                    request_id,
                    200,
                    &turn.message_id,
                    response,
                    false,
                    &turn.citations,
                    turn.usage,
                    turn.directive,
                ))
                .await?;
        }
        Err(e) => {
            poster
                .post(&protocol::error(
                    Some(request_id),
                    "AGENT_ERROR",
                    &format!("agent turn failed: {e}"),
                ))
                .await?;
        }
    }
    Ok(())
}

/// The connection's per-user conversation-read scope, derived from the frame's
/// bearer token.
///
/// This is the reference server's `scope_for` rule verbatim — the verified-
/// principal branch plus its `anonymous_scope` fallback — sourced from the frame
/// instead of the `?token=` query string, because the lambda transport has no
/// persistent connection to hang a principal off. It is copied as a DERIVATION,
/// not as policy: the policy it feeds (`may_read_conversation`) stays single-
/// sourced in the server.
///
/// Fails closed on a multi-user deployment: no token, or a principal with no
/// `email` claim, owns nothing and therefore lists nothing. Single-user flavors
/// (`none` / `disabled` / `local-token`) have no identity to scope by and stay
/// unscoped, so the keyless/local behavior is unchanged.
fn frame_user_scope(auth: &Arc<dyn AuthVerifier>, parsed: &Value) -> UserScope {
    let single_user = smooth_operator::auth::is_single_user_mode(auth.mode());
    match resolve_frame_principal(auth, parsed) {
        Some(principal) => match principal.email {
            Some(email) => UserScope::User(email),
            None if single_user => UserScope::Unscoped,
            None => UserScope::Denied,
        },
        None if single_user => UserScope::Unscoped,
        None => UserScope::Denied,
    }
}

/// Serve one [`DELEGATED_ACTIONS`] frame through the reference server's handler.
///
/// The bridge is the same one `send_message` already uses for streaming: the
/// handler writes protocol events into an `UnboundedSender`, and a drain task
/// posts each to the connection. Here the sender is dropped as soon as the
/// handler returns, which closes the channel and lets the drain finish — these
/// actions emit a single `immediate_response`, not a stream.
async fn delegate_to_server(
    storage: &Arc<dyn StorageAdapter>,
    config: &LambdaConfig,
    auth: &Arc<dyn AuthVerifier>,
    poster: &ConnectionPoster,
    parsed: &Value,
    raw: &str,
) -> Result<()> {
    // Config comes from the same environment the lambda already reads, so the
    // delegated handlers see the deployment's real settings rather than defaults.
    let state = AppState::new(storage.clone(), ServerConfig::from_env()).with_auth(auth.clone());

    let access = resolve_frame_access(auth, parsed);
    // The frame's principal org, else the lambda's CONFIGURED org — the same
    // fallback `create_session` stamps rows with. Passing `None` here instead
    // would be quietly wrong in both directions: the server's org fallback is
    // its own SEED_ORG_ID, so an unauthenticated frame would enumerate a
    // different org than the one this deployment writes to and always come back
    // empty; and `None` also disables the tenant half of `may_read_conversation`
    // altogether. Naming the configured org keeps list and create agreeing, and
    // keeps the check armed.
    let auth_org = frame_org(auth, parsed).unwrap_or_else(|| config.org_id.clone());
    let scope = frame_user_scope(auth, parsed);

    let (tx, rx): (UnboundedSender<Value>, UnboundedReceiver<Value>) = mpsc::unbounded_channel();
    let drain = tokio::spawn(forward_events(rx, poster.clone()));

    // `conn_id` is connection-local bookkeeping the delegated actions never
    // consult; the poster's id is the closest true value on this transport.
    let conn_id = poster.connection_id().unwrap_or("lambda").to_string();
    let spawned = handler::handle_frame(
        &state,
        &access,
        &conn_id,
        None,
        Some(auth_org.as_str()),
        &scope,
        raw,
        &tx,
    )
    .await;

    // Nothing in DELEGATED_ACTIONS spawns a turn. If that ever stops being true,
    // abort rather than leak a task whose events have nowhere to go once this
    // invocation returns — and say so loudly, because it means the set needs
    // revisiting, not that the abort is fine.
    if let Some(turn) = spawned {
        let action = parsed.get("action").and_then(Value::as_str).unwrap_or("?");
        tracing::error!(
            action,
            "delegated action spawned a turn; DELEGATED_ACTIONS is wrong"
        );
        turn.handle.abort();
    }

    drop(tx);
    let _ = drain.await;
    Ok(())
}

/// Drain the runner's event channel, posting each event to the connection.
/// Stops early (draining the rest without posting) if the client has gone.
async fn forward_events(mut rx: UnboundedReceiver<Value>, poster: ConnectionPoster) {
    let mut connection_open = true;
    while let Some(event) = rx.recv().await {
        if !connection_open {
            // Client gone — keep draining so the sender isn't blocked, but skip
            // the network round-trip for the rest of the turn.
            continue;
        }
        match poster.post(&event).await {
            Ok(true) => {}
            // `Ok(false)` = GoneException; stop posting for the rest of the turn.
            Ok(false) => connection_open = false,
            Err(e) => {
                tracing::warn!(error = %e, "failed to post stream event to connection");
            }
        }
    }
}
