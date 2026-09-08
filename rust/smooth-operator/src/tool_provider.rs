//! Host-contributed tool injection seam.
//!
//! The reference runner assembles a fixed [`ToolRegistry`] of built-in tools
//! (`knowledge_search`, …) for every turn. A *host* — a deployment flavor that
//! embeds this runner (e.g. SmooAI's k8s flavor) — often needs to contribute
//! its OWN tools to a turn: per-org integrations, a CRM lookup, a ticketing
//! action, etc. Those tools depend on host-specific state (DB handles, an org's
//! connector config) that has no place in this shared crate.
//!
//! [`ToolProvider`] is the mechanism: a host installs one provider, and the
//! runner asks it — per turn, with the turn's [`ToolProviderContext`] — for the
//! extra tools to MERGE with the built-ins. The shared crate stays free of any
//! host/DB specifics; it only knows "ask the provider, register what it
//! returns". When no provider is installed the registry is exactly the
//! built-ins, so default behavior is byte-for-byte unchanged.
//!
//! ## Org-scoping
//!
//! [`ToolProviderContext`] carries the turn's [`AccessContext`] (the requester's
//! entitlements) and an optional `org_id`, so a provider can return per-org
//! tools and apply the requester's entitlements when wiring them. The shared
//! crate does not interpret `org_id` — it only carries it through.
//!
//! ## Per-turn handles
//!
//! Beyond org-scoping, a host's tools often need two more per-turn facts the
//! runner already has in hand: the turn's `conversation_id` (so a tool can
//! persist or correlate to the conversation it runs in) and the resolved
//! per-org `gateway_key` (so a retrieval-style host tool can call the same LLM
//! gateway this turn was billed/scoped to). Both are carried as `Option` and
//! the shared crate never interprets them — it only threads them through.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use smooth_operator_core::Tool;

use crate::access_control::AccessContext;

/// An image attachment on a multimodal turn's user message. `url` is a `data:`
/// image URL (`data:image/png;base64,...`) or a remote `https` URL; `detail`
/// (`"low"`/`"high"`/`"auto"`) is the optional OpenAI vision hint, omitted when
/// absent. The wire shape mirrors the core `ImageContent` — the server maps one
/// to the other before handing the turn to the engine. Shared by the inbound
/// `send_message` request (`images`) and the [`ToolProviderContext`] so a host
/// tool can see what the turn carried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserImage {
    /// A `data:`/`https` image URL, emitted to the model as an OpenAI
    /// `image_url` content part.
    pub url: String,
    /// Optional OpenAI vision detail hint (`"low"`/`"high"`/`"auto"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A non-image file attachment on a turn's user message (the `send_message`
/// `files[]` array). Unlike [`UserImage`], a file is NOT emitted to the model —
/// it only rides the [`ToolProviderContext`] so the host can persist it. `url`
/// is a `data:`/`https` URL; `mime` is the optional MIME type (`mimeType` on the
/// wire, per the spec), omitted when absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserFile {
    /// A human-facing file name.
    pub name: String,
    /// Optional MIME type. Wire key is `mimeType` (spec); local field is `mime`.
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    /// A `data:`/`https` URL locating the file's bytes.
    pub url: String,
}

/// The per-turn context a [`ToolProvider`] sees when asked for tools.
///
/// Carries everything a host needs to decide which tools a turn gets WITHOUT
/// leaking host/DB specifics into this crate: the requester's entitlements and
/// the (optional) owning org. A host keys its tool catalog off `org_id` and
/// scopes side-effectful tools to `access`.
#[derive(Debug, Clone, Default)]
pub struct ToolProviderContext {
    /// The owning organization for this turn, when known. `None` for a turn
    /// with no resolved org (e.g. an anonymous reference-server connection).
    pub org_id: Option<String>,
    /// The requester's document-level entitlements for this turn. A provider
    /// that returns retrieval-style tools should bind them to this context so a
    /// host tool never surfaces content the requester may not read.
    pub access: AccessContext,
    /// The conversation this turn belongs to, when known. A host tool that
    /// persists or correlates to the conversation it runs in reads this; `None`
    /// for a turn with no resolved conversation. The shared crate does not
    /// interpret it.
    pub conversation_id: Option<String>,
    /// The resolved per-org LLM-gateway key for this turn, when one was
    /// resolved. A retrieval-style host tool (e.g. agent-brain's
    /// `knowledge_search`) reads this to call the same gateway the turn was
    /// billed/scoped to. `None` when no key resolved. The shared crate does not
    /// interpret it.
    pub gateway_key: Option<String>,
    /// An opaque credential letting a host tool act AS THE ACTING USER against
    /// the host's own APIs (th-8400b7). `None` when the host does not supply one
    /// — the default, and what every existing deployment gets.
    ///
    /// The shared crate does not interpret this, exactly as it does not
    /// interpret [`gateway_key`](Self::gateway_key). That is deliberate: it
    /// keeps the POLICY with the host. smooai puts a short-lived, narrowly
    /// scoped minted token here rather than the raw session bearer, so a leak
    /// costs five minutes and one scope instead of a reusable user session —
    /// but a different host may forward whatever it likes, and this crate never
    /// has to know which.
    ///
    /// ## Why this exists
    ///
    /// The operator verifies the connection's JWT and then hands tools only
    /// [`AccessContext`] — user id, groups, org — with the bearer dropped. That
    /// is the right default, and it had a cost: host tools could not call the
    /// host's own REST API as the user, so smooai's copilot drawer grew raw-SQL
    /// CRM writes that bypassed the routes' validation, write gate, outbound
    /// sync and activity log. Its MCP surface, whose transport IS an
    /// authenticated HTTP session, called the API and stayed correct. Two tool
    /// stacks diverged on transport, field coverage and side effects because one
    /// of them had no way to authenticate.
    ///
    /// A host that sets this can converge them. A host that does not is
    /// unaffected.
    pub user_token: Option<String>,
    /// Per-tool config from the agent's `tool_config.enabledTools[*].config`,
    /// keyed by tool id — the operator analog of `registry.ts`'s
    /// `toolSpecificConfig`. A host tool reads its own entry to configure itself
    /// for this agent's turn. Empty when no tool carries config; the shared crate
    /// does not interpret it.
    pub tool_specific_config: std::collections::HashMap<String, serde_json::Value>,
    /// Optional **directive sink** — where a host tool writes a client-side
    /// directive (a navigation / view-application instruction) for this turn. The
    /// runner drains it after the turn and carries the value onto the
    /// `eventual_response`'s `directive` field. Opaque `serde_json::Value`
    /// (last-write-wins): the shared crate never interprets the shape, exactly as
    /// `response` is left loose. `None` (the default) ⇒ no host directive path, so
    /// behavior is byte-for-byte unchanged.
    pub directive_sink: Option<Arc<Mutex<serde_json::Value>>>,
    /// The image attachments this turn carried (multimodal turns). A host tool may
    /// read them; empty for the text-only common case. The runner also maps these
    /// onto the engine's user message via core `with_user_images`.
    pub images: Vec<UserImage>,
    /// The non-image file attachments this turn carried. Unlike `images`, these
    /// are NOT sent to the model — they ride the context only so a host tool can
    /// persist them. Empty for the common case.
    pub files: Vec<UserFile>,
}

impl ToolProviderContext {
    /// Build a context from an optional org id and the requester's access.
    ///
    /// The per-turn [`conversation_id`](Self::conversation_id) and
    /// [`gateway_key`](Self::gateway_key) default to `None`; set them with
    /// [`with_conversation_id`](Self::with_conversation_id) /
    /// [`with_gateway_key`](Self::with_gateway_key).
    #[must_use]
    pub fn new(org_id: Option<String>, access: AccessContext) -> Self {
        Self {
            org_id,
            access,
            conversation_id: None,
            gateway_key: None,
            user_token: None,
            tool_specific_config: std::collections::HashMap::new(),
            directive_sink: None,
            images: Vec::new(),
            files: Vec::new(),
        }
    }

    /// Set the per-tool config map (`tool_id` → config object).
    #[must_use]
    pub fn with_tool_configs(
        mut self,
        configs: std::collections::HashMap<String, serde_json::Value>,
    ) -> Self {
        self.tool_specific_config = configs;
        self
    }

    /// Set the turn's [`conversation_id`](Self::conversation_id).
    #[must_use]
    pub fn with_conversation_id(mut self, conversation_id: impl Into<String>) -> Self {
        self.conversation_id = Some(conversation_id.into());
        self
    }

    /// Set the turn's [`user_token`](Self::user_token) — an opaque act-as-user
    /// credential for the host's own APIs. Prefer a short-lived scoped token
    /// over a long-lived session bearer; this crate cannot tell the difference
    /// and will not try.
    #[must_use]
    pub fn with_user_token(mut self, user_token: impl Into<String>) -> Self {
        self.user_token = Some(user_token.into());
        self
    }

    /// Set the turn's resolved [`gateway_key`](Self::gateway_key).
    #[must_use]
    pub fn with_gateway_key(mut self, gateway_key: impl Into<String>) -> Self {
        self.gateway_key = Some(gateway_key.into());
        self
    }

    /// Set the turn's [`directive_sink`](Self::directive_sink) — the slot a host
    /// tool writes a client-side directive into for this turn.
    #[must_use]
    pub fn with_directive_sink(mut self, sink: Arc<Mutex<serde_json::Value>>) -> Self {
        self.directive_sink = Some(sink);
        self
    }

    /// Set the turn's [`images`](Self::images) — the attachments the turn carried.
    #[must_use]
    pub fn with_images(mut self, images: Vec<UserImage>) -> Self {
        self.images = images;
        self
    }

    /// Set the turn's [`files`](Self::files) — the non-image attachments the turn
    /// carried, for the host to persist. Never sent to the model.
    #[must_use]
    pub fn with_files(mut self, files: Vec<UserFile>) -> Self {
        self.files = files;
        self
    }
}

/// Host seam for contributing EXTRA tools to a turn's [`ToolRegistry`].
///
/// The runner calls [`tools_for`](ToolProvider::tools_for) once per turn and
/// merges the returned tools with the built-ins (built-ins registered first;
/// a returned tool whose name collides with a built-in replaces it — the host
/// opted into that by naming it the same). Returning an empty `Vec` (or not
/// installing a provider at all) leaves the registry as exactly the built-ins.
///
/// Async so a provider may consult host state (config store, DB) to resolve an
/// org's tool catalog.
#[async_trait]
pub trait ToolProvider: Send + Sync {
    /// The extra tools to merge into this turn's registry. May be empty.
    async fn tools_for(&self, ctx: &ToolProviderContext) -> Vec<Arc<dyn Tool>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use smooth_operator_core::{ToolRegistry, ToolSchema};

    /// A trivial tool used to prove injected tools land in the registry.
    struct StubTool {
        name: String,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: self.name.clone(),
                description: "stub".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }
        async fn execute(&self, _arguments: serde_json::Value) -> anyhow::Result<String> {
            Ok("ok".into())
        }
    }

    /// A provider that returns a fixed set of stub tools.
    struct StubProvider {
        names: Vec<String>,
    }

    #[async_trait]
    impl ToolProvider for StubProvider {
        async fn tools_for(&self, _ctx: &ToolProviderContext) -> Vec<Arc<dyn Tool>> {
            self.names
                .iter()
                .map(|n| Arc::new(StubTool { name: n.clone() }) as Arc<dyn Tool>)
                .collect()
        }
    }

    #[tokio::test]
    async fn provider_tools_register_into_registry() {
        let provider = StubProvider {
            names: vec!["crm_lookup".into(), "open_ticket".into()],
        };
        let ctx = ToolProviderContext::new(Some("org-a".into()), AccessContext::anonymous());

        let mut registry = ToolRegistry::new();
        for tool in provider.tools_for(&ctx).await {
            registry.register_arc(tool);
        }

        assert!(registry.has_tool("crm_lookup"));
        assert!(registry.has_tool("open_ticket"));
    }

    #[test]
    fn new_defaults_per_turn_handles_to_none() {
        let ctx = ToolProviderContext::new(Some("org-a".into()), AccessContext::anonymous());
        assert_eq!(ctx.conversation_id, None);
        assert_eq!(ctx.gateway_key, None);
        assert!(ctx.directive_sink.is_none());
        assert!(ctx.images.is_empty());
        assert!(ctx.files.is_empty());
    }

    /// th-8400b7 — the act-as-user credential is absent unless a host sets it.
    ///
    /// This is the security-relevant half. The operator drops the connection's
    /// bearer by design; that default must survive adding a way to opt out of
    /// it, or every existing deployment silently starts handing tools a
    /// credential nobody asked it to carry.
    #[test]
    fn user_token_is_absent_by_default() {
        let ctx = ToolProviderContext::new(Some("org".into()), AccessContext::default());
        assert_eq!(
            ctx.user_token, None,
            "no host opted in, so no credential may be present"
        );
    }

    /// A host that opts in gets the credential through, uninterpreted.
    #[test]
    fn user_token_round_trips_when_the_host_sets_it() {
        // Deliberately not a JWT-shaped string: this crate must not care what
        // the token IS. smooai puts a short-lived scoped token here; another
        // host may forward something else entirely.
        let ctx = ToolProviderContext::new(Some("org".into()), AccessContext::default())
            .with_user_token("opaque-minted-abc123");
        assert_eq!(ctx.user_token.as_deref(), Some("opaque-minted-abc123"));
        // ...and it does not disturb the credential that was already here.
        assert_eq!(ctx.gateway_key, None);
    }

    #[test]
    fn builder_sets_conversation_id_and_gateway_key() {
        let ctx = ToolProviderContext::new(Some("org-a".into()), AccessContext::anonymous())
            .with_conversation_id("conv-123")
            .with_gateway_key("sk-org-a");
        assert_eq!(ctx.conversation_id.as_deref(), Some("conv-123"));
        assert_eq!(ctx.gateway_key.as_deref(), Some("sk-org-a"));
    }

    #[test]
    fn builder_sets_directive_sink_and_images() {
        let sink = Arc::new(Mutex::new(serde_json::Value::Null));
        let ctx = ToolProviderContext::new(Some("org-a".into()), AccessContext::anonymous())
            .with_directive_sink(Arc::clone(&sink))
            .with_images(vec![UserImage {
                url: "https://x/y.png".into(),
                detail: Some("high".into()),
            }]);
        // A host tool writes through the sink; the runner reads it afterward.
        *ctx.directive_sink.as_ref().unwrap().lock().unwrap() =
            serde_json::json!({"kind": "Navigate"});
        assert_eq!(
            *sink.lock().unwrap(),
            serde_json::json!({"kind": "Navigate"})
        );
        assert_eq!(ctx.images.len(), 1);
        assert_eq!(ctx.images[0].url, "https://x/y.png");
        assert_eq!(ctx.images[0].detail.as_deref(), Some("high"));
    }

    #[test]
    fn builder_sets_files() {
        let ctx = ToolProviderContext::new(Some("org-a".into()), AccessContext::anonymous())
            .with_files(vec![UserFile {
                name: "report.pdf".into(),
                mime: Some("application/pdf".into()),
                url: "https://x/report.pdf".into(),
            }]);
        assert_eq!(ctx.files.len(), 1);
        assert_eq!(ctx.files[0].name, "report.pdf");
        assert_eq!(ctx.files[0].mime.as_deref(), Some("application/pdf"));
        assert_eq!(ctx.files[0].url, "https://x/report.pdf");
    }

    #[test]
    fn user_file_round_trips_with_mime_type_wire_key() {
        // Wire key is `mimeType` (spec); absent MIME omits the key entirely.
        let v = serde_json::to_value(UserFile {
            name: "a.txt".into(),
            mime: Some("text/plain".into()),
            url: "data:text/plain;base64,AAAA".into(),
        })
        .unwrap();
        assert_eq!(v["mimeType"], "text/plain");
        assert!(v.get("mime").is_none());

        let back: UserFile =
            serde_json::from_value(serde_json::json!({"name": "a.txt", "url": "u"})).unwrap();
        assert_eq!(back.name, "a.txt");
        assert_eq!(back.url, "u");
        assert!(back.mime.is_none());
    }

    #[test]
    fn user_image_omits_detail_when_absent() {
        // Back-compat wire shape: no `detail` key when None.
        let v = serde_json::to_value(UserImage {
            url: "data:image/png;base64,AAAA".into(),
            detail: None,
        })
        .unwrap();
        assert!(v.get("detail").is_none());
        assert_eq!(v["url"], "data:image/png;base64,AAAA");
    }

    #[tokio::test]
    async fn empty_provider_leaves_registry_unchanged() {
        let provider = StubProvider { names: vec![] };
        let ctx = ToolProviderContext::default();

        let mut registry = ToolRegistry::new();
        let before = registry.schemas().len();
        for tool in provider.tools_for(&ctx).await {
            registry.register_arc(tool);
        }
        assert_eq!(registry.schemas().len(), before);
    }
}
