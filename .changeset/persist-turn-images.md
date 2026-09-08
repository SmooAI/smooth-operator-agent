---
"@smooai/smooth-operator": patch
---

th-1fca98: persist a user turn's attached images onto the stored inbound message so other clients re-render them.

Images on `send_message` rode the live LLM turn only — the inbound message was stored text-only (`MessageContent::from_text`), so a DIFFERENT client reading the conversation's history (e.g. desktop viewing a photo the iOS app sent) saw text with no picture. `ContentItem` now carries an optional `url` and an `"image"` type; the runner persists each of the turn's image URLs as an `image` content item alongside the text. Text-only turns are byte-for-byte unchanged (`from_text` still used when there are no images), and the addition is backward-compatible on the wire (`url` is optional, `image` is additive to the schema enum).
