//! Action dispatch — parses a client action envelope and produces the matching
//! server events.
//!
//! Each handler is `async` and forwards events through an
//! `UnboundedSender<serde_json::Value>` (the per-connection outbound sink). The
//! socket task drains the sink and writes each value as a JSON WS text frame, so
//! streaming actions (`send_message`) can emit many events while still being
//! driven from one place.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

use smooth_operator::access_control::AccessContext;
use smooth_operator::adapter::{ConversationUpdate, StorageAdapter};
use smooth_operator::agent_config::{AgentBehaviorConfig, AuthGateHook, AuthLevel};
use smooth_operator::domain::{
    Conversation, Participant, ParticipantType, Platform, Session, SessionStatus,
};
use smooth_operator::identity_intake::IntakeValues;
use smooth_operator::interaction::{InteractionOutcome, InteractionResolution};
use smooth_operator_core::llm_provider::LlmProvider;
use smooth_operator_core::{LlmClient, LlmConfig};

use crate::protocol;
use crate::runner;
use crate::runner::TurnRequest;
use crate::state::AppState;

/// The agent's display name for the reference server.
const AGENT_NAME: &str = "smooth-agent";

/// The per-user read scope of a connection, derived **only** from the
/// connection's authenticated principal — never from a client-supplied frame
/// field (`userEmail` in a create frame is caller-controlled, so trusting it
/// would let anyone assume anyone's scope).
///
/// Org scoping stays where it is; this is the second, per-user dimension —
/// without it any authenticated member of an org can enumerate and open every
/// other member's conversations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserScope {
    /// Auth is not configured for this deployment (`none` / `disabled` /
    /// `local-token`: local, dev, single-user daemon). There is no identity to
    /// scope by, so conversation reads behave exactly as they always have.
    /// **The only unscoped variant.**
    Unscoped,
    /// An authenticated principal carrying an email: reads see that user's
    /// conversations and nothing else.
    User(String),
    /// Auth *is* configured but this connection has no usable user identity
    /// (anonymous/widget connection, or a principal with no `email` claim).
    /// Owns nothing, so it matches no OWNED conversation — but it may still
    /// reach OWNERLESS ones, which are exactly the conversations it creates
    /// itself. Denying those too locked anonymous and emailless principals out
    /// of their own sessions: empty list, resume refused, `send_message`
    /// refused. th-909995.
    Denied,
}

impl UserScope {
    /// The email to stamp on a created session's user participant, when the
    /// principal has one. `None` ⇒ keep whatever the create frame supplied
    /// (only reachable for anonymous/unscoped connections, which own nothing).
    #[must_use]
    pub fn principal_email(&self) -> Option<&str> {
        match self {
            Self::User(email) => Some(email),
            Self::Unscoped | Self::Denied => None,
        }
    }
}

/// Whether a connection authenticated to `auth_org` may touch a row owned by
/// `row_org`.
///
/// `auth_org == None` — an anonymous / tokenless connection, the embeddable
/// widget's normal state — keeps the pre-existing behavior: there is no org to
/// compare against, and the widget must still reach the session it just created.
/// A connection that DID present a verified principal, however, is pinned to
/// that principal's org: it may not read, drive, or mutate another tenant's
/// conversation even if it learns the id. Before this, org was resolved only to
/// *stamp* new sessions — every by-id path (`get_session`, `send_message`,
/// `confirm_tool_action`, `submit_interaction`, `verify_otp`,
/// `rename_conversation`, and conversation resume) was checked for per-user
/// ownership and never for tenant, so an ownerless conversation (the widget
/// default — no `user` participant carrying an email) was reachable by any
/// authenticated user in any org. Feature gap G7.
fn same_org(auth_org: Option<&str>, row_org: &str) -> bool {
    auth_org.is_none_or(|o| o == row_org)
}

/// How the caller arrived at the conversation, which is what decides whether the
/// anonymous exception in [`may_read_conversation`] applies.
///
/// [`Reach::ById`] means the caller already holds an unguessable id — for a
/// public widget visitor that id IS its capability, the only credential it has.
/// [`Reach::Listing`] means `list_conversations` turned the conversation up by
/// enumeration. Letting an identity-less connection past the ownership axis is
/// defensible for the first and never for the second: it must not be handed ids
/// it could not already name. Listing is org-bounded but falls back to the SEED
/// org for an anonymous caller, which is precisely where widget conversations
/// pool — so widening it there would leak visitors' chats to each other.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    ById,
    Listing,
}

/// Whether this connection may read `conversation_id`.
///
/// Two boundaries, outer first. **Tenant**: a connection carrying a verified org
/// may only reach that org's conversations — see [`same_org`] — and that applies
/// to `Unscoped` too, since a single-user flavor still must not reach another
/// tenant. **Ownership**: `Unscoped` (auth not configured) then sees
/// everything. Otherwise the conversation
/// is owner-checked **only if it has an owner** — a `user` participant carrying
/// a non-blank email. A conversation with no such participant (created by an
/// anonymous or emailless principal, or predating ownership) stays readable by
/// everyone, as it was before scoping shipped; fail-closing it instead denied
/// those principals their own sessions (th-909995, and the .NET revert in #309).
/// An owned conversation still needs a matching `User(email)`, so an
/// *authenticated* `Denied` — and any other user — is refused.
///
/// **The anonymous exception (th-anon-owned).** `Denied` covers two very
/// different connections: an authenticated principal whose token carries no
/// `email` claim, and a connection with no verified principal at all — every
/// public widget visitor (see `server::anonymous_scope`). Only the first can
/// meaningfully fail the owner check; the second can NEVER satisfy it, because
/// it has no identity to match with. Applying ownership to it broke the widget
/// outright the moment its pre-chat form started sending `userEmail`: that email
/// lands on the visitor's own `user` participant, makes the conversation
/// `owned`, and the visitor is then locked out of the session it just created —
/// `send_message` answers `SESSION_NOT_FOUND` for a session that plainly exists,
/// and the widget's recovery loop re-creates and is denied identically, forever.
/// This is th-909995 recurring for the emailful case. So an anonymous connection
/// (`auth_org.is_none()` — set only by the tokenless and degraded-token branches
/// of `server::resolve_ws_access`) skips the ownership axis *for a
/// [`Reach::ById`] read only*, and is bounded by session-id unguessability,
/// exactly as it was before scoping shipped. An authenticated emailless
/// principal still fails closed, and [`Reach::Listing`] stays strict for
/// everyone so nothing becomes enumerable.
///
/// A storage error is a denial — an owner check that can't be completed must not
/// pass.
async fn may_read_conversation(
    state: &AppState,
    conversation_id: &str,
    auth_org: Option<&str>,
    scope: &UserScope,
    reach: Reach,
) -> bool {
    // Nothing to check on either axis — skip the participant read entirely, as
    // this function did for `Unscoped` before the tenant check existed.
    if auth_org.is_none() && matches!(scope, UserScope::Unscoped) {
        return true;
    }
    match state
        .storage
        .list_participants_by_conversation(conversation_id)
        .await
    {
        Ok(participants) => {
            // Tenant boundary first — it outranks the per-user one, and applies
            // even to `Unscoped` (a single-user flavor still must not reach
            // another tenant). The conversation's org is read off its
            // participants, which all carry it, so this costs no extra query.
            // A conversation with no participants yet (the create→first-frame
            // race) has no derivable org and is left to the ownership check
            // below, exactly as before.
            if let Some(row_org) = participants.first().map(|p| p.organization_id.as_str()) {
                if !same_org(auth_org, row_org) {
                    return false;
                }
            }
            if matches!(scope, UserScope::Unscoped) {
                return true;
            }
            let owned = participants.iter().any(|p| {
                p.participant_type == smooth_operator::domain::ParticipantType::User
                    && p.email.as_deref().is_some_and(|e| !e.trim().is_empty())
            });
            match scope {
                // Ownerless ⇒ open (see above); owned ⇒ must match.
                UserScope::User(email) => {
                    !owned
                        || participants
                            .iter()
                            .any(|p| smooth_operator::adapter::is_owner(p, email))
                }
                // Ownerless ⇒ open. Owned ⇒ refused for an authenticated
                // emailless principal, and refused for everyone while
                // enumerating — but NOT for an anonymous connection that already
                // named the id, which has no identity the check could ever be
                // satisfied by.
                UserScope::Denied => !owned || (auth_org.is_none() && reach == Reach::ById),
                UserScope::Unscoped => true, // handled above
            }
        }
        Err(_) => false,
    }
}

/// The **only** way a handler may turn a client-supplied `sessionId` into a
/// session. It loads the session and then hides it unless the connection's
/// authenticated principal owns its conversation — returning `Ok(None)`, exactly
/// what an unknown session id returns, so every caller emits the identical
/// not-found event and no caller can distinguish "not yours" from "never
/// existed".
///
/// `Err` is the third outcome and is NOT an existence claim: storage could not
/// answer. Callers emit `STORAGE_ERROR` (retryable) for it instead of
/// not-found — telling a visitor their live session does not exist because
/// Postgres hiccuped is a lie the UI has no way to walk back. It leaks nothing:
/// a storage failure is independent of whether the id is real or ours.
///
/// Every sessionId-taking handler routes through here rather than calling
/// [`AppState::get_session`] directly: the check lives once, at the chokepoint,
/// instead of being re-derived — and forgotten — per handler. `get_session`,
/// `send_message`, `verify_otp`, `confirm_tool_action` and `submit_interaction`
/// each used to load a session by raw id, so any authenticated user who knew or
/// guessed another user's session id could drive a turn in it (and read the
/// replayed history back through their own stream). th-1b7ed0.
async fn scoped_session(
    state: &AppState,
    session_id: &str,
    auth_org: Option<&str>,
    scope: &UserScope,
) -> anyhow::Result<Option<Session>> {
    // th-ca579c: hydrate from storage on a local miss. `get_session` here would
    // report "not found" for a session this pod simply has not seen — which with
    // 2+ replicas is most returning visitors.
    let Some(session) = state.load_session(session_id).await? else {
        return Ok(None);
    };
    // The session carries its own org, so the tenant check needs no extra read
    // and covers every session-id-taking handler at this one chokepoint.
    if !same_org(auth_org, &session.organization_id) {
        return Ok(None);
    }
    Ok(may_read_conversation(
        state,
        &session.conversation_id,
        auth_org,
        scope,
        Reach::ById,
    )
    .await
    .then_some(session))
}

/// The `error` event for a session read that storage could not answer. Distinct
/// from the not-found event on purpose: this one says "try again", and the code
/// (`STORAGE_ERROR`, already used by the rename path) is what a client keys on
/// to retry rather than to clear its session.
fn session_storage_error(request_id: Option<&str>, session_id: &str, e: &anyhow::Error) -> Value {
    tracing::warn!(error = %e, session_id, "session lookup unavailable");
    protocol::error(
        request_id,
        "STORAGE_ERROR",
        "session lookup is temporarily unavailable, please try again",
    )
}

/// A spawned agent turn: its task handle, plus the flag the cancel path raises to
/// make `cancelled` genuinely terminal.
///
/// `JoinHandle::abort()` alone is not enough. It only takes effect when the task
/// next yields to the runtime, so a turn already past its final `.await` runs its
/// whole tail — dispatching an OTP, emitting `eventual_response` — AFTER the client
/// has been told the turn was cancelled. That race put both a `cancelled` (499) and
/// an `eventual_response` (200) on the wire for one requestId, and could put a real
/// verification code in front of a real person after they hit Stop.
///
/// The flag is raised BEFORE the abort and before the `cancelled` event goes out, and
/// the turn tail re-reads it immediately before each side effect. A late abort is then
/// harmless: the tail sees the flag and produces nothing.
pub struct SpawnedTurn {
    /// The turn's task handle — aborted by the cancel path and the disconnect path.
    pub handle: tokio::task::JoinHandle<()>,
    /// Raised before `cancelled` is emitted; read by the turn tail before every
    /// side effect it would otherwise perform.
    pub cancelled: Arc<AtomicBool>,
}

/// Parse and dispatch a single inbound text frame. Any produced events are sent
/// through `sink`. Protocol-level failures are surfaced as `error` events, never
/// as hard errors that drop the connection.
///
/// Returns the [`SpawnedTurn`] for a `send_message` frame (so the connection loop
/// can track the single active turn, abort it on a `cancel` frame or disconnect,
/// and raise its cancelled flag so the turn's tail stays silent); `None` for every
/// other action. The turn is spawned — not awaited inline — because a
/// confirmation-gated turn parks awaiting a later `confirm_tool_action` frame the
/// same reader must be free to receive.
///
/// Note: `cancel` is NOT dispatched here. Cancellation is connection-local state
/// (it aborts the tracked turn handle), so [`crate::server`]'s reader loop handles
/// the `cancel` action directly before delegating other frames here.
#[allow(clippy::too_many_arguments)]
pub async fn handle_frame(
    state: &AppState,
    access: &AccessContext,
    conn_id: &str,
    origin: Option<&str>,
    auth_org: Option<&str>,
    scope: &UserScope,
    raw: &str,
    sink: &UnboundedSender<Value>,
) -> Option<SpawnedTurn> {
    let parsed: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            let _ = sink.send(protocol::error(
                None,
                "VALIDATION_ERROR",
                &format!("invalid JSON frame: {e}"),
            ));
            return None;
        }
    };

    let action = parsed.get("action").and_then(Value::as_str);
    let request_id = parsed.get("requestId").and_then(Value::as_str);

    match action {
        Some("ping") => {
            let _ = sink.send(protocol::pong(request_id));
            None
        }
        Some("create_conversation_session") => {
            handle_create_session(
                state, conn_id, origin, auth_org, scope, &parsed, request_id, sink,
            )
            .await;
            None
        }
        Some("get_session") => {
            handle_get_session(state, auth_org, scope, &parsed, request_id, sink).await;
            None
        }
        Some("get_conversation_messages") => {
            handle_get_conversation_messages(state, auth_org, scope, &parsed, request_id, sink)
                .await;
            None
        }
        Some("list_conversations") => {
            handle_list_conversations(state, auth_org, scope, &parsed, request_id, sink).await;
            None
        }
        Some("rename_conversation") => {
            handle_rename_conversation(state, auth_org, scope, &parsed, request_id, sink).await;
            None
        }
        // The only action that spawns a turn — its handle flows back to the reader
        // loop so a later `cancel` (or a disconnect) can abort it.
        Some("send_message") => {
            handle_send_message(state, auth_org, access, scope, &parsed, request_id, sink).await
        }
        // May ALSO spawn a turn (th-db0816): resolving a DURABLE pending
        // confirmation — the parked turn lived on another pod, or died — spawns
        // a continuation turn to carry out the approved action, and its handle
        // flows back for `cancel`/disconnect exactly like `send_message`'s.
        Some("confirm_tool_action") => {
            handle_confirm_tool_action(state, auth_org, access, scope, &parsed, request_id, sink)
                .await
        }
        Some("verify_otp") => {
            handle_verify_otp(state, auth_org, scope, &parsed, request_id, sink).await;
            None
        }
        Some("submit_interaction") => {
            handle_submit_interaction(state, auth_org, scope, &parsed, request_id, sink).await;
            None
        }
        Some(other) => {
            let _ = sink.send(protocol::error(
                request_id,
                "UNSUPPORTED_ACTION",
                &format!("action '{other}' is not supported by this server"),
            ));
            None
        }
        None => {
            let _ = sink.send(protocol::error(
                request_id,
                "VALIDATION_ERROR",
                "missing 'action' field",
            ));
            None
        }
    }
}

/// Outcome of widget-auth enforcement: whether to proceed, and (when an agent
/// policy resolved) the org that policy attributes the agent to.
enum WidgetAuthOutcome {
    /// Auth denied — an `error` event was already emitted; the caller must stop.
    Denied,
    /// Auth passed. `org_id` is `Some` when the resolved policy carried an
    /// `organization_id` (a multi-tenant host that knows the agent's org), else
    /// `None` (no policy, or a policy without an org — org derivation falls
    /// through to the JWT principal, then the seed org).
    Allowed { org_id: Option<String> },
}

/// Enforce an agent's embeddable-widget policy (origin allowlist + `authContext`)
/// before a session is created. Returns [`WidgetAuthOutcome::Allowed`] to proceed
/// (carrying the policy's org when known), or [`WidgetAuthOutcome::Denied`] after
/// emitting a protocol `error` (the caller must then stop). Agents with no policy
/// proceed — unless `WIDGET_AUTH_STRICT` is set, in which case an unknown agent is
/// rejected (fail closed).
async fn enforce_widget_auth(
    state: &AppState,
    origin: Option<&str>,
    agent_id: &str,
    parsed: &Value,
    request_id: Option<&str>,
    sink: &UnboundedSender<Value>,
) -> WidgetAuthOutcome {
    let Some(policy) = state.widget_auth.agent_widget_auth(agent_id).await else {
        if state.config.widget_auth_strict {
            let _ = sink.send(protocol::error(
                request_id,
                "AGENT_NOT_AUTHORIZED",
                "this agent is not registered for embedding",
            ));
            return WidgetAuthOutcome::Denied;
        }
        return WidgetAuthOutcome::Allowed { org_id: None };
    };

    // Origin allowlist — fail closed: a missing or disallowed `Origin` is rejected.
    if !smooth_operator::widget_auth::origin_allowed(
        &policy.allowed_origins,
        origin.unwrap_or_default(),
    ) {
        let _ = sink.send(protocol::error(
            request_id,
            "ORIGIN_NOT_ALLOWED",
            "this origin is not allowed to embed this agent",
        ));
        return WidgetAuthOutcome::Denied;
    }

    // Pre-auth `authContext` (optional): when present it must verify.
    if let Some(ac) = parsed.get("authContext") {
        if !verify_auth_context_value(policy.public_key.as_deref(), ac) {
            let _ = sink.send(protocol::error(
                request_id,
                "AUTH_CONTEXT_INVALID",
                "authContext signature failed verification",
            ));
            return WidgetAuthOutcome::Denied;
        }
    }
    WidgetAuthOutcome::Allowed {
        org_id: policy.organization_id,
    }
}

/// Verify a JSON `authContext` (`{userId, signature, timestamp}`) against the
/// agent's `public_key`. False on any missing field/key or signature/replay
/// failure. Replay window: 60s.
fn verify_auth_context_value(public_key: Option<&str>, ac: &Value) -> bool {
    let (Some(pk), Some(user_id), Some(signature), Some(timestamp)) = (
        public_key,
        ac.get("userId").and_then(Value::as_str),
        ac.get("signature").and_then(Value::as_str),
        ac.get("timestamp").and_then(Value::as_i64),
    ) else {
        return false;
    };
    let now = chrono::Utc::now().timestamp();
    smooth_operator::widget_auth::verify_auth_context(pk, user_id, signature, timestamp, now, 60)
}

/// `create_conversation_session` — create a conversation + user & agent
/// participants + a session, then reply with an `immediate_response` carrying
/// the session descriptor (per `create-conversation-session.schema.json`).
#[allow(clippy::too_many_arguments)]
async fn handle_create_session(
    state: &AppState,
    conn_id: &str,
    origin: Option<&str>,
    auth_org: Option<&str>,
    scope: &UserScope,
    parsed: &Value,
    request_id: Option<&str>,
    sink: &UnboundedSender<Value>,
) {
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
        let _ = sink.send(protocol::error(
            request_id,
            "VALIDATION_ERROR",
            "missing 'agentId'",
        ));
        return;
    };

    // Embeddable-widget auth: enforce the agent's origin allowlist + authContext
    // before creating any session. No-op for agents without a policy (unless
    // WIDGET_AUTH_STRICT). On denial, an error is emitted and we stop here. A
    // resolved policy may also carry the agent's org (multi-tenant host).
    let widget_org =
        match enforce_widget_auth(state, origin, &agent_id, parsed, request_id, sink).await {
            WidgetAuthOutcome::Denied => return,
            WidgetAuthOutcome::Allowed { org_id } => org_id,
        };

    let user_name = parsed
        .get("userName")
        .and_then(Value::as_str)
        .unwrap_or("Visitor")
        .to_string();
    // The session's user email — the key every conversation read is scoped by.
    // An authenticated principal's email ALWAYS wins over the frame's
    // `userEmail`: the frame field is caller-controlled, so honoring it would
    // let anyone mint a conversation under someone else's scope (and read it
    // back from their sidebar). The frame value is used only when the
    // connection has no principal email at all — the anonymous widget flow,
    // which owns nothing and can list nothing.
    let user_email = scope.principal_email().map(str::to_string).or_else(|| {
        parsed
            .get("userEmail")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let browser_fingerprint = parsed
        .get("browserFingerprint")
        .and_then(Value::as_str)
        .map(str::to_string);

    let now = chrono::Utc::now();

    // Resume: when the caller passes a `conversationId` for a conversation that
    // exists, bind this new session to it (reuse its id + org, skip
    // `create_conversation`) so subsequent `send_message` appends to it and the
    // runner replays its history by `thread_id`. Absent/unknown id → mint a fresh
    // conversation (byte-for-byte unchanged behavior).
    //
    // Ownership: a conversation this connection may NOT read is treated exactly
    // like an id that never existed — the resume is ignored and a fresh
    // conversation is minted, byte-for-byte the unknown-id path. Returning a
    // distinct denial here would be an existence oracle: it would confirm which
    // conversation ids are real, letting a caller enumerate other users'
    // conversations by their ids alone.
    // SMOODEV-3057: a reconnect may name a conversation whose create is still
    // parked (opened the widget, socket blipped, never typed). Land it first so
    // the resume below finds it — otherwise the reconnect mints a fresh
    // conversation and loses the durable `supports` record it should inherit.
    // Best-effort: on failure the resume simply falls through to the unknown-id
    // path, which is the pre-existing behavior for a conversation that is gone.
    if let Some(cid) = parsed
        .get("conversationId")
        .and_then(Value::as_str)
        .filter(|cid| !cid.is_empty())
    {
        if let Err(e) = state.materialize_conversation(cid).await {
            tracing::warn!(error = %e, conversation_id = cid, "deferred conversation could not be landed for resume");
        }
    }

    let resume = match parsed.get("conversationId").and_then(Value::as_str) {
        Some(cid)
            if !cid.is_empty()
                && may_read_conversation(state, cid, auth_org, scope, Reach::ById).await =>
        {
            state.storage.get_conversation(cid).await.ok().flatten()
        }
        _ => None,
    };

    // Derive the org this session (conversation + participants) belongs to. When
    // resuming, it's the existing conversation's org (keeps the session
    // self-consistent). Otherwise, in priority order:
    //   1. the widget policy's `organization_id` — a multi-tenant host that knows
    //      the agent's org (widget visitors authenticate via origin/authContext,
    //      not a JWT, so their org rides on the agent's policy);
    //   2. the connection's authenticated JWT principal org (`auth_org`) — a
    //      dashboard user / authed client;
    //   3. the server's seed org — the single-org reference/dev case, so the
    //      admin API's org-scoping (document sets, indexing runs) still lines up
    //      with the seeded knowledge. This keeps the no-auth/local flavor
    //      behavior unchanged.
    let org_id = if let Some(ref c) = resume {
        c.organization_id.clone()
    } else {
        widget_org
            .or_else(|| auth_org.map(str::to_string))
            .unwrap_or_else(|| crate::server::SEED_ORG_ID.to_string())
    };

    let conversation_id = resume
        .as_ref()
        .map(|c| c.id.clone())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let session_id = uuid::Uuid::new_v4().to_string();
    let user_participant_id = uuid::Uuid::new_v4().to_string();
    let agent_participant_id = uuid::Uuid::new_v4().to_string();

    // Associate this connection with its session (and agent) on the backplane so
    // events published to the session/agent — by an agent turn or any other
    // service — reach this client's socket, on this pod or (with a Redis/NATS
    // backplane) any pod.
    state
        .backplane
        .associate(
            conn_id,
            smooth_operator::backplane::Target::Session(session_id.clone()),
        )
        .await;
    state
        .backplane
        .associate(
            conn_id,
            smooth_operator::backplane::Target::Agent(agent_id.clone()),
        )
        .await;

    // Client render capabilities (`supports`, per
    // create-conversation-session.schema.json) — the per-kind list gating which
    // Rich Interactions this session gets as parked cards (e.g. `identity_form`
    // for kind `identity_intake`); kinds without their capability degrade to
    // the conversational fallback. Unknown values are kept (forward-compatible:
    // a future kind may gate on them). `None` = the frame omitted the key
    // entirely, which is what the inherit-on-resume rule below keys on; an
    // explicit `[]` is a declaration of "I render nothing" and never inherits.
    let declared: Option<Vec<String>> =
        parsed
            .get("supports")
            .and_then(Value::as_array)
            .map(|caps| {
                caps.iter()
                    .filter_map(|c| c.as_str().map(str::to_string))
                    .collect()
            });

    // A RECONNECT is a resume: the client re-opens the socket and re-issues
    // `create_conversation_session` with the same `conversationId`, which mints a
    // NEW session id. Capabilities that live only on the session therefore have
    // to be re-declared on every reconnect or Rich Interactions silently stop
    // being offered — the shipped feature degrades with no error anyone sees
    // (th-13df6d). So the declared set is persisted on the CONVERSATION, the same
    // durability the workflow step pointer moved to for the same reason
    // (th-c12df5: the session registry is per-pod and resets on reconnect/pod
    // hop), and a resume that omits `supports` inherits it.
    //
    // A resume that declares (including `[]`) always wins, so a text-only client
    // resuming a rich conversation opts out by sending `"supports": []`. The
    // fallback direction is bounded either way: an offered card a client can't
    // render times out (`INTERACTION_TIMEOUT`) into the same conversational
    // fallback the capability gate would have chosen.
    let supports: Vec<String> = match (&declared, &resume) {
        (Some(caps), _) => caps.clone(),
        (None, Some(c)) => conversation_supports(c.metadata_json.as_ref()),
        (None, None) => Vec::new(),
    };

    // Only mint a conversation on a fresh session — a resume reuses the existing
    // one (and its persisted history), so `create_conversation` is skipped.
    let conversation = resume.is_none().then(|| Conversation {
        id: conversation_id.clone(),
        platform: Platform::Web,
        name: format!("Session {session_id}"),
        organization_id: org_id.clone(),
        idempotency_key: session_id.clone(),
        metadata_json: with_client_supports(parsed.get("metadata").cloned(), &supports),
        analytics_json: None,
        created_at: now,
        updated_at: now,
    });

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
        email: user_email.clone(),
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

    // Stash the caller's OTP contact on the session so the end_user auth-gate
    // flow can offer verification without a storage roundtrip (mirrors how the
    // workflow step pointer lives in session metadata). The reference create path
    // captures only an email; a host that also captures a phone would add
    // `contactPhone` here for an SMS channel. The declared render capabilities
    // (`supports`) ride the same metadata map.
    let session_metadata = {
        let mut meta = std::collections::HashMap::new();
        if let Some(email) = user_email.as_ref() {
            meta.insert("contactEmail".to_string(), Value::from(email.clone()));
        }
        if !supports.is_empty() {
            meta.insert("supports".to_string(), Value::from(supports.clone()));
        }
        (!meta.is_empty()).then_some(meta)
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
        // replaying this conversation's persisted message log (see runner.rs).
        thread_id: conversation_id.clone(),
        status: Some(SessionStatus::Active),
        token_count: Some(0),
        message_count: Some(0),
        metadata: session_metadata,
        created_at: Some(now),
        updated_at: Some(now),
        ended_at: None,
        last_activity_at: Some(now),
    };

    // Persist to the storage adapter (best-effort: a failure surfaces as error).
    let storage = state.storage.clone();
    let sink_clone = sink.clone();
    let request_id_owned = request_id.map(str::to_string);
    let session_for_registry = session.clone();
    let state_clone = state.clone();
    let conversation_id_owned = conversation_id.clone();
    let redeclared = declared.is_some();

    let data = json!({
        "sessionId": session_id,
        "conversationId": conversation_id,
        "agentId": agent_id,
        "agentName": AGENT_NAME,
        "userParticipantId": user_participant_id,
        "agentParticipantId": agent_participant_id,
    });

    // SMOODEV-3057: a bare widget open writes NOTHING until it earns it. Opening
    // the widget used to mint a `conversations` row on the spot — 44 of 117 web
    // conversations in a 30-day production sample had zero messages, every one of
    // them an inbox row for a visitor who never typed — and a web create feeds a
    // fresh UUID as the conversation's `idempotency_key`, so the unique index can
    // never collapse a double-connect the way it does for sms/slack/discord.
    // Parking the whole create until the first message removes the empty rows and
    // makes a double-connect harmless.
    //
    // An open that already carries visitor identity is NOT bare — it is a
    // captured lead. A host adapter may hook the `user` participant write to
    // upsert that visitor into its CRM (SmooAI's `chat-storage::crm_capture`
    // does, reading phone + marketing consent off the conversation's
    // `metadata_json`), and deferring those would silently stop capturing a
    // pre-chat form submit from someone who then closed the tab. So identity —
    // an authenticated principal's email, the frame's `userEmail`, or ADR-048's
    // `metadata.userPhone` — persists immediately, exactly as before, as does
    // every non-web channel and every resume.
    let carries_identity = user_email.is_some()
        || parsed
            .get("metadata")
            .and_then(|m| m.get("userPhone"))
            .and_then(Value::as_str)
            .is_some_and(|phone| !phone.trim().is_empty());

    // ponytail: `clone` rather than restructure the eager path around a partial
    // move — one Conversation per session create is not worth the churn.
    if let Some(conversation) = conversation.clone().filter(|_| !carries_identity) {
        state.defer_session(crate::state::PendingSession {
            session_id: session_id.clone(),
            conn_id: conn_id.to_string(),
            conversation: Some(conversation),
            user_participant: Some(user_participant),
            agent_participant: Some(agent_participant),
            session: Some(session),
        });
        // The registry is what `send_message` resolves the session through
        // (`load_session` reads it before storage), and a WebSocket stays pinned
        // to this pod for its life, so the create→first-message window is served
        // locally. A reconnect that lands elsewhere finds nothing and mints a
        // fresh session — which is correct: there was no conversation to resume.
        state.insert_session(session_for_registry);
        let _ = sink.send(protocol::immediate_response(
            request_id,
            200,
            "Session created",
            data,
        ));
        return;
    }

    tokio::spawn(async move {
        let rid = request_id_owned.as_deref();
        if let Some(conversation) = conversation {
            if let Err(e) = storage.create_conversation(conversation).await {
                let _ = sink_clone.send(protocol::error(
                    rid,
                    "INTERNAL_ERROR",
                    &format!("create conversation failed: {e}"),
                ));
                return;
            }
        } else if redeclared {
            // A resume that re-declared `supports`: refresh the durable set so the
            // NEXT reconnect — which may omit the key — inherits what this client
            // actually renders. (A fresh conversation carries it in the metadata
            // it was created with, so this write is the resume path only.)
            persist_client_supports(storage.as_ref(), &conversation_id_owned, &supports).await;
        }
        if let Err(e) = storage.add_participant(user_participant).await {
            let _ = sink_clone.send(protocol::error(
                rid,
                "INTERNAL_ERROR",
                &format!("add user participant failed: {e}"),
            ));
            return;
        }
        if let Err(e) = storage.add_participant(agent_participant).await {
            let _ = sink_clone.send(protocol::error(
                rid,
                "INTERNAL_ERROR",
                &format!("add agent participant failed: {e}"),
            ));
            return;
        }
        if let Err(e) = storage.create_session(session).await {
            let _ = sink_clone.send(protocol::error(
                rid,
                "INTERNAL_ERROR",
                &format!("create session failed: {e}"),
            ));
            return;
        }
        state_clone.insert_session(session_for_registry);
        let _ = sink_clone.send(protocol::immediate_response(
            rid,
            200,
            "Session created",
            data,
        ));
    });
}

/// `get_session` — return the session snapshot (per `get-session.schema.json`).
async fn handle_get_session(
    state: &AppState,
    auth_org: Option<&str>,
    scope: &UserScope,
    parsed: &Value,
    request_id: Option<&str>,
    sink: &UnboundedSender<Value>,
) {
    let Some(session_id) = parsed.get("sessionId").and_then(Value::as_str) else {
        let _ = sink.send(protocol::error(
            request_id,
            "VALIDATION_ERROR",
            "missing 'sessionId'",
        ));
        return;
    };

    match scoped_session(state, session_id, auth_org, scope).await {
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
            let _ = sink.send(protocol::immediate_response(
                request_id, 200, "Session", data,
            ));
        }
        Ok(None) => {
            let _ = sink.send(protocol::error(
                request_id,
                "SESSION_NOT_FOUND",
                &format!("session '{session_id}' not found"),
            ));
        }
        Err(e) => {
            let _ = sink.send(session_storage_error(request_id, session_id, &e));
        }
    }
}

/// `get_conversation_messages` — paginated message history for a session's
/// conversation. Wraps the storage adapter's `list_messages_by_conversation`
/// (the same call the admin API + the turn runner use) and replies with an
/// `immediate_response` carrying `{ conversationId, messages, nextCursor, hasMore }`.
///
/// Optional inputs: `limit` (default 50) and an opaque `cursor` from a prior
/// page's `nextCursor`. Newest-first (the common "recent history" read).
async fn handle_get_conversation_messages(
    state: &AppState,
    auth_org: Option<&str>,
    scope: &UserScope,
    parsed: &Value,
    request_id: Option<&str>,
    sink: &UnboundedSender<Value>,
) {
    let Some(session_id) = parsed.get("sessionId").and_then(Value::as_str) else {
        let _ = sink.send(protocol::error(
            request_id,
            "VALIDATION_ERROR",
            "missing 'sessionId'",
        ));
        return;
    };
    // A session belonging to ANOTHER user is reported with the identical
    // not-found event a session id that never existed produces — same code,
    // same message, same shape. A distinct "forbidden" would be an existence
    // oracle: it would tell a caller which session ids are real, which is all
    // an attacker needs to enumerate other users' conversations.
    let session = match scoped_session(state, session_id, auth_org, scope).await {
        Ok(Some(session)) => session,
        Ok(None) => {
            let _ = sink.send(protocol::error(
                request_id,
                "SESSION_NOT_FOUND",
                &format!("session '{session_id}' not found"),
            ));
            return;
        }
        Err(e) => {
            let _ = sink.send(session_storage_error(request_id, session_id, &e));
            return;
        }
    };

    const DEFAULT_LIMIT: usize = 50;
    let limit = parsed
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_LIMIT);
    let cursor = parsed
        .get("cursor")
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut query = smooth_operator::adapter::MessageQuery::new(&session.conversation_id, limit);
    query.cursor = cursor;
    query.descending = true;

    match state.storage.list_messages_by_conversation(query).await {
        Ok(page) => {
            let data = json!({
                "conversationId": session.conversation_id,
                "messages": page.messages,
                "nextCursor": page.next_cursor,
                "hasMore": page.next_cursor.is_some(),
            });
            let _ = sink.send(protocol::immediate_response(
                request_id,
                200,
                "ConversationMessages",
                data,
            ));
        }
        Err(e) => {
            let _ = sink.send(protocol::error(
                request_id,
                "STORAGE_ERROR",
                &format!("failed to list messages: {e}"),
            ));
        }
    }
}

/// `list_conversations` — the conversation-sidebar / resume substrate. Returns
/// the org's conversations that have at least one message, most-recent-first,
/// each with a short title preview + message count. Empty conversations (every
/// page-load currently mints one) are filtered out so the sidebar isn't buried
/// in blanks. Reply is an `immediate_response` carrying `{ conversations: [ {
/// conversationId, title, updatedAt, messageCount } ] }`.
///
/// Optional input: `limit` (default 50) — the max conversations returned after
/// filtering + sorting.
async fn handle_list_conversations(
    state: &AppState,
    auth_org: Option<&str>,
    scope: &UserScope,
    parsed: &Value,
    request_id: Option<&str>,
    sink: &UnboundedSender<Value>,
) {
    const DEFAULT_LIMIT: usize = 50;
    let limit = parsed
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_LIMIT);

    // Org scope: the authenticated principal's org, else the seed org — matching
    // the create-session derivation's fallback for the local/no-auth flavor.
    let org_id = auth_org.unwrap_or(crate::server::SEED_ORG_ID);

    // User scope, on top of (never instead of) the org scope, applied below via
    // `may_read_conversation` — the SAME predicate the session reads use, so the
    // list can never disagree with what `get_session` will hand over. It runs
    // before the limit, so a page is never silently short.
    let listed = state.storage.list_conversations_by_org(org_id).await;

    let conversations = match listed {
        Ok(c) => c,
        Err(e) => {
            let _ = sink.send(protocol::error(
                request_id,
                "STORAGE_ERROR",
                &format!("failed to list conversations: {e}"),
            ));
            return;
        }
    };

    // Peek each conversation's messages for a preview + count, dropping empties.
    // ponytail: per-conversation peek + owner check, capped at MSG_CAP — fine for
    // a local daemon's ~100 convos. If this ever fronts a multi-thousand-conversation
    // org, push count + first-inbound + the owner filter down into the storage
    // adapter as one query (`list_conversations_by_org_and_user` is that pushdown
    // for the owned half; it can't express "or ownerless" yet).
    const MSG_CAP: usize = 200;
    let mut rows: Vec<(i64, Value)> = Vec::new();
    for conv in conversations {
        if !may_read_conversation(state, &conv.id, auth_org, scope, Reach::Listing).await {
            continue;
        }
        let mut query = smooth_operator::adapter::MessageQuery::new(&conv.id, MSG_CAP);
        query.descending = false; // oldest-first: the first inbound is the title source
        let Ok(page) = state.storage.list_messages_by_conversation(query).await else {
            continue;
        };
        if page.messages.is_empty() {
            continue;
        }
        rows.push((
            conv.updated_at.timestamp_millis(),
            json!({
                "conversationId": conv.id,
                "title": conversation_title(&page.messages, &conv.name),
                "updatedAt": conv.updated_at.to_rfc3339(),
                "messageCount": page.messages.len(),
            }),
        ));
    }

    // Most-recent-first, then cap.
    rows.sort_by_key(|(ts, _)| std::cmp::Reverse(*ts));
    let conversations: Vec<Value> = rows.into_iter().take(limit).map(|(_, v)| v).collect();

    let _ = sink.send(protocol::immediate_response(
        request_id,
        200,
        "Conversations",
        json!({ "conversations": conversations }),
    ));
}

/// Derive a sidebar title. A **meaningful** conversation `name` — an auto-title
/// or a manual rename, i.e. anything not the default `Session <uuid>` — wins, so
/// titles set by [`maybe_auto_title`] / [`handle_rename_conversation`] surface in
/// the sidebar. Otherwise fall back to a truncated preview of the FIRST inbound
/// (user) message, then the default name. `messages` is oldest-first.
///
/// Back-compat: every pre-titling conversation carried the default name, so this
/// is byte-for-byte the old message-preview behavior for them.
fn conversation_title(messages: &[smooth_operator::domain::Message], name: &str) -> String {
    if !name.starts_with(DEFAULT_NAME_PREFIX) && !name.trim().is_empty() {
        return truncate_preview(name, TITLE_MAX);
    }
    messages
        .iter()
        .find(|m| matches!(m.direction, smooth_operator::domain::Direction::Inbound))
        .and_then(message_text)
        .map_or_else(|| name.to_string(), |t| truncate_preview(&t, TITLE_MAX))
}

/// Flat text of a message: the content's `text` mirror, else the first text item.
/// `None` when blank.
fn message_text(m: &smooth_operator::domain::Message) -> Option<String> {
    m.content
        .text
        .clone()
        .or_else(|| m.content.items.iter().find_map(|i| i.text.clone()))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Truncate to `max` characters (char-safe), appending `…` when clipped.
fn truncate_preview(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let clipped: String = s.chars().take(max).collect();
    format!("{}…", clipped.trim_end())
}

/// The default conversation name minted at create-session time
/// (`Session <uuid>`). The auto-titler only fires while a conversation still
/// carries this prefix, so a manual rename (or a prior successful auto-title) is
/// never clobbered.
const DEFAULT_NAME_PREFIX: &str = "Session ";

/// Fast/cheap model used to auto-title a conversation from its first exchange.
const AUTO_TITLE_MODEL: &str = "groq-gpt-oss-20b";

/// Max characters of a title (both auto-generated and manually set).
const TITLE_MAX: usize = 60;

/// `rename_conversation` — set a conversation's `name` to a caller-supplied
/// title. Per the daemon-sidebar rename affordance the client sends
/// `{ action, requestId, conversationId, title }`. The title is sanitized/trimmed
/// and rejected when empty; on success the conversation row's `name` is persisted
/// (which `list_conversations` surfaces as the sidebar title, since it prefers
/// `name` over the first-message preview). Replies with an `immediate_response`
/// (200) carrying `{ conversationId, title }`.
async fn handle_rename_conversation(
    state: &AppState,
    auth_org: Option<&str>,
    scope: &UserScope,
    parsed: &Value,
    request_id: Option<&str>,
    sink: &UnboundedSender<Value>,
) {
    let Some(conversation_id) = parsed.get("conversationId").and_then(Value::as_str) else {
        let _ = sink.send(protocol::error(
            request_id,
            "VALIDATION_ERROR",
            "rename_conversation requires a 'conversationId'",
        ));
        return;
    };

    let title = sanitize_title(
        parsed
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    if title.is_empty() {
        let _ = sink.send(protocol::error(
            request_id,
            "VALIDATION_ERROR",
            "rename_conversation requires a non-empty 'title'",
        ));
        return;
    }

    // The conversation must exist AND be ours — this action takes a raw
    // conversation id, so without the owner check any authenticated user could
    // retitle another user's conversation. Not-ours is reported exactly as
    // never-existed.
    match state.storage.get_conversation(conversation_id).await {
        Ok(Some(_))
            if may_read_conversation(state, conversation_id, auth_org, scope, Reach::ById)
                .await => {}
        Ok(_) => {
            let _ = sink.send(protocol::error(
                request_id,
                "CONVERSATION_NOT_FOUND",
                &format!("conversation '{conversation_id}' not found"),
            ));
            return;
        }
        Err(e) => {
            let _ = sink.send(protocol::error(
                request_id,
                "STORAGE_ERROR",
                &format!("failed to load conversation: {e}"),
            ));
            return;
        }
    }

    let update = ConversationUpdate {
        name: Some(title.clone()),
        ..Default::default()
    };
    match state
        .storage
        .update_conversation(conversation_id, update)
        .await
    {
        Ok(_) => {
            let _ = sink.send(protocol::immediate_response(
                request_id,
                200,
                "Conversation renamed",
                json!({ "conversationId": conversation_id, "title": title }),
            ));
        }
        Err(e) => {
            let _ = sink.send(protocol::error(
                request_id,
                "STORAGE_ERROR",
                &format!("failed to rename conversation: {e}"),
            ));
        }
    }
}

/// Best-effort auto-title: after the first assistant turn on a conversation
/// still carrying the default `Session <uuid>` name, ask a small/cheap model for
/// a short title and persist it as the conversation `name`. **Non-blocking &
/// fail-safe** — spawned detached off the turn path and any failure (no key,
/// gateway error, empty output, storage error) simply leaves the default name.
/// The default-name guard means a manual [`handle_rename_conversation`] rename is
/// never overwritten, and once a title lands the conversation is no longer
/// default-named so it won't re-fire.
pub async fn maybe_auto_title(
    state: &AppState,
    conversation_id: &str,
    user_message: &str,
    reply: &str,
) {
    // Only title conversations still on their default name.
    let conversation = match state.storage.get_conversation(conversation_id).await {
        Ok(Some(c)) => c,
        _ => {
            tracing::warn!(conversation_id, "auto-title: conversation not found");
            return;
        }
    };
    if !conversation.name.starts_with(DEFAULT_NAME_PREFIX) {
        // Expected on every turn after the first (or after a manual rename) — debug.
        tracing::debug!(conversation_id, name = %conversation.name, "auto-title: name not default, skip");
        return;
    }

    // Resolve the org's gateway key (per-org resolver, else the env key). No key
    // ⇒ no title (same gate as a live turn).
    let key = smooth_operator::gateway_key::resolve_gateway_key(
        &state.gateway_key_resolver,
        &conversation.organization_id,
        state.config.gateway_key.as_deref(),
    )
    .await;
    let Some(key) = key else {
        tracing::warn!(org = %conversation.organization_id, "auto-title: no gateway key resolved");
        return;
    };

    let Some(raw) = generate_title(&state.config.gateway_url, &key, user_message, reply).await
    else {
        tracing::warn!("auto-title: generate_title returned None (gateway/parse)");
        return;
    };
    let title = sanitize_title(&raw);
    if title.is_empty() {
        tracing::warn!(raw = %raw, "auto-title: sanitized title empty");
        return;
    }
    tracing::debug!(conversation_id, title = %title, "auto-title: writing title");

    // Re-check the guard right before writing: a manual rename could have landed
    // while the model was thinking. Best-effort — a lost race just means the
    // manual name wins, which is the desired precedence.
    if let Ok(Some(c)) = state.storage.get_conversation(conversation_id).await {
        if !c.name.starts_with(DEFAULT_NAME_PREFIX) {
            return;
        }
    }
    let update = ConversationUpdate {
        name: Some(title),
        ..Default::default()
    };
    let _ = state
        .storage
        .update_conversation(conversation_id, update)
        .await;
}

/// The title model (`groq-gpt-oss-20b`) is a reasoning model: reasoning tokens
/// count against `max_tokens`, so a tight cap (the original 32) gets fully
/// consumed by reasoning and leaves the content empty — the auto-titler then
/// silently produced nothing. Give reasoning headroom; the title itself is
/// capped to `TITLE_MAX` chars by [`sanitize_title`] regardless.
const AUTO_TITLE_MAX_TOKENS: u32 = 512;

/// Build the `/chat/completions` request body for the auto-titler. Extracted so
/// the token budget (the thing that broke) is unit-testable without a gateway.
fn title_request_body(user_message: &str, reply: &str) -> Value {
    let user_snippet: String = user_message.chars().take(500).collect();
    let reply_snippet: String = reply.chars().take(500).collect();
    let prompt = format!(
        "Give this conversation a short 3-6 word title. Reply with ONLY the title, no quotes.\n\nUser: {user_snippet}\nAssistant: {reply_snippet}"
    );
    json!({
        "max_tokens": AUTO_TITLE_MAX_TOKENS,
        "model": AUTO_TITLE_MODEL,
        "temperature": 0.3,
        "messages": [{ "role": "user", "content": prompt }],
    })
}

/// Call the gateway's `/chat/completions` with the small title model over the
/// first exchange, returning the model's raw title text (unsanitized). `None` on
/// any transport / non-2xx / decode failure or a missing content field — the
/// caller treats that as "no title". Inputs are truncated so the prompt stays
/// cheap regardless of how long the exchange ran.
async fn generate_title(
    gateway_url: &str,
    key: &str,
    user_message: &str,
    reply: &str,
) -> Option<String> {
    let url = format!("{}/chat/completions", gateway_url.trim_end_matches('/'));
    let resp: Value = reqwest::Client::new()
        .post(&url)
        .bearer_auth(key)
        .json(&title_request_body(user_message, reply))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    resp.get("choices")?
        .get(0)?
        .get("message")?
        .get("content")?
        .as_str()
        .map(str::to_string)
}

/// Sanitize a conversation title (auto-generated or manually supplied): collapse
/// all whitespace/newlines to single spaces, strip wrapping quotes / markdown
/// emphasis the model sometimes adds (`"`, `'`, `*`, `` ` ``, `#`), and cap at
/// [`TITLE_MAX`] chars (char-safe). Returns an empty string for blank/whitespace
/// input so callers can reject it.
fn sanitize_title(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_matches(|c: char| matches!(c, '"' | '\'' | '*' | '`' | '#' | ' '));
    trimmed
        .chars()
        .take(TITLE_MAX)
        .collect::<String>()
        .trim()
        .to_string()
}

/// `send_message` — ack with `immediate_response` (202), run a streaming
/// knowledge-grounded turn, emit `stream_token` / `stream_chunk` as it goes, and
/// finish with `eventual_response` (200). Errors (no gateway key, unknown
/// session, agent failure) surface as clean `error` events.
/// Keys under `conversation.metadata_json` where the durable workflow step
/// pointer + per-step attempt count live. They belong on the CONVERSATION
/// (shared storage, addressed by the stable `conversation_id`) — NOT the
/// per-connection `session_id` or the per-pod in-memory session map, both of
/// which reset on a widget reconnect or a pod hop and froze the pointer at step 0
/// so the judge/cap could never advance it (th-c12df5 / th-d57a1d).
const WF_STEP_META_KEY: &str = "workflowCurrentStepId";
const WF_ATTEMPTS_META_KEY: &str = "workflowStepAttempts";

/// Conversation-metadata key holding the client render capabilities (`supports`)
/// last declared for this conversation. Durable, unlike the session registry, so
/// a reconnect that resumes the conversation without re-declaring still gets its
/// Rich Interactions (th-13df6d).
const CLIENT_SUPPORTS_META_KEY: &str = "clientSupports";

/// Read the durable capability list off a conversation's metadata. Missing /
/// unreadable → empty, i.e. exactly the text-only behavior (every interaction
/// kind degrades to its conversational fallback).
fn conversation_supports(metadata: Option<&Value>) -> Vec<String> {
    metadata
        .and_then(|m| m.get(CLIENT_SUPPORTS_META_KEY))
        .and_then(Value::as_array)
        .map(|caps| {
            caps.iter()
                .filter_map(|c| c.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Fold the declared capabilities into the metadata a fresh conversation is
/// created with, so the very first session already leaves the durable record a
/// later reconnect reads. An empty list writes nothing (keeps the metadata of a
/// text-only conversation byte-for-byte what it was).
fn with_client_supports(metadata: Option<Value>, supports: &[String]) -> Option<Value> {
    if supports.is_empty() {
        return metadata;
    }
    let mut obj = match metadata {
        Some(Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    };
    obj.insert(
        CLIENT_SUPPORTS_META_KEY.to_string(),
        Value::from(supports.to_vec()),
    );
    Some(Value::Object(obj))
}

/// Persist the declared capability list onto conversation metadata,
/// read-modify-write so sibling metadata keys (the workflow step pointer, the
/// caller's own `metadata`) survive. Best-effort, like
/// [`persist_workflow_step`]: a storage error is logged, not fatal — this
/// session already has its capabilities from the frame, and the worst case is
/// that a LATER reconnect which omits `supports` falls back to text-only.
async fn persist_client_supports(
    storage: &dyn StorageAdapter,
    conversation_id: &str,
    supports: &[String],
) {
    let existing = match storage.get_conversation(conversation_id).await {
        Ok(Some(c)) => c.metadata_json,
        _ => None,
    };
    let mut obj = match existing {
        Some(Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    };
    if supports.is_empty() {
        obj.remove(CLIENT_SUPPORTS_META_KEY);
    } else {
        obj.insert(
            CLIENT_SUPPORTS_META_KEY.to_string(),
            Value::from(supports.to_vec()),
        );
    }
    if let Err(e) = storage
        .update_conversation(
            conversation_id,
            ConversationUpdate {
                metadata_json: Some(Value::Object(obj)),
                ..Default::default()
            },
        )
        .await
    {
        tracing::warn!(
            error = %e,
            conversation_id,
            "failed to persist declared client capabilities; a later reconnect that omits 'supports' will degrade to text-only"
        );
    }
}

/// Read the durable `(current_step_id, attempts)` off the conversation's
/// metadata. Missing / unreadable → `(None, 0)`, so the runner resolves to the
/// workflow's first step exactly as a fresh conversation should.
async fn load_workflow_step(
    storage: &dyn StorageAdapter,
    conversation_id: &str,
) -> (Option<String>, u32) {
    let meta = match storage.get_conversation(conversation_id).await {
        Ok(Some(c)) => c.metadata_json,
        _ => None,
    };
    let Some(Value::Object(m)) = meta else {
        return (None, 0);
    };
    let step = m
        .get(WF_STEP_META_KEY)
        .and_then(Value::as_str)
        .map(str::to_string);
    let attempts = m
        .get(WF_ATTEMPTS_META_KEY)
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    (step, attempts)
}

/// Persist the step pointer + attempt count onto the conversation metadata,
/// read-modify-write so sibling metadata keys survive. Best-effort: a storage
/// error is logged, not fatal — the turn already succeeded, and the worst case is
/// the next turn re-resolves the prior step (the attempt cap still bounds it).
async fn persist_workflow_step(
    storage: &dyn StorageAdapter,
    conversation_id: &str,
    step_id: &str,
    attempts: u32,
) {
    let existing = match storage.get_conversation(conversation_id).await {
        Ok(Some(c)) => c.metadata_json,
        _ => None,
    };
    let mut obj = match existing {
        Some(Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    };
    obj.insert(WF_STEP_META_KEY.to_string(), Value::from(step_id));
    obj.insert(WF_ATTEMPTS_META_KEY.to_string(), Value::from(attempts));
    if let Err(e) = storage
        .update_conversation(
            conversation_id,
            ConversationUpdate {
                metadata_json: Some(Value::Object(obj)),
                ..Default::default()
            },
        )
        .await
    {
        tracing::warn!(
            error = %e,
            conversation_id,
            "failed to persist workflow step pointer; next turn may re-resolve the prior step"
        );
    }
}

/// Handle a `send_message` frame. Validates the frame, then **spawns** the agent
/// turn (so the reader stays free to receive `confirm_tool_action` while the turn
/// parks). Returns the spawned turn's [`JoinHandle`](tokio::task::JoinHandle) so
/// the connection loop can track it as the connection's single active turn and
/// abort it on a `cancel` frame (or disconnect). Returns `None` on any validation
/// failure (an `error` event was already emitted) — no turn was spawned.
async fn handle_send_message(
    state: &AppState,
    auth_org: Option<&str>,
    access: &AccessContext,
    scope: &UserScope,
    parsed: &Value,
    request_id: Option<&str>,
    sink: &UnboundedSender<Value>,
) -> Option<SpawnedTurn> {
    // requestId is load-bearing for streaming correlation; require it.
    let Some(request_id) = request_id else {
        let _ = sink.send(protocol::error(
            None,
            "VALIDATION_ERROR",
            "send_message requires a 'requestId'",
        ));
        return None;
    };

    let Some(session_id) = parsed.get("sessionId").and_then(Value::as_str) else {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "VALIDATION_ERROR",
            "missing 'sessionId'",
        ));
        return None;
    };

    let message = match parsed.get("message").and_then(Value::as_str) {
        Some(m) if !m.trim().is_empty() => m.to_string(),
        _ => {
            let _ = sink.send(protocol::error(
                Some(request_id),
                "VALIDATION_ERROR",
                "missing or empty 'message'",
            ));
            return None;
        }
    };

    // th-694c22: one line per turn — enough to correlate a visitor's report
    // ("I sent a message and nothing happened") to a session and requestId.
    tracing::info!(
        session_id,
        request_id,
        message_chars = message.len(),
        "send_message: turn requested"
    );

    // Optional multimodal attachments. Fail-soft: absent ⇒ text-only; a malformed
    // `images` array is dropped rather than rejecting the turn (per the schema).
    let images: Vec<smooth_operator::tool_provider::UserImage> = parsed
        .get("images")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    // Optional non-image file attachments (`files[]`: `{name, mimeType?, url}`).
    // Fail-soft like `images`: absent ⇒ none, a malformed array is dropped rather
    // than rejecting the turn. Files never reach the model — they ride the
    // tool-provider context so a host tool can persist them.
    let files: Vec<smooth_operator::tool_provider::UserFile> = parsed
        .get("files")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    // Optional named skill. The wire carries the INTENT ("use skill X"); the
    // server resolves the body here and composes it into the turn's system
    // prompt below, so `message` stays exactly what the user typed (and is what
    // gets persisted + replayed as history).
    //
    // Fail-CLOSED, unlike `images`: an unresolvable skill aborts the turn rather
    // than quietly answering without it. A caller that asked for a code review
    // recipe and got a freeform answer has no way to tell.
    let skill_section = match parsed
        .get("skill")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(name) => {
            match crate::skills::resolve_section(state.skill_resolver.as_ref(), name).await {
                Some(section) => Some(section),
                None => {
                    let _ = sink.send(protocol::error(
                        Some(request_id),
                        "SKILL_NOT_FOUND",
                        &format!("skill '{name}' is not available on this server"),
                    ));
                    return None;
                }
            }
        }
        None => None,
    };

    // Ownership is checked BEFORE any turn is spawned or any message persisted:
    // sending into another user's session would replay their history as context
    // and stream the reply back to the sender, so an unscoped write here is also
    // a read of their conversation.
    let session = match scoped_session(state, session_id, auth_org, scope).await {
        Ok(Some(session)) => session,
        Ok(None) => {
            let _ = sink.send(protocol::error(
                Some(request_id),
                "SESSION_NOT_FOUND",
                &format!("session '{session_id}' not found"),
            ));
            return None;
        }
        Err(e) => {
            let _ = sink.send(session_storage_error(Some(request_id), session_id, &e));
            return None;
        }
    };

    // SMOODEV-3057: this is the moment a deferred create earns its rows — the
    // first message. Before the turn spawns, because the runner persists the
    // inbound message against `conversation_id` and every child row FKs the
    // conversation. A no-op for a session that was never deferred.
    //
    // A failure is reported as retryable (`STORAGE_ERROR`), not as a missing
    // session: the parked writes are kept, so the visitor's resend finishes them.
    if let Err(e) = state.materialize_session(session_id).await {
        let _ = sink.send(session_storage_error(Some(request_id), session_id, &e));
        return None;
    }

    // A test-injected provider (the scenario-parity corpus's `MockLlmClient`)
    // overrides the live gateway client entirely — the turn never touches the
    // network — so it does NOT need a configured gateway key. Production leaves
    // `chat_provider` `None`, so this clone is `None` and the key gate below is
    // unchanged.
    let chat_provider = state.chat_provider.clone();

    // Resolve the gateway key for this turn's org. The conversation carries the
    // org; the resolver maps it to a per-org key (e.g. a LiteLLM virtual key per
    // tenant) so a multi-tenant flavor bills/scopes each org separately. The
    // default `EnvGatewayKeyResolver` returns the single env key for every org,
    // so the local/default flavor is unchanged. On `None` (no per-org key) we
    // fall back to the env key; only when neither supplies a key do we error.
    let org_id = match state
        .storage
        .get_conversation(&session.conversation_id)
        .await
    {
        Ok(Some(conversation)) => conversation.organization_id,
        // No conversation row (shouldn't happen for a live session) → resolve as
        // if anonymous; the env fallback still applies.
        Ok(None) | Err(_) => String::new(),
    };
    let resolved_key = smooth_operator::gateway_key::resolve_gateway_key(
        &state.gateway_key_resolver,
        &org_id,
        state.config.gateway_key.as_deref(),
    )
    .await;

    // No resolvable key → can't run a *live* LLM turn. Return a clean error (the
    // server stays usable for protocol-only checks). When a mock provider is
    // injected we fall back to a placeholder config — the mock replaces the
    // client built from it, so its url/key/model are never used.
    // Keep a copy of the resolved key to thread into the turn's
    // `ToolProviderContext` (a retrieval-style host tool calls the same gateway);
    // `None` on the mock/placeholder path so a host tool can fall back.
    let turn_gateway_key = resolved_key.clone();
    let llm = match resolved_key {
        Some(key) => state.config.llm_config_with_key(key),
        None if chat_provider.is_some() => state.config.placeholder_llm_config(),
        None => {
            let _ = sink.send(protocol::error(
                Some(request_id),
                "LLM_UNAVAILABLE",
                "No LLM gateway key is available for this turn (SMOOAI_GATEWAY_KEY is unset and no \
                 per-org key resolved); this server cannot serve LLM turns. Configure the gateway \
                 key to enable send_message.",
            ));
            return None;
        }
    };

    // NB: the model overrides (per-agent config default, then per-turn Smooth
    // Modes) are applied below, AFTER the per-agent `AgentBehaviorConfig` resolves
    // (SEAM 3) — see `apply_agent_model_override` + `apply_model_override`. Nothing
    // between here and there reads `llm.model`.

    // Ack: processing started.
    let _ = sink.send(protocol::immediate_response(
        Some(request_id),
        202,
        "Processing your request...",
        json!({}),
    ));

    // Run the turn in a spawned task, NOT inline. A turn that calls a
    // confirmation-gated write tool **parks** awaiting a later
    // `confirm_tool_action` frame; the socket reader dispatches that frame on the
    // same connection, so blocking the reader here would deadlock (the confirm
    // can never be read). Spawning frees the reader to receive the confirmation
    // while the turn streams its events through the (cloned) sink. Pearl: HITL
    // pause/resume.
    // th-be3f55: build the confirmation config when EITHER tool patterns are
    // configured OR a host hook supplied approver channels. Previously this was
    // patterns-only, so a host that classifies calls itself (Big Smooth's
    // auto-mode gate) could never get a bridge — which is why its `Ask` verdicts
    // failed closed and the daemon had to run in `Bypass`.
    let host_approver = state.host_approver.clone();
    let patterns = state.config.confirmation_tool_patterns();
    let confirmation = (patterns.is_some() || host_approver.is_some()).then(|| {
        crate::runner::ConfirmationConfig {
            tool_patterns: patterns.unwrap_or_default(),
            host_approver,
            session_id: session.session_id.clone(),
            register: {
                let state = state.clone();
                Arc::new(move |sid: &str, responder| state.register_confirmation(sid, responder))
            },
            clear: {
                let state = state.clone();
                Arc::new(move |sid: &str| state.clear_confirmation(sid))
            },
            // th-db0816: mirror every park into the session's durable
            // `metadata.pendingConfirmation` (and clear it when the park
            // resolves), so a confirm that lands on another pod — or after this
            // turn died — can still carry out the verdict. The closure is sync;
            // the write-through is spawned (best-effort on set, and the bridge
            // never blocks a live turn on storage).
            persist: {
                let state = state.clone();
                Some(Arc::new(move |sid: &str, record: Option<Value>| {
                    let state = state.clone();
                    let sid = sid.to_string();
                    tokio::spawn(async move {
                        let _ = state.set_pending_confirmation(&sid, record).await;
                    });
                })
                    as crate::runner::PersistPendingConfirmation)
            },
            // One-shot: `Some` only on the continuation turn spawned by a
            // `confirm_tool_action` that resolved a durable record.
            pre_approved: state.take_pre_approval(session_id),
        }
    });

    // Rich Interactions: always wired in the WS server (the primitive is on;
    // the per-agent `enabled_tools` allow-list can restrict individual raise
    // tools). Rich vs conversational-fallback is decided PER KIND from the
    // capabilities the client declared at create-session.
    let interactions = Some(crate::runner::InteractionConfig {
        session_id: session.session_id.clone(),
        kinds: Arc::clone(&state.interactions),
        capabilities: state.session_capabilities(session_id),
        register: {
            let state = state.clone();
            Arc::new(
                move |sid: &str, interaction_id: &str, kind: &str, spec: &Value, responder| {
                    state.register_interaction(
                        sid,
                        crate::state::PendingInteraction {
                            interaction_id: interaction_id.to_string(),
                            kind: kind.to_string(),
                            spec: spec.clone(),
                            responder,
                        },
                    );
                },
            )
        },
        clear: {
            let state = state.clone();
            Arc::new(move |sid: &str| state.clear_interaction(sid))
        },
        attach: {
            let state = state.clone();
            let sid = session.session_id.clone();
            Arc::new(move |kind, values| attach_interaction_effect(&state, &sid, kind, values))
        },
        // th-db0816: mirror every raise into the session's durable
        // `metadata.pendingInteraction`, the interaction sibling of the
        // confirmation record — a submit landing on another pod (or after this
        // turn died) can then still validate and resolve it.
        persist: {
            let state = state.clone();
            Some(Arc::new(move |sid: &str, record: Option<Value>| {
                let state = state.clone();
                let sid = sid.to_string();
                tokio::spawn(async move {
                    let _ = state.set_pending_interaction(&sid, record).await;
                });
            }) as crate::runner::PersistPendingConfirmation)
        },
    });

    // The turn's org, used to (a) resolve the org's persona override (SEAM 2)
    // and (b) scope the host's tool provider (SEAM 1).
    //
    // This REUSES the org already derived from the conversation above for the
    // gateway key, rather than re-binding the seed org. Re-binding it shadowed
    // the real value, so on a multi-tenant host every turn scoped its persona
    // and its host tools to `reference-org` no matter which tenant was talking —
    // while the gateway key, resolved from the same function a few lines up, was
    // correctly per-org. That split is what made it survive: billing looked
    // right, so nothing pointed at the scope being wrong. It also put
    // `reference-org` on the `gen_ai.chat` turn span, which is where it was
    // finally caught (SMOODEV-2952).
    //
    // Empty (no conversation row, or a flavor without storage) still falls back
    // to the seed org, so the single-org reference/local behavior is unchanged.
    let org_id = if org_id.is_empty() {
        crate::server::SEED_ORG_ID.to_string()
    } else {
        org_id
    };

    // SEAM 3 — per-agent behavior config (instructions + conversation workflow),
    // resolved by the connection's `agent_id` so two agents in the same org can
    // behave differently. Absent / malformed ⇒ `None`, so the org-default persona
    // (SEAM 2) is used, unchanged. Isolated per agent by construction.
    let agent_cfg: Option<AgentBehaviorConfig> = match session.agent_id.as_deref() {
        Some(id) => state.agent_config.resolve(id).await,
        None => None,
    };

    // SEAM 3 — model precedence, applied low → high so the winner clobbers last:
    //   1. server default (`SMOOTH_AGENT_MODEL`, already in `llm`),
    //   2. the per-AGENT `model` override (when configured),
    //   3. the per-TURN `send_message.model` (Smooth Modes) — always wins.
    let llm = apply_agent_model_override(llm, agent_cfg.as_ref());
    let llm = apply_model_override(llm, parsed);

    // SEAM 3 — per-agent agent-loop cap: the resolved `max_iterations`, else the
    // server default (`SMOOTH_AGENT_MAX_ITERATIONS`). Computed here (not in the
    // spawned turn) so the `Copy` value is simply moved into the task below.
    let max_iterations = agent_cfg
        .as_ref()
        .and_then(|c| c.max_iterations)
        .unwrap_or(state.config.max_iterations);

    // SEAM 2/3 — resolve the system prompt in priority order:
    //   1. the per-AGENT instructions (+ personality), when set,
    //   2. the per-ORG persona override ([`AgentSettings::persona`]),
    //   3. the host's installed default persona ([`AppState::default_persona`]).
    // All absent ⇒ `None`, so the runner stays on its const customer-support
    // prompt and behavior is byte-for-byte unchanged.
    let system_prompt = agent_cfg
        .as_ref()
        .and_then(AgentBehaviorConfig::system_prompt)
        .or_else(|| state.settings.get(&org_id).persona)
        .or_else(|| state.default_persona.clone());

    // The agent's first-turn greeting section (the runner injects it only when
    // the conversation has no prior messages) + its tool allow-list (`None` ⇒ the
    // full server tool set).
    let greeting_section = agent_cfg
        .as_ref()
        .and_then(AgentBehaviorConfig::greeting_section);
    let enabled_tools = agent_cfg
        .as_ref()
        .and_then(AgentBehaviorConfig::enabled_tool_ids);
    // Per-agent passthrough LLM-request metadata (spend attribution etc);
    // `None`/empty ⇒ no `metadata` on the wire. A host resolver sets it on the
    // agent's `AgentBehaviorConfig`; core normalizes empty to omitted.
    let request_metadata = agent_cfg.as_ref().and_then(|c| c.llm_metadata.clone());

    // SEP per-agent extension enablement (SMOODEV-2259). A resolved agent (Some
    // cfg) always yields `Some(vec)` — even an EMPTY vec — so the extension host
    // intersects the server allowlist with these ids and a resolved agent that
    // enables no extension loads ZERO (fail-closed). `None` only when no per-agent
    // config resolved at all (bare/standalone operator), preserving the
    // server-allowlist-only behavior. Extensions can intercept & mutate tool calls,
    // so a public agent must never silently inherit one.
    let enabled_extensions: Option<Vec<String>> = agent_cfg
        .as_ref()
        .map(AgentBehaviorConfig::enabled_extension_ids);

    // Per-tool config delivered to host tools at execution + the authLevel gate.
    let tool_configs = agent_cfg
        .as_ref()
        .map(AgentBehaviorConfig::tool_configs)
        .filter(|m| !m.is_empty());
    // The session's identity-verified bit (set by a prior successful verify_otp)
    // is threaded into the gate so a verified caller's `end_user` tools run.
    let session_authed = state.session_authenticated(session_id);
    let auth_gate = agent_cfg
        .as_ref()
        .and_then(|cfg| build_auth_gate(state, cfg, session_authed));
    // Keep a handle to the gate's OTP-refusal flag so, after the turn, we can see
    // whether an `end_user` tool was refused for lack of verification and (with an
    // OtpService installed + a known contact) offer the OTP flow. `None` when
    // there's no gate — the OTP flow can't trigger.
    let otp_gate = auth_gate.clone();

    // The agent's conversation workflow (if any) + the durable step this
    // CONVERSATION is on. The pointer + attempt count load from shared storage
    // (conversation metadata, keyed by the stable `conversation_id`) so they
    // survive widget reconnects and pod hops — the per-pod in-memory session map
    // reset them to step 0 every turn, freezing the workflow at the first step so
    // the judge/cap could never advance it (th-c12df5). Only read when the agent
    // actually has a workflow.
    let wf_cfg = agent_cfg
        .as_ref()
        .and_then(|c| c.conversation_workflow.clone());
    let (loaded_step_id, loaded_attempts) = if wf_cfg.is_some() {
        load_workflow_step(state.storage.as_ref(), &session.conversation_id).await
    } else {
        (None, 0)
    };
    let workflow = wf_cfg.map(|wf| runner::WorkflowTurn {
        workflow: wf,
        current_step_id: loaded_step_id,
    });

    // Captured for the post-turn per-step attempt cap (moved into the spawn): the
    // workflow (to compute a force-advance target), the step we started this turn
    // on, and the carried consecutive-hold count. See `apply_step_cap`.
    //
    // The count MUST come from `loaded_attempts` (durable conversation metadata),
    // not the per-pod session map — that is the whole point of th-c12df5, and a
    // duplicate of this block reading the session map shadowed this one until
    // th-fc07ac.
    let (cap_workflow, cap_step_before, cap_attempts) = match workflow.as_ref() {
        Some(wt) => (
            Some(wt.workflow.clone()),
            wt.current_step_id.clone(),
            loaded_attempts,
        ),
        None => (None, None, 0),
    };

    // The judge LLM surface — only built when there's a workflow to advance. A
    // test-injected chat provider (the mock) doubles as the judge offline; in
    // production the judge runs on the server's default (cheap) model with the
    // turn's resolved gateway key, independent of any per-turn model override so
    // the yes/no/maybe decision stays cheap.
    let judge: Option<Arc<dyn LlmProvider>> = if workflow.is_some() {
        Some(build_judge_provider(state, &llm))
    } else {
        None
    };

    // SEAM 1 — host tool provider (None by default ⇒ built-ins only).
    let tool_provider = state.tool_provider.clone();
    let session_id_owned = session_id.to_string();

    let state_for_turn = state.clone();
    // Carry the turn's org on the AccessContext so a multi-tenant host adapter's
    // `knowledge_for_access` can scope RAG to this tenant. The authed-principal
    // path already stamps its own org (`Principal::access_context`); a widget /
    // anonymous connection does not, so fall back to the session's persisted org
    // (every session carries `organization_id` since the create-session path
    // derives it). The operator's built-in single-tenant ACL ignores the org, so
    // this is behavior-preserving for the reference flavor.
    let access_owned = if access.organization_id.is_some() {
        access.clone()
    } else {
        access
            .clone()
            .with_organization_id(session.organization_id.clone())
    };
    let sink_owned = sink.clone();
    let request_id_owned = request_id.to_string();
    let conversation_id = session.conversation_id.clone();

    // See `SpawnedTurn`: raised by the cancel path before `cancelled` goes out, and
    // read by this turn's tail before every side effect. `abort()` is asynchronous,
    // so this flag — not the abort — is what makes `cancelled` terminal.
    let cancelled = Arc::new(AtomicBool::new(false));
    let turn_cancelled = cancelled.clone();

    let turn_handle = tokio::spawn(async move {
        // SEP — build this turn's extension host (only when SMOOTH_EXTENSIONS_ALLOW
        // is set; `None` otherwise, zero overhead). The delegate is bound to THIS
        // turn's sink/request/session so a hosted extension's `ui/confirm` routes
        // back over this connection.
        let extensions = crate::extensions::build_extension_host(
            &state_for_turn,
            &session_id_owned,
            &request_id_owned,
            sink_owned.clone(),
            enabled_extensions.as_deref(),
        )
        .await;
        // Clamp max_tokens to the resolved model's output ceiling (best-effort;
        // None ⇒ unclamped). Reuses the cached /model/info fetch. EPIC th-1cc9fa.
        let model_max_output =
            crate::admin::model_output_ceiling(&state_for_turn, &llm.model).await;
        let result = runner::run_streaming_turn(
            TurnRequest {
                storage: state_for_turn.storage.clone(),
                llm,
                max_iterations,
                conversation_id: &conversation_id,
                request_id: &request_id_owned,
                user_message: &message,
                // The resolved model's output ceiling (clamps max_tokens; None ⇒ unclamped).
                model_max_output,
                // The connection's resolved document-level entitlement: retrieval is
                // filtered to what this requester may read (org-public only when the
                // connection is anonymous).
                access: access_owned,
                // Production: `None` (a live client is built from `llm`). Tests: the
                // scenario corpus's `MockLlmClient`, which runs the turn offline.
                llm_provider: chat_provider,
                // SEAM — the executor this turn runs on. `None` (the default)
                // is in-process. A host installs one via `AppState::with_executor`:
                // a durable backend (ADR-030), or a decorator that guards the
                // returned conversation's reply against the tools that actually
                // ran (pearl th-39999c).
                executor: state_for_turn.executor.clone(),
                // Opt-in rerank stage (feature gap G8): `None` unless the operator
                // enabled it via `SMOOTH_AGENT_RERANK` (gateway/lexical). Default-off
                // keeps retrieval behavior unchanged.
                reranker: crate::reranker::build_reranker(
                    &crate::reranker::RerankerConfig::from_server_config(&state_for_turn.config),
                ),
                confirmation,
                // Rich Interactions (per-kind card park on capable clients;
                // validated conversational fallback otherwise).
                interactions,
                // SEAM 1 — host tool provider (None by default ⇒ built-ins only).
                tool_provider,
                // Host tool hooks applied to every turn's registry (empty by
                // default). Big Smooth injects its auto-mode gate + narc judge here.
                tool_hooks: state_for_turn.tool_hooks.clone(),
                // SEAM 2 — resolved per-org persona (None ⇒ const prompt).
                system_prompt,
                org_id: Some(org_id),
                // The per-org key resolved above, threaded so a host tool
                // provider's retrieval tools call the same gateway this turn used.
                gateway_key: turn_gateway_key,
                user_token: None,
                // th-8400b7: no act-as-user credential from the reference
                // server. A host that wants its tools to call its own API as
                // the user populates this; leaving it `None` keeps the existing
                // behaviour exactly.
                // SEAM 3 — per-agent conversation workflow + its cheap judge. Both
                // `None` for a freeform agent, so the turn is unchanged.
                workflow,
                judge,
                // SEAM 3 — per-agent first-turn greeting + tool allow-list.
                greeting_section,
                // The turn's resolved skill, rendered as a prompt section
                // (None ⇒ unchanged).
                skill_section,
                enabled_tools,
                // SEAM 3 — authLevel gate + per-tool config delivery.
                auth_gate,
                tool_configs,
                // SEP — the per-turn extension host (None unless allowlisted).
                extensions,
                // Optional multimodal attachments (empty ⇒ text-only, unchanged).
                images,
                // Optional non-image file attachments (empty ⇒ none). Never sent
                // to the model — carried onto the tool-provider context only.
                files,
                // Per-agent LLM-request metadata (spend attribution etc).
                request_metadata,
                // Seeded-demo flavor (`SMOOTH_AGENT_SEED_KB=1`): registers the mock
                // `issue_refund` write tool + the refund-flow persona so the HITL
                // demo gates approval on a write. False in every other flavor
                // (byte-for-byte unchanged).
                demo_tools: state_for_turn.config.seed_kb,
            },
            &sink_owned,
        )
        .await;

        // A cancel that raced this turn's completion has already put the terminal
        // `cancelled` on the wire. Everything below — the workflow pointer, the OTP
        // dispatch, `eventual_response`, the auto-title — is a side effect of a turn
        // the client was told did not happen, so produce none of it.
        if turn_cancelled.load(Ordering::SeqCst) {
            return;
        }

        match result {
            Ok(turn) => {
                // Persist the workflow step pointer the judge landed on, so the
                // next turn resumes on the right step, applying the per-step
                // attempt cap so a step the judge never advances can't loop forever
                // (th-d57a1d). Written to the CONVERSATION's shared metadata (keyed
                // by the stable `conversation_id`) so it survives reconnects/pod
                // hops (th-c12df5). No-op when the agent has no workflow
                // (`next_step_id` is `None`).
                if let Some(step) = turn.next_step_id.as_deref() {
                    let (persist_step, persist_attempts) = match cap_workflow.as_ref() {
                        Some(wf) => smooth_operator::agent_config::apply_step_cap(
                            wf,
                            cap_step_before.as_deref(),
                            step,
                            cap_attempts,
                            smooth_operator::agent_config::WORKFLOW_STEP_ATTEMPT_CAP,
                        ),
                        None => (step.to_string(), 0),
                    };
                    persist_workflow_step(
                        state_for_turn.storage.as_ref(),
                        &conversation_id,
                        &persist_step,
                        persist_attempts,
                    )
                    .await;
                }
                // If the auth gate refused an `end_user` tool for lack of a
                // verified session this turn, and a host OTP service is installed
                // and the session has a contact to reach, offer the OTP flow
                // (prompt → dispatch → ack). The reference server does not
                // park/auto-resume; the client verifies via `verify_otp` and
                // re-sends its message once the session is authenticated.
                if let (Some(gate), Some(otp)) =
                    (otp_gate.as_ref(), state_for_turn.otp_service.clone())
                {
                    if let Some(tool) = gate.otp_refused_tool() {
                        let contact = state_for_turn.session_contact(&session_id_owned);
                        // Re-read adjacent to the send: `persist_workflow_step` above
                        // awaits, so a cancel can land between the arm's check and here,
                        // and this branch puts a real code in front of a real person.
                        if !contact.is_empty() && !turn_cancelled.load(Ordering::SeqCst) {
                            offer_otp(
                                otp.as_ref(),
                                &session_id_owned,
                                &tool,
                                &contact,
                                &request_id_owned,
                                &sink_owned,
                            )
                            .await;
                        }
                    }
                }
                // Re-read adjacent to the emit: `offer_otp` above awaits, so a cancel
                // can land between the previous check and this send — which is exactly
                // how one requestId ended up with both a 499 and a 200.
                if turn_cancelled.load(Ordering::SeqCst) {
                    return;
                }
                let response =
                    runner::general_agent_response(&turn.reply, &turn.suggested_next_actions);
                let _ = sink_owned.send(protocol::eventual_response(
                    &request_id_owned,
                    200,
                    &turn.message_id,
                    response,
                    false,
                    &turn.citations,
                    turn.usage,
                    turn.directive,
                ));

                // Best-effort auto-title (fires only while the conversation is
                // still default-named ⇒ effectively the first turn). Detached so
                // the small-model call never delays this turn; a failure just
                // leaves the default `Session <uuid>` name.
                let title_state = state_for_turn.clone();
                let title_conv = conversation_id.clone();
                let title_user = message.clone();
                let title_reply = turn.reply.clone();
                tokio::spawn(async move {
                    maybe_auto_title(&title_state, &title_conv, &title_user, &title_reply).await;
                });
            }
            Err(e) => {
                if turn_cancelled.load(Ordering::SeqCst) {
                    return;
                }
                let _ = sink_owned.send(protocol::error(
                    Some(&request_id_owned),
                    "AGENT_ERROR",
                    &format!("agent turn failed: {e}"),
                ));
            }
        }
    });
    Some(SpawnedTurn {
        handle: turn_handle,
        cancelled,
    })
}

/// `confirm_tool_action` — resume a turn parked on a write-tool confirmation.
///
/// Per `spec/actions/confirm-tool-action.schema.json` the client sends
/// `{ action, sessionId, requestId, approved }` in reply to a
/// `write_confirmation_required` event. We look up the session's registered
/// [`HumanResponse`](smooth_operator_core::HumanResponse) sender (set by the
/// runner's confirmation bridge when the turn parked), take it, and feed it the
/// verdict: `approved` → `Approved` (the parked tool executes), else `Denied`
/// (the tool is skipped with a rejection result the model sees). There is no
/// dedicated response event — the resumed workflow signals continuation via its
/// normal streaming sequence (`stream_chunk`/`stream_token` → `eventual_response`);
/// we additionally ack with an `immediate_response`. Taking the sender makes a
/// duplicate confirm a no-op (`NO_PENDING_CONFIRMATION`).
async fn handle_confirm_tool_action(
    state: &AppState,
    auth_org: Option<&str>,
    access: &AccessContext,
    scope: &UserScope,
    parsed: &Value,
    request_id: Option<&str>,
    sink: &UnboundedSender<Value>,
) -> Option<SpawnedTurn> {
    let Some(session_id) = parsed.get("sessionId").and_then(Value::as_str) else {
        let _ = sink.send(protocol::error(
            request_id,
            "VALIDATION_ERROR",
            "confirm_tool_action requires a 'sessionId'",
        ));
        return None;
    };

    // `approved` is required and must be a boolean — a missing/garbled verdict
    // must NOT silently approve a write. Fail closed on a bad shape.
    let Some(approved) = parsed.get("approved").and_then(Value::as_bool) else {
        let _ = sink.send(protocol::error(
            request_id,
            "VALIDATION_ERROR",
            "confirm_tool_action requires a boolean 'approved'",
        ));
        return None;
    };

    // Approving a write parked in ANOTHER user's turn is the same class of hole
    // as writing into their session. A session we may not read is reported with
    // the identical event an id with no pending confirmation produces.
    let owned = match scoped_session(state, session_id, auth_org, scope).await {
        Ok(session) => session.is_some(),
        Err(e) => {
            // Not "no such pending confirmation" — we could not find out. The
            // parked turn is still waiting; a retry can still approve it.
            let _ = sink.send(session_storage_error(request_id, session_id, &e));
            return None;
        }
    };
    if !owned {
        let _ = sink.send(protocol::error(
            request_id,
            "NO_PENDING_CONFIRMATION",
            &format!("no tool action is awaiting confirmation for session '{session_id}'"),
        ));
        return None;
    }

    // FAST PATH: the parked turn lives on THIS pod — feed its sender the
    // verdict and it resumes in place (execute or reject the tool).
    if let Some(responder) = state.take_confirmation(session_id) {
        let verdict = if approved {
            smooth_operator_core::HumanResponse::Approved
        } else {
            smooth_operator_core::HumanResponse::Denied {
                reason: "user rejected the action".to_string(),
            }
        };

        if responder.send(verdict).is_ok() {
            tracing::info!(
                session_id,
                approved,
                "confirm_tool_action: live park resolved"
            );
            // The park is resolved: retire the durable record NOW rather than at
            // turn end, shrinking the window in which a duplicate confirm could
            // read it back as still pending (th-db0816). Best-effort — the
            // bridge clears it again at turn end.
            {
                let state = state.clone();
                let sid = session_id.to_string();
                tokio::spawn(async move {
                    let _ = state.set_pending_confirmation(&sid, None).await;
                });
            }
            // Ack; the resumed turn streams its own follow-on events.
            let _ = sink.send(protocol::immediate_response(
                request_id,
                200,
                if approved {
                    "Tool action approved"
                } else {
                    "Tool action rejected"
                },
                json!({ "sessionId": session_id, "approved": approved }),
            ));
            return None;
        }
        // The local park died (timeout / disconnect) before the confirm landed
        // — fall through to the durable record, which may still be resolvable.
    }

    // DURABLE PATH (th-db0816): no live park on this pod. The park may live on
    // ANOTHER pod (a refresh reconnected the visitor elsewhere), or its turn
    // may have died with a pod roll. Storage carries the record the bridge
    // persisted at park time — enough to carry out the verdict here with a
    // continuation turn instead of telling a human their approval went nowhere.
    durable_confirm_fallback(
        state, auth_org, access, scope, session_id, approved, request_id, sink,
    )
    .await
}

/// Resolve a `confirm_tool_action` against the DURABLE pending-confirmation
/// record when no live park exists on this pod (th-db0816).
///
/// The record (`metadata.pendingConfirmation`, written by the runner's
/// confirmation bridge) is read FRESH from storage — the local session cache
/// may predate the park. It is then cleared BEFORE acting, fail-closed: a
/// record we cannot retire is a record that could execute a write twice, so a
/// failed clear surfaces as a retryable storage error instead of proceeding.
///
/// Approved → grant a one-shot pre-approval for the recorded tool and spawn a
/// continuation turn through the normal `send_message` path; the model re-issues
/// the call (the approval message carries the recorded arguments), the bridge
/// auto-approves that one call, and the reply streams to THIS socket. Denied →
/// ack; the parked tool never runs anywhere (a dead park cannot execute, and a
/// still-parked twin on another pod resolves to a timeout rejection).
#[allow(clippy::too_many_arguments)]
async fn durable_confirm_fallback(
    state: &AppState,
    auth_org: Option<&str>,
    access: &AccessContext,
    scope: &UserScope,
    session_id: &str,
    approved: bool,
    request_id: Option<&str>,
    sink: &UnboundedSender<Value>,
) -> Option<SpawnedTurn> {
    let no_pending = || {
        protocol::error(
            request_id,
            "NO_PENDING_CONFIRMATION",
            &format!("no tool action is awaiting confirmation for session '{session_id}'"),
        )
    };

    // Fresh read — the local cache may hold a copy primed BEFORE the park was
    // persisted (e.g. this pod served an earlier frame for the session).
    let session = match state.storage.get_session(session_id).await {
        Ok(Some(session)) => session,
        Ok(None) => {
            let _ = sink.send(no_pending());
            return None;
        }
        Err(e) => {
            let _ = sink.send(session_storage_error(request_id, session_id, &e));
            return None;
        }
    };
    let Some(record) = session
        .metadata
        .as_ref()
        .and_then(|m| m.get("pendingConfirmation"))
        .cloned()
    else {
        let _ = sink.send(no_pending());
        return None;
    };

    // Same lifetime as the in-process park: a record older than the
    // confirmation timeout belongs to a park that would have expired anyway
    // (its pod died before the bridge could clear it). Refuse rather than
    // execute a stale write.
    let fresh = record
        .get("requestedAt")
        .and_then(Value::as_i64)
        .is_some_and(|at| {
            let age = chrono::Utc::now().timestamp().saturating_sub(at);
            age >= 0 && age <= crate::runner::CONFIRMATION_TIMEOUT.as_secs() as i64
        });

    // Prime the cache with the FRESH session so the metadata edit below starts
    // from current state, then retire the record — fail-closed on a failed
    // clear when we are about to execute (a lingering record could run the
    // write twice), best-effort when refusing anyway.
    state.insert_session(session);
    let cleared = state.set_pending_confirmation(session_id, None).await;
    if !fresh {
        let _ = sink.send(no_pending());
        return None;
    }
    if let Err(e) = cleared {
        let _ = sink.send(session_storage_error(request_id, session_id, &e));
        return None;
    }

    if !approved {
        let _ = sink.send(protocol::immediate_response(
            request_id,
            200,
            "Tool action rejected",
            json!({ "sessionId": session_id, "approved": false }),
        ));
        return None;
    }

    let tool = record
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let prompt = record
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let arguments = record.get("arguments").cloned().unwrap_or(Value::Null);
    if tool.is_empty() {
        let _ = sink.send(no_pending());
        return None;
    }

    tracing::info!(
        session_id,
        tool,
        "confirm_tool_action: durable record resolved approved — driving continuation turn"
    );
    // The continuation turn: a normal `send_message` whose user message IS the
    // approval. The model sees the full conversation (its own "shall I?"
    // included) plus the recorded arguments, re-issues the call, and the
    // one-shot grant below lets that single call execute without parking again.
    state.grant_pre_approval(session_id, tool);
    let message = format!(
        "Approved — please proceed with the pending \"{tool}\" action now ({prompt}). \
         Use exactly these arguments: {arguments}"
    );
    let synthetic = json!({
        "action": "send_message",
        "sessionId": session_id,
        "message": message,
    });
    handle_send_message(state, auth_org, access, scope, &synthetic, request_id, sink).await
}

/// Apply an optional per-turn `model` override (from a `send_message` body) to a
/// resolved [`LlmConfig`]. When the body carries a non-empty `model` string, this
/// turn runs on that gateway model id (a Smooth Modes / `/smooth-mode` preset),
/// overriding the server's configured default; an absent, non-string, or
/// blank/whitespace-only `model` leaves the config's default model unchanged
/// (byte-for-byte the prior behavior). Every other field (url, key, limits)
/// stays as resolved — only the model id changes.
fn apply_model_override(mut llm: LlmConfig, body: &Value) -> LlmConfig {
    if let Some(model) = body.get("model").and_then(Value::as_str) {
        let model = model.trim();
        if !model.is_empty() {
            llm.model = model.to_string();
        }
    }
    llm
}

/// Apply a per-agent `model` override (from the resolved [`AgentBehaviorConfig`])
/// to a config. `Some(model)` sets this agent's default gateway model, overriding
/// the server default; `None` (no per-agent config, or no `model` set) leaves the
/// config unchanged. `from_row_values` already rejects blank models, but a defensive
/// trim keeps this a no-op on whitespace. An explicit per-turn `send_message.model`
/// is layered on top by [`apply_model_override`] and wins.
fn apply_agent_model_override(mut llm: LlmConfig, cfg: Option<&AgentBehaviorConfig>) -> LlmConfig {
    if let Some(model) = cfg.and_then(|c| c.model.as_deref()) {
        let model = model.trim();
        if !model.is_empty() {
            llm.model = model.to_string();
        }
    }
    llm
}

/// Cap the judge's output: a `yes` / `no` / `maybe` verdict needs only a few
/// tokens. Small so the extra per-turn cost + latency stay negligible.
const JUDGE_MAX_TOKENS: u32 = 16;

/// Build the per-agent authLevel gate, or `None` when it would be inert.
///
/// The set of tools that "support auth requirements" (the operator analog of the
/// TS `supportsAuthRequirement` flag) comes from `SMOOTH_AGENT_AUTH_TOOLS`
/// (comma-separated); empty (the default) ⇒ nothing is gated.
/// `session_authenticated` is the session's OTP-verified bit (from a prior
/// successful `verify_otp`): `false` fail-closed-refuses `end_user` tools (and,
/// with an OtpService installed, triggers the OTP-offer flow); `true` lets a
/// verified caller's `end_user` tools run.
fn build_auth_gate(
    state: &AppState,
    cfg: &AgentBehaviorConfig,
    session_authenticated: bool,
) -> Option<AuthGateHook> {
    let supporting: std::collections::HashSet<String> = std::env::var("SMOOTH_AGENT_AUTH_TOOLS")
        .ok()
        .into_iter()
        .flat_map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect();
    if supporting.is_empty() {
        let _ = state; // no host-declared auth-supporting tools → gate is inert
        return None;
    }
    let levels = cfg
        .enabled_tools
        .iter()
        .map(|t| (t.tool_id.clone(), AuthLevel::parse(&t.auth_level)))
        .collect();
    let hook = AuthGateHook::new(levels, cfg.visibility, session_authenticated, supporting);
    hook.is_active().then_some(hook)
}

/// Emit the OTP-offer sequence for a turn whose `end_user` tool was refused for
/// lack of a verified session: `otp_verification_required` (prompt the client),
/// then `send_otp` on the host service, then `otp_sent` (ack delivery) — or an
/// `error` event if delivery fails. The masked destination + channel come from
/// the host; the server never sees the code. `auth_level` is fixed `end_user`
/// (the only level this flow remedies).
async fn offer_otp(
    otp: &dyn smooth_operator::otp::OtpService,
    session_id: &str,
    tool: &str,
    contact: &smooth_operator::otp::OtpContact,
    request_id: &str,
    sink: &UnboundedSender<Value>,
) {
    let channels: Vec<&str> = contact
        .available_channels()
        .iter()
        .map(|c| c.as_str())
        .collect();
    let _ = sink.send(protocol::otp_verification_required(
        request_id,
        tool,
        &format!("Verify your identity to continue using '{tool}'."),
        &channels,
        "end_user",
    ));
    match otp.send_otp(session_id, contact).await {
        Ok(delivery) => {
            let _ = sink.send(protocol::otp_sent(
                request_id,
                delivery.channel.as_str(),
                &delivery.masked_destination,
            ));
        }
        Err(e) => {
            let _ = sink.send(protocol::error(
                Some(request_id),
                "OTP_SEND_FAILED",
                &format!("failed to send verification code: {e}"),
            ));
        }
    }
}

/// The kind-routed host effect of an accepted interaction. For
/// `identity_intake`, stamp the validated values onto the session (metadata
/// `userName` / `contactEmail` / `contactPhone` — the same keys the pre-chat /
/// create path stashes and the OTP contact seam reads). Future kinds add their
/// effect here (or a host overrides the attach seam entirely).
fn attach_interaction_effect(state: &AppState, session_id: &str, kind: &str, values: &Value) {
    if kind == "identity_intake" {
        if let Ok(values) = serde_json::from_value::<IntakeValues>(values.clone()) {
            state.attach_session_identity(session_id, &values);
        }
    }
}

/// A pending interaction's validation contract, from either source: the live
/// park on this pod, or the durable `metadata.pendingInteraction` record
/// (th-db0816).
struct PendingView {
    interaction_id: String,
    kind: String,
    spec: Value,
    /// Whether a live park (with a resumable responder) backs this view.
    live: bool,
}

/// The durable pending-interaction record for a session, read FRESH from
/// storage (the local cache can predate the park on another pod). Primes the
/// cache with the fresh session so a later retire edits current state.
async fn durable_pending_interaction(
    state: &AppState,
    session_id: &str,
) -> anyhow::Result<Option<PendingView>> {
    let Some(session) = state.storage.get_session(session_id).await? else {
        return Ok(None);
    };
    let record = session
        .metadata
        .as_ref()
        .and_then(|m| m.get("pendingInteraction"))
        .cloned();
    let Some(record) = record else {
        return Ok(None);
    };
    state.insert_session(session);
    let (Some(interaction_id), Some(kind), Some(spec)) = (
        record.get("interactionId").and_then(Value::as_str),
        record.get("kind").and_then(Value::as_str),
        record.get("spec"),
    ) else {
        return Ok(None);
    };
    Ok(Some(PendingView {
        interaction_id: interaction_id.to_string(),
        kind: kind.to_string(),
        spec: spec.clone(),
        live: false,
    }))
}

/// `submit_interaction` — resume a turn parked on a Rich Interaction.
///
/// Per `spec/actions/submit-interaction.schema.json` the client sends
/// `{ action, sessionId, requestId, interactionId, kind?, values?, declined? }`
/// in reply to an `interaction_required` event. Validation is **server-side**,
/// routed to the parked kind's validator against the spec the raise carried:
///   - invalid → an `interaction_invalid` event with per-field errors; the
///     turn STAYS parked so the card can resubmit (mirrors `otp_invalid`);
///   - valid → the kind's host effect runs (identity_intake: session identity
///     attach), the parked raise resumes with the canonical values, and an
///     `immediate_response` acks;
///   - `declined: true` → the raise resumes with a declined payload.
///
/// The `interactionId` must echo the event's, so a stale submit can never
/// resolve a newer park; taking the responder only on resolution makes a
/// duplicate submit a no-op (`NO_PENDING_INTERACTION`).
async fn handle_submit_interaction(
    state: &AppState,
    auth_org: Option<&str>,
    scope: &UserScope,
    parsed: &Value,
    request_id: Option<&str>,
    sink: &UnboundedSender<Value>,
) {
    // requestId is load-bearing (it echoes the originating
    // interaction_required); require it.
    let Some(request_id) = request_id else {
        let _ = sink.send(protocol::error(
            None,
            "VALIDATION_ERROR",
            "submit_interaction requires a 'requestId'",
        ));
        return;
    };

    let Some(session_id) = parsed.get("sessionId").and_then(Value::as_str) else {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "VALIDATION_ERROR",
            "submit_interaction requires a 'sessionId'",
        ));
        return;
    };

    // Peek the pending interaction WITHOUT consuming the park — an invalid
    // submit must leave the turn parked for a resubmit. A session we may not
    // read reports the identical event an id with no pending park produces (the
    // submitted values would otherwise land in another user's turn, and its
    // identity-attach effect on their session).
    let owned = match scoped_session(state, session_id, auth_org, scope).await {
        Ok(session) => session.is_some(),
        Err(e) => {
            // The park is untouched (this path only peeks), so a retry after the
            // blip still resolves the same interaction.
            let _ = sink.send(session_storage_error(Some(request_id), session_id, &e));
            return;
        }
    };
    // The pending interaction, from the live park on THIS pod or — when no
    // live park exists (the raise happened on another pod, or its turn died) —
    // from the durable record the bridge persisted (th-db0816). Either source
    // carries the full validation contract: id, kind, spec.
    let live = owned
        .then(|| state.pending_interaction(session_id))
        .flatten();
    let pending = match &live {
        Some(p) => PendingView {
            interaction_id: p.interaction_id.clone(),
            kind: p.kind.clone(),
            spec: p.spec.clone(),
            live: true,
        },
        None => {
            let durable = if owned {
                match durable_pending_interaction(state, session_id).await {
                    Ok(v) => v,
                    Err(e) => {
                        // Could not find out — retryable, the record is untouched.
                        let _ = sink.send(session_storage_error(Some(request_id), session_id, &e));
                        return;
                    }
                }
            } else {
                None
            };
            let Some(view) = durable else {
                let _ = sink.send(protocol::error(
                    Some(request_id),
                    "NO_PENDING_INTERACTION",
                    &format!("no interaction is awaiting submission for session '{session_id}'"),
                ));
                return;
            };
            view
        }
    };

    // The submit must target THIS interaction instance (and, when it names a
    // kind, the right kind) — a stale card can never resolve a newer park.
    let interaction_id = parsed.get("interactionId").and_then(Value::as_str);
    if interaction_id != Some(pending.interaction_id.as_str()) {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "INTERACTION_MISMATCH",
            "the submitted 'interactionId' does not match the pending interaction",
        ));
        return;
    }
    if let Some(kind) = parsed.get("kind").and_then(Value::as_str) {
        if kind != pending.kind {
            let _ = sink.send(protocol::error(
                Some(request_id),
                "INTERACTION_MISMATCH",
                &format!(
                    "the pending interaction is '{}', not '{kind}'",
                    pending.kind
                ),
            ));
            return;
        }
    }

    // Decline path: resume the raise with a declined payload.
    if parsed
        .get("declined")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let resolved = if pending.live {
            resolve_interaction(
                state,
                session_id,
                request_id,
                InteractionOutcome::Declined,
                sink,
            )
        } else {
            // No live park to resume — retiring the durable record IS the
            // resolution (a dead raise cannot be fed a decline).
            match state.set_pending_interaction(session_id, None).await {
                Ok(()) => true,
                Err(e) => {
                    let _ = sink.send(session_storage_error(Some(request_id), session_id, &e));
                    false
                }
            }
        };
        if resolved {
            let _ = sink.send(protocol::immediate_response(
                Some(request_id),
                200,
                "Interaction declined",
                json!({ "sessionId": session_id, "interactionId": pending.interaction_id, "declined": true }),
            ));
        }
        return;
    }

    // Values path: route to the parked kind's server-side validator.
    let Some(values) = parsed.get("values") else {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "VALIDATION_ERROR",
            "submit_interaction requires 'values' (or 'declined': true)",
        ));
        return;
    };
    let Some(kind) = state.interactions.get(&pending.kind) else {
        // A parked kind the registry no longer hosts (shouldn't happen).
        let _ = sink.send(protocol::error(
            Some(request_id),
            "NO_PENDING_INTERACTION",
            &format!(
                "interaction kind '{}' is not hosted by this server",
                pending.kind
            ),
        ));
        return;
    };

    match kind.validate(&pending.spec, values) {
        Err(errors) => {
            // Retryable: the turn stays parked; the client re-renders the card
            // with the per-field errors (never a terminal `error` event).
            let _ = sink.send(protocol::interaction_invalid(
                request_id,
                &pending.interaction_id,
                &pending.kind,
                &errors,
                "Some fields need attention.",
            ));
        }
        Ok(canonical) => {
            // Resume the parked raise FIRST, and run the kind's host effect only
            // if that succeeded. The effect writes to the SESSION
            // (identity_intake stamps `userName` / `contactEmail` /
            // `contactPhone`), so running it before the responder is proven live
            // let a submit against a park whose turn was cancelled or
            // disconnected stamp contact metadata anyway — repeatably, since a
            // failed resolve leaves nothing behind to notice (th-6fbab2).
            //
            // Durable path (th-db0816): with no live park, retiring the record
            // is the "proven" step — fail-closed, so a record that cannot be
            // cleared never runs the effect (the same retire-before-act rule as
            // durable confirmations). The visitor's card resolves and their
            // identity is attached durably; the dead raise's model
            // acknowledgment is forgone rather than fabricated.
            let resolved = if pending.live {
                resolve_interaction(
                    state,
                    session_id,
                    request_id,
                    InteractionOutcome::Submitted {
                        values: canonical.clone(),
                    },
                    sink,
                )
            } else {
                match state.set_pending_interaction(session_id, None).await {
                    Ok(()) => true,
                    Err(e) => {
                        let _ = sink.send(session_storage_error(Some(request_id), session_id, &e));
                        false
                    }
                }
            };
            if resolved {
                tracing::info!(
                    session_id,
                    kind = %pending.kind,
                    live = pending.live,
                    "submit_interaction: resolved with values"
                );
                attach_interaction_effect(state, session_id, &pending.kind, &canonical);
                let _ = sink.send(protocol::immediate_response(
                    Some(request_id),
                    200,
                    "Interaction submitted",
                    json!({
                        "sessionId": session_id,
                        "interactionId": pending.interaction_id,
                        "kind": pending.kind,
                        "values": canonical,
                    }),
                ));
            }
        }
    }
}

/// Take the pending interaction responder for `session_id` and feed it
/// `outcome`. Returns `true` when the parked turn was resumed; emits
/// `NO_PENDING_INTERACTION` and returns `false` when the park raced away
/// (duplicate submit, or the parked turn ended before the submit landed).
fn resolve_interaction(
    state: &AppState,
    session_id: &str,
    request_id: &str,
    outcome: InteractionOutcome,
    sink: &UnboundedSender<Value>,
) -> bool {
    let Some(pending) = state.take_interaction(session_id) else {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "NO_PENDING_INTERACTION",
            &format!("no interaction is awaiting submission for session '{session_id}'"),
        ));
        return false;
    };
    // Tag the outcome with the interaction it answers: the turn's raises share
    // one outcome channel, so an untagged resolution can be consumed by whatever
    // park happens to be waiting (th-d121f5).
    let resolution = InteractionResolution {
        interaction_id: pending.interaction_id.clone(),
        outcome,
    };
    if pending.responder.send(resolution).is_err() {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "NO_PENDING_INTERACTION",
            &format!(
                "the turn awaiting an interaction for session '{session_id}' is no longer active"
            ),
        ));
        return false;
    }
    true
}

/// `verify_otp` — validate a submitted OTP code and, on success, mark the/// `verify_otp` — validate a submitted OTP code and, on success, mark the
/// session identity-verified. Per `spec/actions/verify-otp.schema.json` the
/// client sends `{ action, sessionId, requestId, code }` in reply to an
/// `otp_verification_required` event. There is no dedicated response event: a
/// correct code emits `otp_verified` (the client then re-sends its message to
/// run the gated tool — the reference server does not park/auto-resume the
/// original turn), a rejected code emits `otp_invalid` carrying the host's
/// remaining-attempt count. With no [`OtpService`](smooth_operator::otp::OtpService)
/// installed, verification is impossible, so we fail closed with an `otp_invalid`
/// (`NOT_FOUND`, 0 attempts).
async fn handle_verify_otp(
    state: &AppState,
    auth_org: Option<&str>,
    scope: &UserScope,
    parsed: &Value,
    request_id: Option<&str>,
    sink: &UnboundedSender<Value>,
) {
    // requestId is load-bearing (it echoes the originating
    // otp_verification_required); require it.
    let Some(request_id) = request_id else {
        let _ = sink.send(protocol::error(
            None,
            "VALIDATION_ERROR",
            "verify_otp requires a 'requestId'",
        ));
        return;
    };

    let Some(session_id) = parsed.get("sessionId").and_then(Value::as_str) else {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "VALIDATION_ERROR",
            "verify_otp requires a 'sessionId'",
        ));
        return;
    };

    let Some(code) = parsed.get("code").and_then(Value::as_str) else {
        let _ = sink.send(protocol::error(
            Some(request_id),
            "VALIDATION_ERROR",
            "verify_otp requires a 'code'",
        ));
        return;
    };

    // The session must exist AND be ours (a code can't verify — or brute-force —
    // a session we don't track, nor one belonging to another user).
    match scoped_session(state, session_id, auth_org, scope).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            let _ = sink.send(protocol::error(
                Some(request_id),
                "SESSION_NOT_FOUND",
                &format!("session '{session_id}' not found"),
            ));
            return;
        }
        Err(e) => {
            // Fail closed on the gate — no code is checked — but say why, so the
            // caller retries instead of abandoning a verification in progress.
            let _ = sink.send(session_storage_error(Some(request_id), session_id, &e));
            return;
        }
    }

    // No host OTP service → verification is impossible. Fail closed on the
    // documented otp_invalid path (a client shouldn't reach here without first
    // receiving otp_verification_required, which only an installed service emits).
    let Some(otp) = state.otp_service.clone() else {
        let _ = sink.send(protocol::otp_invalid(
            request_id,
            Some("NOT_FOUND"),
            0,
            "No verification is in progress for this session.",
        ));
        return;
    };

    match otp.verify_otp(session_id, code).await {
        smooth_operator::otp::OtpVerifyOutcome::Verified => {
            state.set_session_authenticated(session_id, true).await;
            tracing::info!(session_id, "verify_otp: session identity verified");
            let _ = sink.send(protocol::otp_verified(
                request_id,
                "Identity verified successfully.",
            ));
        }
        smooth_operator::otp::OtpVerifyOutcome::Invalid {
            attempts_remaining,
            error,
            message,
        } => {
            let _ = sink.send(protocol::otp_invalid(
                request_id,
                error.map(smooth_operator::otp::OtpError::as_str),
                attempts_remaining,
                &message,
            ));
        }
    }
}

/// Build the workflow judge's LLM surface. Prefers a test-injected chat provider
/// (the scenario mock — runs offline); otherwise builds a live client on the
/// server's **default** (cheap) model with the turn's resolved gateway url/key,
/// independent of any per-turn model override, so the judge stays cheap even when
/// the turn itself runs on a bigger model.
fn build_judge_provider(state: &AppState, turn_llm: &LlmConfig) -> Arc<dyn LlmProvider> {
    if let Some(mock) = state.chat_provider.clone() {
        return mock;
    }
    let mut cfg = turn_llm.clone();
    cfg.model = state.config.judge_model.clone();
    cfg.max_tokens = JUDGE_MAX_TOKENS;
    Arc::new(LlmClient::new(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smooth_operator_core::llm::{ApiFormat, RetryPolicy};

    /// The `send_message.files[]` parse (mirrors the inline handler expression):
    /// a well-formed array parses onto the turn context, a malformed array is
    /// dropped rather than rejecting the turn, and an absent key ⇒ empty.
    #[test]
    fn files_array_parses_fail_soft() {
        use smooth_operator::tool_provider::UserFile;
        let parse = |v: Value| -> Vec<UserFile> {
            v.get("files")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default()
        };

        // Well-formed: `mimeType` maps onto `mime`; missing MIME ⇒ None.
        let files = parse(json!({
            "files": [
                { "name": "report.pdf", "mimeType": "application/pdf", "url": "https://x/report.pdf" },
                { "name": "notes.txt", "url": "data:text/plain;base64,AAAA" }
            ]
        }));
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "report.pdf");
        assert_eq!(files[0].mime.as_deref(), Some("application/pdf"));
        assert_eq!(files[1].mime, None);

        // Malformed entry (missing required `url`) drops the whole array, like `images`.
        assert!(parse(json!({ "files": [{ "name": "x" }] })).is_empty());
        // Absent key ⇒ empty.
        assert!(parse(json!({})).is_empty());
    }

    /// The durable workflow pointer round-trips through conversation metadata and
    /// a FRESH load resumes on it — the whole point of moving it off the per-pod
    /// in-memory session map, which reset to step 0 on every reconnect / pod hop
    /// and froze the workflow at the first step (th-c12df5). Sibling metadata keys
    /// survive the read-modify-write; an unknown conversation defaults, never panics.
    #[tokio::test]
    async fn workflow_step_persists_to_conversation_metadata_and_resumes() {
        use smooth_operator::domain::{Conversation, Platform};
        use smooth_operator_adapter_memory::InMemoryStorageAdapter;

        let storage = InMemoryStorageAdapter::new();
        let conv_id = "conv-step-1";
        let ts = chrono::Utc::now();
        storage
            .create_conversation(Conversation {
                id: conv_id.into(),
                platform: Platform::Web,
                name: "wf".into(),
                organization_id: "org-1".into(),
                idempotency_key: conv_id.into(),
                // A sibling metadata key that must survive the step writes.
                metadata_json: Some(json!({ "keep": "me" })),
                analytics_json: None,
                created_at: ts,
                updated_at: ts,
            })
            .await
            .expect("seed conversation");

        // Fresh conversation → no pointer yet.
        assert_eq!(load_workflow_step(&storage, conv_id).await, (None, 0));

        // Persist an advanced step; a NEW stateless load (as a reconnect / a
        // different pod would do) resumes on it.
        persist_workflow_step(&storage, conv_id, "collect", 2).await;
        assert_eq!(
            load_workflow_step(&storage, conv_id).await,
            (Some("collect".to_string()), 2)
        );

        // Overwrite advances the pointer + resets attempts.
        persist_workflow_step(&storage, conv_id, "summary", 0).await;
        assert_eq!(
            load_workflow_step(&storage, conv_id).await,
            (Some("summary".to_string()), 0)
        );

        // Sibling metadata survived the read-modify-write.
        let conv = storage.get_conversation(conv_id).await.unwrap().unwrap();
        assert_eq!(
            conv.metadata_json
                .unwrap()
                .get("keep")
                .and_then(Value::as_str),
            Some("me")
        );

        // Unknown conversation → defaults, never panics.
        assert_eq!(load_workflow_step(&storage, "nope").await, (None, 0));
    }

    /// The attempt cap counts across RECONNECTS: the count the cap sees must come
    /// from the durable conversation metadata, so a client cannot reset its own cap
    /// by reconnecting. Drives the exact pipeline the handler runs each turn —
    /// `load_workflow_step` → `apply_step_cap` → `persist_workflow_step` — with a
    /// judge that always holds, reloading from storage every turn as a fresh pod
    /// would. A per-pod (or otherwise always-zero) source makes `next_attempts`
    /// never reach the cap, so the step never force-advances and this fails.
    /// Regression test for th-fc07ac.
    #[tokio::test]
    async fn step_attempt_cap_counts_across_reconnects_and_force_advances() {
        use smooth_operator::agent_config::{
            apply_step_cap, ConversationWorkflow, ConversationWorkflowStep,
            WORKFLOW_STEP_ATTEMPT_CAP,
        };
        use smooth_operator::domain::{Conversation, Platform};
        use smooth_operator_adapter_memory::InMemoryStorageAdapter;

        let step = |id: &str| ConversationWorkflowStep {
            id: id.into(),
            intent: "i".into(),
            criteria: "c".into(),
            next: None,
            suggested_replies: None,
        };
        let workflow = ConversationWorkflow {
            goal: "g".into(),
            steps: vec![step("greet"), step("collect")],
        };

        let storage = InMemoryStorageAdapter::new();
        let conv_id = "conv-cap-1";
        let ts = chrono::Utc::now();
        storage
            .create_conversation(Conversation {
                id: conv_id.into(),
                platform: Platform::Web,
                name: "wf".into(),
                organization_id: "org-1".into(),
                idempotency_key: conv_id.into(),
                metadata_json: None,
                analytics_json: None,
                created_at: ts,
                updated_at: ts,
            })
            .await
            .expect("seed conversation");

        // Each iteration is a separate turn on a freshly-loaded (reconnected) state,
        // with a judge that never advances — it always re-picks the current step.
        let mut forced_on_turn = None;
        for turn in 1..=WORKFLOW_STEP_ATTEMPT_CAP {
            let (step_before, attempts) = load_workflow_step(&storage, conv_id).await;
            let judged_next = step_before.clone().unwrap_or_else(|| "greet".to_string());
            let (persist_step, persist_attempts) = apply_step_cap(
                &workflow,
                step_before.as_deref(),
                &judged_next,
                attempts,
                WORKFLOW_STEP_ATTEMPT_CAP,
            );
            persist_workflow_step(&storage, conv_id, &persist_step, persist_attempts).await;
            if persist_step == "collect" && forced_on_turn.is_none() {
                forced_on_turn = Some(turn);
            }
        }

        // The held step force-advanced exactly at the cap, and the counter reset.
        assert_eq!(
            forced_on_turn,
            Some(WORKFLOW_STEP_ATTEMPT_CAP),
            "a step the judge never advances must force-advance on the capth turn"
        );
        assert_eq!(
            load_workflow_step(&storage, conv_id).await,
            (Some("collect".to_string()), 0)
        );
    }

    /// A baseline config whose `model` is the server default, so each override
    /// test asserts against a known starting model.
    fn base_llm() -> LlmConfig {
        LlmConfig {
            api_url: "https://llm.smoo.ai/v1".to_string(),
            api_key: "sk-test".to_string(),
            model: "claude-haiku-4-5".to_string(),
            max_tokens: 512,
            temperature: crate::config::DEFAULT_TEMPERATURE,
            retry_policy: RetryPolicy::default(),
            api_format: ApiFormat::OpenAiCompat,
        }
    }

    #[test]
    fn model_override_present_replaces_model() {
        let body = json!({ "action": "send_message", "model": "claude-opus-4-8" });
        let llm = apply_model_override(base_llm(), &body);
        assert_eq!(llm.model, "claude-opus-4-8");
        // Only the model id changes — every other field is preserved.
        assert_eq!(llm.api_url, "https://llm.smoo.ai/v1");
        assert_eq!(llm.api_key, "sk-test");
        assert_eq!(llm.max_tokens, 512);
    }

    #[test]
    fn model_override_absent_keeps_default() {
        let body = json!({ "action": "send_message", "message": "hi" });
        let llm = apply_model_override(base_llm(), &body);
        assert_eq!(llm.model, "claude-haiku-4-5");
    }

    #[test]
    fn model_override_blank_or_non_string_keeps_default() {
        // Whitespace-only is treated as absent.
        let blank = json!({ "model": "   " });
        assert_eq!(
            apply_model_override(base_llm(), &blank).model,
            "claude-haiku-4-5"
        );
        // A non-string `model` is ignored (no panic, default kept).
        let wrong_type = json!({ "model": 42 });
        assert_eq!(
            apply_model_override(base_llm(), &wrong_type).model,
            "claude-haiku-4-5"
        );
    }

    fn cfg_with_model(model: Option<&str>) -> AgentBehaviorConfig {
        AgentBehaviorConfig {
            model: model.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn agent_model_override_present_replaces_default() {
        let cfg = cfg_with_model(Some("claude-sonnet-5"));
        assert_eq!(
            apply_agent_model_override(base_llm(), Some(&cfg)).model,
            "claude-sonnet-5"
        );
    }

    #[test]
    fn agent_model_override_absent_keeps_default() {
        // No per-agent config at all.
        assert_eq!(
            apply_agent_model_override(base_llm(), None).model,
            "claude-haiku-4-5"
        );
        // Config present but no model set.
        let cfg = cfg_with_model(None);
        assert_eq!(
            apply_agent_model_override(base_llm(), Some(&cfg)).model,
            "claude-haiku-4-5"
        );
    }

    #[test]
    fn per_turn_model_wins_over_per_agent() {
        // Precedence as wired in `process_send_message`: agent override first,
        // then the per-turn body override on top — the turn model must win.
        let cfg = cfg_with_model(Some("claude-sonnet-5"));
        let body = json!({ "model": "claude-opus-4-8" });
        let llm = apply_model_override(apply_agent_model_override(base_llm(), Some(&cfg)), &body);
        assert_eq!(llm.model, "claude-opus-4-8");
    }

    #[test]
    fn sanitize_title_strips_quotes_markdown_and_collapses_whitespace() {
        // Wrapping double quotes + trailing newline.
        assert_eq!(
            sanitize_title("\"Reset password help\"\n"),
            "Reset password help"
        );
        // Markdown bold wrapping.
        assert_eq!(sanitize_title("**Billing question**"), "Billing question");
        // Code-fence backticks + collapse internal newlines/spaces.
        assert_eq!(
            sanitize_title("`Order   status\ncheck`"),
            "Order status check"
        );
        // Leading markdown heading marker.
        assert_eq!(sanitize_title("# Refund request"), "Refund request");
        // Inner apostrophe is preserved (only wrapping quotes stripped).
        assert_eq!(sanitize_title("What's my balance"), "What's my balance");
    }

    #[test]
    fn sanitize_title_blank_is_empty() {
        assert_eq!(sanitize_title(""), "");
        assert_eq!(sanitize_title("   \n\t "), "");
        // Only wrapping symbols ⇒ empty (callers reject).
        assert_eq!(sanitize_title("\"\"  "), "");
    }

    #[test]
    fn sanitize_title_caps_length() {
        let long = "word ".repeat(40); // 200 chars
        let out = sanitize_title(&long);
        assert!(out.chars().count() <= TITLE_MAX, "capped: {out:?}");
    }

    #[test]
    fn title_request_body_gives_reasoning_headroom() {
        // Regression: the title model is a reasoning model, so max_tokens must
        // leave room for reasoning + the title. The original 32 was fully eaten
        // by reasoning tokens and yielded empty content. Guard a generous budget.
        let body = title_request_body("What is the capital of France?", "Paris.");
        let max = body["max_tokens"].as_u64().expect("max_tokens present");
        assert!(max >= 256, "auto-title needs reasoning headroom, got {max}");
        assert_eq!(body["model"], AUTO_TITLE_MODEL);
        let prompt = body["messages"][0]["content"].as_str().unwrap();
        assert!(
            prompt.contains("capital of France"),
            "prompt carries the user message"
        );
        assert!(prompt.contains("Paris."), "prompt carries the reply");
    }

    #[test]
    fn per_agent_model_used_when_turn_body_absent() {
        // No per-turn model → the per-agent default stands.
        let cfg = cfg_with_model(Some("claude-sonnet-5"));
        let body = json!({ "message": "hi" });
        let llm = apply_model_override(apply_agent_model_override(base_llm(), Some(&cfg)), &body);
        assert_eq!(llm.model, "claude-sonnet-5");
    }
}
