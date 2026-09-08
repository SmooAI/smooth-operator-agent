//! Domain model for smooth-operator.
//!
//! These structs mirror `spec/domain/*.json` exactly (field names, optionality)
//! and are storage-agnostic — no backend is named here. They are the shapes the
//! `StorageAdapter` (see [`crate::adapter`]) reads and writes.
//!
//! Checkpoints are *not* redefined here: we re-use smooth-operator's
//! [`Checkpoint`](smooth_operator_core::Checkpoint) directly so the engine plugs
//! straight into the checkpoint slice. See [`crate::adapter`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

// Re-export the engine's Checkpoint so callers get the domain "Checkpoint"
// from one place. spec/domain/checkpoint.schema.json documents this struct
// as "the `Checkpoint` struct in the smooth-operator Rust crate".
pub use smooth_operator_core::Checkpoint;

/// The channel on which a conversation takes place.
/// Mirrors `conversation.schema.json#/properties/platform`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Web,
    Messenger,
    Instagram,
    Email,
    Discord,
    Phone,
    Sms,
    Slack,
    Whatsapp,
    Tiktok,
}

/// A conversation thread between participants.
/// Mirrors `conversation.schema.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Conversation {
    pub id: String,
    pub platform: Platform,
    pub name: String,
    pub organization_id: String,
    pub idempotency_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_json: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analytics_json: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Participant role discriminator.
/// Mirrors `participant.schema.json#/properties/type` (`user` | `ai-agent` | `human-agent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParticipantType {
    User,
    AiAgent,
    HumanAgent,
}

/// A participant in a conversation.
/// Mirrors `participant.schema.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Participant {
    pub id: String,
    pub conversation_id: String,
    pub organization_id: String,
    #[serde(rename = "type")]
    pub participant_type: ParticipantType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_info: Option<Value>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crm_contact_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_json: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Message direction relative to the platform.
/// Mirrors `message.schema.json#/properties/direction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Inbound,
    Outbound,
}

/// A single content element within a message.
/// Mirrors `message.schema.json#/$defs/ContentItem`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentItem {
    /// Content item type discriminator: `"text"` or `"image"`.
    #[serde(rename = "type")]
    pub item_type: String,
    /// The text content (present when `type = "text"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// A `data:`/`https` image URL (present when `type = "image"`). Persisting it
    /// on the stored user message is what lets a DIFFERENT client re-render an
    /// image another client attached — cross-client conversation parity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl ContentItem {
    /// Build a `text` content item.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            item_type: "text".to_string(),
            text: Some(text.into()),
            url: None,
        }
    }

    /// Build an `image` content item from a `data:`/`https` URL.
    pub fn image(url: impl Into<String>) -> Self {
        Self {
            item_type: "image".to_string(),
            text: None,
            url: Some(url.into()),
        }
    }
}

/// Structured content of a message.
/// Mirrors `message.schema.json#/$defs/MessageContent`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageContent {
    #[serde(default)]
    pub items: Vec<ContentItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_response: Option<Value>,
}

impl MessageContent {
    /// Convenience: a single text item plus the flat-text mirror.
    pub fn from_text(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            items: vec![ContentItem::text(text.clone())],
            text: Some(text),
            structured_response: None,
        }
    }

    /// A text item plus one `image` item per URL. Used to persist a user turn's
    /// attached images so another client rendering this conversation's history
    /// re-shows them (cross-client parity). With no URLs this is `from_text`.
    pub fn from_text_and_images(
        text: impl Into<String>,
        image_urls: impl IntoIterator<Item = String>,
    ) -> Self {
        let text = text.into();
        let mut items = vec![ContentItem::text(text.clone())];
        items.extend(image_urls.into_iter().map(ContentItem::image));
        Self {
            items,
            text: Some(text),
            structured_response: None,
        }
    }
}

/// Abbreviated sender/recipient descriptor (wire shape).
/// Mirrors `message.schema.json#/properties/from` (and `to`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParticipantRef {
    pub id: String,
    #[serde(rename = "type")]
    pub participant_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A single message within a conversation.
/// Mirrors `message.schema.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    pub direction: Direction,
    pub content: MessageContent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<ParticipantRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<ParticipantRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_json: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analytics_json: Option<Value>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

/// Lifecycle status of a session.
/// Mirrors `session.schema.json#/properties/status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Active,
    Idle,
    Ended,
}

/// An AI conversation session — ties a conversation to a smooth-operator
/// workflow thread via `thread_id`.
/// Mirrors `session.schema.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub session_id: String,
    pub conversation_id: String,
    /// Owning organization. Mirrors `organization_id` on `Conversation`,
    /// `Participant`, and `Message` so org-scoping is uniform across every
    /// core domain type — storage backends can write the session's org
    /// directly instead of re-deriving it from the conversation.
    pub organization_id: String,
    /// The agent this session talks to, as supplied by the caller. `None` when the
    /// caller named no agent — this used to be filled with a fresh UUID, which made
    /// every agentless session point at an agent that has never existed (th-68897a).
    /// Absent is the honest answer; a fabricated reference is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub agent_name: String,
    pub user_participant_id: String,
    pub agent_participant_id: String,
    /// smooth-operator workflow thread identifier (the historical
    /// `langgraph_thread_id`). Resumes agent state across turns.
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SessionStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<DateTime<Utc>>,
}

/// A source the agent used to ground its answer.
///
/// Mirrors `spec/domain/citation.schema.json` and the optional `citations`
/// array on the terminal `eventual_response` event. Each citation points back at
/// one retrieved knowledge-base document — the chunk the model read plus enough
/// metadata to render an attribution link.
///
/// Citations are built from the
/// [`KnowledgeResult`](smooth_operator_core::KnowledgeResult)s that actually
/// grounded a turn (see [`Citation::from_knowledge_result`] /
/// [`From<KnowledgeResult>`]): `id` ← `document_id`, `title` ← `source`,
/// `url` ← `source` when it is an `http(s)` URL (the GitHub blob/issue URL the
/// connector stamps onto the document's `source` at ingest — see
/// `docs/CONNECTORS.md`) else `None`, `snippet` ← the chunk truncated for
/// display, `score` ← `score`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Citation {
    /// Stable identifier of the cited source document (the knowledge-base
    /// `document_id`). Used to deduplicate citations within a turn.
    pub id: String,
    /// Human-readable label for the source — the document's source path or, for
    /// web-sourced docs, the URL/title.
    pub title: String,
    /// Canonical link to the source, when one exists. For GitHub-sourced
    /// documents this is the blob/issue URL stamped onto the document's `source`
    /// at ingest. `None` for sources with no web location (e.g. uploaded files).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The retrieved chunk text that grounded the answer, truncated for display.
    pub snippet: String,
    /// Relevance score of this source for the turn's query (the knowledge-base
    /// similarity score). Higher is more relevant.
    pub score: f32,
}

/// Max characters of a chunk to carry as a citation `snippet`. Bounds the size
/// of the `eventual_response` payload; the full chunk lives in the KB.
pub const CITATION_SNIPPET_MAX_CHARS: usize = 280;

impl Citation {
    /// Build a [`Citation`] from a knowledge-base
    /// [`KnowledgeResult`](smooth_operator_core::KnowledgeResult).
    ///
    /// - `id` ← `document_id`
    /// - `title` ← `source`
    /// - `url` ← `source` when it parses as an `http`/`https` URL (the GitHub
    ///   blob/issue URL the connector stamps onto `Document.source` at ingest —
    ///   `docs/CONNECTORS.md`), otherwise `None` (e.g. a local `policies/x.md`
    ///   path has no web location).
    /// - `snippet` ← `chunk`, truncated to [`CITATION_SNIPPET_MAX_CHARS`] on a
    ///   char boundary (an ellipsis appended when truncated).
    /// - `score` ← `score`
    #[must_use]
    pub fn from_knowledge_result(result: &smooth_operator_core::KnowledgeResult) -> Self {
        Self {
            id: result.document_id.clone(),
            title: result.source.clone(),
            url: web_url(&result.source),
            snippet: truncate_snippet(&result.chunk, CITATION_SNIPPET_MAX_CHARS),
            score: result.score,
        }
    }
}

impl From<&smooth_operator_core::KnowledgeResult> for Citation {
    fn from(result: &smooth_operator_core::KnowledgeResult) -> Self {
        Self::from_knowledge_result(result)
    }
}

impl From<smooth_operator_core::KnowledgeResult> for Citation {
    fn from(result: smooth_operator_core::KnowledgeResult) -> Self {
        Self::from_knowledge_result(&result)
    }
}

/// Return `Some(source)` when `source` is an `http`/`https` URL (the citation's
/// `url`), else `None`. GitHub-sourced documents carry the blob/issue URL in
/// `Document.source`; local docs carry a path, which has no web location.
fn web_url(source: &str) -> Option<String> {
    if source.starts_with("http://") || source.starts_with("https://") {
        Some(source.to_string())
    } else {
        None
    }
}

/// Truncate `text` to at most `max` chars on a char boundary, appending `…`
/// when truncation occurred. Empty/short text is returned unchanged.
fn truncate_snippet(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ts() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-07T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn participant_serializes_camelcase_and_kebab_type() {
        let p = Participant {
            id: "p1".into(),
            conversation_id: "c1".into(),
            organization_id: "org1".into(),
            participant_type: ParticipantType::AiAgent,
            external_id: None,
            internal_id: Some("agent-uuid".into()),
            browser_fingerprint: None,
            browser_info: None,
            name: "Smantha".into(),
            email: None,
            phone: None,
            crm_contact_id: None,
            metadata_json: None,
            created_at: ts(),
            updated_at: ts(),
        };
        let v = serde_json::to_value(&p).unwrap();
        // camelCase field names match the spec
        assert!(v.get("conversationId").is_some());
        assert!(v.get("organizationId").is_some());
        assert!(v.get("internalId").is_some());
        // `type` discriminator is kebab-cased per the enum spec
        assert_eq!(v.get("type").unwrap(), &json!("ai-agent"));
        // round-trip
        let back: Participant = serde_json::from_value(v).unwrap();
        assert_eq!(back.participant_type, ParticipantType::AiAgent);
    }

    #[test]
    fn participant_type_variants_match_spec() {
        assert_eq!(
            serde_json::to_value(ParticipantType::User).unwrap(),
            json!("user")
        );
        assert_eq!(
            serde_json::to_value(ParticipantType::AiAgent).unwrap(),
            json!("ai-agent")
        );
        assert_eq!(
            serde_json::to_value(ParticipantType::HumanAgent).unwrap(),
            json!("human-agent")
        );
    }

    #[test]
    fn message_serializes_direction_and_content_items() {
        let m = Message {
            id: "m1".into(),
            external_id: None,
            organization_id: Some("org1".into()),
            conversation_id: Some("c1".into()),
            direction: Direction::Inbound,
            content: MessageContent::from_text("hello"),
            from: Some(ParticipantRef {
                id: "p1".into(),
                participant_type: "user".into(),
                name: Some("Visitor".into()),
            }),
            to: None,
            metadata_json: None,
            analytics_json: None,
            created_at: ts(),
            updated_at: None,
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v.get("direction").unwrap(), &json!("inbound"));
        assert_eq!(v["content"]["items"][0]["type"], json!("text"));
        assert_eq!(v["content"]["items"][0]["text"], json!("hello"));
        assert_eq!(v["content"]["text"], json!("hello"));
        // `from` uses camelCase `id`/`type`
        assert_eq!(v["from"]["type"], json!("user"));
        let back: Message = serde_json::from_value(v).unwrap();
        assert_eq!(back.direction, Direction::Inbound);
    }

    #[test]
    fn content_with_images_serializes_image_items_web_reads() {
        // th-1fca98: a user turn's attached images are persisted as `image`
        // content items so ANOTHER client re-renders them from history. This is
        // the exact wire shape the web SPA parses (`items[].type` / `items[].url`).
        let c = MessageContent::from_text_and_images(
            "look at this",
            [
                "data:image/png;base64,AAAA".to_string(),
                "https://x/y.png".to_string(),
            ],
        );
        let v = serde_json::to_value(&c).unwrap();
        // Text item first (flat-text mirror preserved), then one image item per URL.
        assert_eq!(v["items"][0]["type"], json!("text"));
        assert_eq!(v["items"][0]["text"], json!("look at this"));
        assert_eq!(v["text"], json!("look at this"));
        assert_eq!(v["items"][1]["type"], json!("image"));
        assert_eq!(v["items"][1]["url"], json!("data:image/png;base64,AAAA"));
        assert_eq!(v["items"][2]["type"], json!("image"));
        assert_eq!(v["items"][2]["url"], json!("https://x/y.png"));
        // A text item carries no `url` key (skip_serializing_if None).
        assert!(v["items"][0].get("url").is_none());
        // Round-trips back through the domain type.
        let back: MessageContent = serde_json::from_value(v).unwrap();
        assert_eq!(back.items.len(), 3);
        assert_eq!(back.items[1].item_type, "image");
        assert_eq!(
            back.items[1].url.as_deref(),
            Some("data:image/png;base64,AAAA")
        );
        // No images ⇒ identical to from_text (no regression to text-only turns).
        let text_only = MessageContent::from_text_and_images("hi", std::iter::empty());
        assert_eq!(text_only.items.len(), 1);
        assert_eq!(text_only.items[0].item_type, "text");
    }

    #[test]
    fn session_uses_thread_id_camelcase() {
        let s = Session {
            session_id: "s1".into(),
            conversation_id: "c1".into(),
            organization_id: "org1".into(),
            agent_id: Some("a1".into()),
            agent_name: "Smantha".into(),
            user_participant_id: "pu".into(),
            agent_participant_id: "pa".into(),
            thread_id: "thread-xyz".into(),
            status: Some(SessionStatus::Active),
            token_count: Some(0),
            message_count: Some(0),
            metadata: None,
            created_at: Some(ts()),
            updated_at: Some(ts()),
            ended_at: None,
            last_activity_at: Some(ts()),
        };
        let v = serde_json::to_value(&s).unwrap();
        assert!(v.get("sessionId").is_some());
        assert!(v.get("conversationId").is_some());
        assert!(v.get("userParticipantId").is_some());
        assert!(v.get("agentParticipantId").is_some());
        assert_eq!(v.get("threadId").unwrap(), &json!("thread-xyz"));
        assert_eq!(v.get("status").unwrap(), &json!("active"));
        let back: Session = serde_json::from_value(v).unwrap();
        assert_eq!(back.thread_id, "thread-xyz");
        assert_eq!(back.status, Some(SessionStatus::Active));
    }

    #[test]
    fn conversation_platform_and_camelcase() {
        let c = Conversation {
            id: "c1".into(),
            platform: Platform::Web,
            name: "Lead chat".into(),
            organization_id: "org1".into(),
            idempotency_key: "idem-1".into(),
            metadata_json: Some(json!({"campaign": "spring"})),
            analytics_json: None,
            created_at: ts(),
            updated_at: ts(),
        };
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v.get("platform").unwrap(), &json!("web"));
        assert!(v.get("organizationId").is_some());
        assert!(v.get("idempotencyKey").is_some());
        assert_eq!(v["metadataJson"]["campaign"], json!("spring"));
        let back: Conversation = serde_json::from_value(v).unwrap();
        assert_eq!(back.platform, Platform::Web);
    }
}
