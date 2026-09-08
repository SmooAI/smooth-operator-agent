//! AWS Lambda entry point — serves the smooth-operator protocol over API
//! Gateway WebSocket.
//!
//! ## Why this differs from the axum reference server
//! API Gateway WebSocket is **not** a persistent socket from the Lambda's
//! perspective. Each inbound frame is a **separate Lambda invocation** carrying
//! a `connectionId` + `domainName` + `stage` in `requestContext`. There is no
//! socket to write to and no in-process state across invocations:
//!
//! - **State** lives entirely in DynamoDB (the `smooth-operator` single
//!   table) via the [`DynamoDbAdapter`](smooth_operator_adapter_dynamodb::DynamoDbAdapter).
//! - **Outbound events** are sent with the API Gateway **Management API**
//!   ([`ConnectionPoster`](crate::poster::ConnectionPoster)) — `post_to_connection`
//!   against `https://{domainName}/{stage}` — not a socket write.
//!
//! ## Route dispatch
//! `requestContext.routeKey` selects the behavior:
//!
//! | route | behavior |
//! | --- | --- |
//! | `$connect` | record the connection in DynamoDB → 200 |
//! | `$disconnect` | delete the connection record → 200 |
//! | `send_message` / `ping` / `create_conversation_session` / `get_session` / `$default` | parse the action envelope and dispatch (see [`dispatch`]); for `$default`, the `action` field in the body selects the behavior |
//!
//! For action routes the Lambda returns `200` immediately after posting every
//! produced event back over the Management API; protocol-level failures are
//! posted as `error` events, never returned as Lambda errors (which would drop
//! the connection / trigger retries).

// The modules live in the LIB (src/lib.rs) and this binary is a thin shim over
// it. Re-declaring them here with `mod` would compile every file a second time
// into the bin, where anything only the lib's tests use — the capturing poster
// variant — is dead code, and clippy's `-D warnings` fails the build on it.
use smooai_smooth_operator_lambda::{adapter, config, connection, dispatch, poster};

use std::sync::Arc;
use std::sync::OnceLock;

use aws_lambda_events::apigw::{ApiGatewayProxyResponse, ApiGatewayWebsocketProxyRequest};
use lambda_runtime::{service_fn, Error, LambdaEvent};

use smooth_operator::adapter::StorageAdapter;
use smooth_operator::auth::AuthVerifier;
use smooth_operator_adapter_dynamodb::DynamoDbAdapter;

use config::LambdaConfig;
use poster::ConnectionPoster;

/// Process-global state, built once on cold start and reused across warm
/// invocations: the resolved config + the DynamoDB adapter (which holds the
/// AWS clients). Lambda keeps the process warm between invocations, so this
/// avoids rebuilding the AWS config / DynamoDB client every request.
struct Shared {
    config: LambdaConfig,
    adapter: Arc<DynamoDbAdapter>,
    /// The env-configured, secure-by-default auth verifier (see
    /// [`smooth_operator::auth::AuthConfig`]). Used to turn a per-frame bearer
    /// `token` into the requester's document-level [`AccessContext`] so chat
    /// retrieval is access-controlled. A `send_message` with no/invalid token
    /// runs anonymous (org-public knowledge only — fail closed for ACL'd docs).
    auth: Arc<dyn AuthVerifier>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Tracing + OpenTelemetry GenAI export to CloudWatch / OTLP. Honors
    // RUST_LOG; installs an OTLP exporter only when OTEL_EXPORTER_OTLP_ENDPOINT
    // is set (otherwise local-only logging, no collector needed). Idempotent.
    smooth_operator::init_telemetry();

    // Build shared state once on cold start, so the handler closure can borrow
    // it for every invocation without rebuilding AWS clients.
    let config = LambdaConfig::from_env();
    let adapter = adapter::build_storage(&config).await?;
    // Secure-by-default auth verifier (jwt|smoo|none; disabled if unconfigured).
    // A misconfiguration (e.g. AUTH_MODE=jwt with no key) is a hard cold-start
    // error — the handler refuses to serve rather than silently run no-auth.
    let auth: Arc<dyn AuthVerifier> = Arc::from(
        smooth_operator::auth::AuthConfig::from_env()
            .map_err(|e| Error::from(format!("auth configuration error: {e}")))?,
    );
    tracing::info!(
        table = %config.table,
        org = %config.org_id,
        model = %config.model,
        llm_enabled = config.has_llm(),
        s3_vectors = config.vector_bucket.is_some(),
        auth_mode = auth.mode(),
        "smooth-operator-lambda cold start"
    );
    let _ = SHARED.set(Shared {
        config,
        adapter,
        auth,
    });

    lambda_runtime::run(service_fn(handler)).await
}

/// One Lambda invocation = one inbound WebSocket frame.
async fn handler(
    event: LambdaEvent<ApiGatewayWebsocketProxyRequest>,
) -> Result<ApiGatewayProxyResponse, Error> {
    let shared = SHARED
        .get()
        .expect("shared state initialized on cold start");
    let ctx = &event.payload.request_context;

    let route_key = ctx.route_key.as_deref().unwrap_or("$default");
    let connection_id = ctx.connection_id.as_deref().unwrap_or_default();

    match route_key {
        "$connect" => {
            if let Err(e) = connection::record_connect(&shared.adapter, connection_id).await {
                tracing::error!(error = %e, "$connect record failed");
                return Ok(http_response(500));
            }
            Ok(http_response(200))
        }
        "$disconnect" => {
            if let Err(e) = connection::record_disconnect(&shared.adapter, connection_id).await {
                // A failed cleanup is non-fatal (TTL will reap the row); log and
                // still 200 so API Gateway doesn't retry a disconnect.
                tracing::warn!(error = %e, "$disconnect cleanup failed");
            }
            Ok(http_response(200))
        }
        // Every action route (`send_message`, `ping`, `create_conversation_session`,
        // `get_session`) and the catch-all `$default` parse the body's `action`
        // field and dispatch the same way; the post-back transport carries the
        // events, so the HTTP response just acks the invocation.
        _ => {
            let Some(domain_name) = ctx.domain_name.as_deref() else {
                tracing::error!("missing domainName in request context; cannot post back");
                return Ok(http_response(500));
            };
            let stage = ctx.stage.as_deref().unwrap_or("$default");
            let body = event.payload.body.as_deref().unwrap_or("");

            let poster = ConnectionPoster::new(domain_name, stage, connection_id).await;
            // Coerce the concrete adapter to the trait object the protocol logic
            // + runner expect (cheap Arc clone).
            let storage: Arc<dyn StorageAdapter> = shared.adapter.clone();

            if let Err(e) =
                dispatch::handle_frame(&storage, &shared.config, &shared.auth, &poster, body).await
            {
                // A post-back failure (e.g. transient Management API error) is
                // logged; we still ack the invocation so API Gateway doesn't
                // retry an already-partially-streamed turn.
                tracing::error!(error = %e, route = route_key, "frame dispatch failed");
            }
            Ok(http_response(200))
        }
    }
}

/// Build a minimal API Gateway proxy response with the given status. API
/// Gateway WebSocket uses the same proxy-response shape; the body is empty
/// because the real payload is streamed back over the Management API.
fn http_response(status: i64) -> ApiGatewayProxyResponse {
    ApiGatewayProxyResponse {
        status_code: status,
        headers: Default::default(),
        multi_value_headers: Default::default(),
        body: None,
        is_base64_encoded: false,
    }
}
