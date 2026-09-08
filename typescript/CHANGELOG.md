# @smooai/smooth-operator

## 1.58.10

### Patch Changes

- 8ce1ac5: th-1fca98: persist a user turn's attached images onto the stored inbound message so other clients re-render them.

  Images on `send_message` rode the live LLM turn only — the inbound message was stored text-only (`MessageContent::from_text`), so a DIFFERENT client reading the conversation's history (e.g. desktop viewing a photo the iOS app sent) saw text with no picture. `ContentItem` now carries an optional `url` and an `"image"` type; the runner persists each of the turn's image URLs as an `image` content item alongside the text. Text-only turns are byte-for-byte unchanged (`from_text` still used when there are no images), and the addition is backward-compatible on the wire (`url` is optional, `image` is additive to the schema enum).

## 1.58.9

### Patch Changes

- 6bfc3ed: feat(dotnet,python,ts,go): let a host supply the turn's memory — durable auto-recall off Rust (th-ebe27d)

  Rust #330 put `memory_for_access` on `StorageAdapter` and had the server runner thread the
  result into the engine's agent options, which is what lights up Big Smooth's durable
  auto-recall. The four sibling servers never did — and the gap was invisible, because **all
  five engine cores already implement `Memory` and already recall relevant entries into
  context**. The capability was fully built on both ends with nothing connecting them: no
  matter what store a deployment had, every turn on these servers ran without auto-recall.

  Each server now takes a `MemoryProvider` (`IMemoryProvider` in C#) with one method —
  `memory_for_access(access)` — resolved per turn and passed to the engine as
  `AgentOptions.memory`:

  |            | seam              | install                                       |
  | ---------- | ----------------- | --------------------------------------------- |
  | C#         | `IMemoryProvider` | DI (`services.GetService<IMemoryProvider>()`) |
  | Python     | `MemoryProvider`  | `ServerState.memory_provider`                 |
  | TypeScript | `MemoryProvider`  | `serve({ memoryProvider })`                   |
  | Go         | `MemoryProvider`  | `WithMemoryProvider(...)`                     |

  `access` is threaded exactly as it is for knowledge, so a multi-tenant host can bind memory
  to the requester's org/user; single-tenant hosts — Big Smooth's daemon, the reason the seam
  exists — ignore it, so each language also ships a `StaticMemoryProvider` over one store.

  **Nothing changes for anyone who does not opt in.** No provider, or a provider that returns
  nothing for this caller, leaves the turn byte-for-byte what it was — and that is a test, not
  a claim: each language asserts the no-provider and the declining-provider paths inject
  nothing, alongside the positive case and a relevance case (an unrelated message recalls
  nothing, so this is not a blanket dump of every stored memory into every turn).

  Five tests per language, named after their Rust counterparts in
  `rust/smooth-operator-server/tests/injection_seams.rs`, all four mutation-checked — dropping
  the one line that hands memory to the engine fails them.

  Two notes for whoever picks this up next. The recall block's **header text is deliberately
  not asserted**: the five cores currently inject three different strings for it (th-ffaeae),
  so the tests assert the recalled _content_ reaches the model, which is the behavior the seam
  exists for. And the bundled lexical scorer counts raw token overlap with **no stopword
  filter**, so in practice a single shared "the" scores a hit — worth knowing before trusting
  recall precision in production.

  No wire-protocol change.

## 1.58.8

### Patch Changes

- 55bf1ca: feat(python,go): resolve `send_message.skill` server-side — the last two languages (th-ebe27d)

  The skill seam (Rust #338) let the wire carry **intent** — `skill: "code-review"` —
  instead of prose: the server resolves the name to its markdown body and composes it
  into the turn's system prompt, so the persisted user message stays exactly what the
  user typed and the body is not replayed as context on every later turn. Rust, C#
  and TypeScript had it; the Python and Go servers ignored the field entirely, which
  is worse than not supporting it — a client that asked for a skill got a confident
  **unskilled** answer with no signal that anything was dropped.

  Both now carry the full seam, mirroring `rust/smooth-operator-server/src/skills.rs`:
  a `SkillResolver` host seam (`ServerState.skill_resolver` / `WithSkillResolver`) and
  a `DirSkillResolver` default reading `<root>/<name>/SKILL.md` over the `:`-separated
  roots in `SMOOTH_SKILLS_DIR`, first root wins. Unset ⇒ no resolver is installed and
  any `skill` field is a clean `SKILL_NOT_FOUND`, so a multi-tenant deploy never
  serves host skills by accident.

  Two properties are load-bearing and tested as such:

  - **Fail closed, before the ack.** An unresolvable skill emits `SKILL_NOT_FOUND`
    _instead of_ the 202 and never starts a turn. Resolving after the ack would leave
    the client holding an accepted turn that then errors, and running the turn anyway
    is the silent-degradation bug above. A blank/whitespace `skill` is "no skill",
    not an unknown one, so a client that always sends the field still works.
  - **Traversal is unrepresentable, not filtered.** The name is `[A-Za-z0-9_-]{1,128}`
    — matching the pattern `spec/actions/send-message.schema.json` already declared —
    before it is ever joined onto a filesystem root, so `../../etc/passwd`, `a/b`,
    `a\b` and an embedded NUL cannot round-trip into a path.

  New conformance scenario `skill-unknown-error` pins the fail-closed contract across
  **all five** servers: it needs no filesystem setup (the default server installs no
  resolver), so it is the corpus's oracle for this seam rather than five per-language
  opinions about it.

  No wire-protocol change — the schema already carried the field.

## 1.58.7

### Patch Changes

- 8865fbd: fix(server): a conversation is born on its first message, not on the widget open (SMOODEV-3057)

  Opening the widget wrote a `conversations` row — plus both participants and a
  session — before the visitor had typed anything. In a 30-day production sample
  **44 of 117** web conversations carried zero messages: bare opens occupying an
  inbox row. And because a web create feeds a fresh UUID as the conversation's
  `idempotency_key`, the unique index's `ON CONFLICT DO NOTHING` could never
  collapse a double-connect the way it does for sms/slack/discord, so a
  reconnecting visitor accumulated rows.

  `create_conversation_session` now parks a **bare** open — no `userEmail`, no
  `metadata.userPhone`, no `conversationId` to resume — and writes it on the
  session's first `send_message`, in the same order as before (conversation → user
  participant → agent participant → session). A connection that closes without a
  message drops its parked writes; a reconnect naming the parked `conversationId`
  binds to it, keeping the id and the durable `supports` record.

  The wire is unchanged: the client still gets its `sessionId` / `conversationId`
  back immediately, and the session is usable on that connection at once.

  An open that **carries visitor identity** is not deferred. It is a captured lead,
  and a host adapter may hook the `user` participant write to upsert that visitor
  into its CRM (reading phone + marketing consent off the conversation's
  `metadata_json`), so those still persist immediately — as do every non-web
  channel and every resume.

  A storage blip during the flush is reported as retryable `STORAGE_ERROR` with the
  parked writes kept, and the retry resumes at the step it stopped on rather than
  re-inserting a row whose primary key is already taken.

## 1.58.6

### Patch Changes

- 512cb01: fix(server): a prod failure is diagnosable from logs alone (th-694c22)

  A live "session not found" incident produced ZERO server log lines — chat-ws
  emitted one line in six hours of continuous traffic, because the server's
  decision points were silent: every one of the ~30 `protocol::error(...)` emit
  sites sent the client-visible error frame without logging, and the session
  read-through, confirmation/interaction parks and resolves, OTP verification,
  and turn starts logged nothing at info.

  One warn at the single error-frame construction site now covers every
  client-visible failure, present and future (the frame's human text rides as
  `detail` — `message` is tracing's reserved event-message field). Info lines
  land at the decision points an incident responder actually needs: session
  primed from storage (the cross-pod resume working as designed), turn requested
  (session + requestId), confirmation parked / live-resolved / durably resolved,
  interaction parked / resolved-with-values, and OTP verified. No debug spam —
  one line per event that changes state.

- e0d4213: fix(server): a pending Rich Interaction survives the pod that parked it (th-db0816)

  The interaction sibling of the durable-confirmation fix. A raise's park is a
  channel into a turn on ONE pod, so a visitor whose refresh reconnected them to
  another replica — or whose pod rolled — got `NO_PENDING_INTERACTION` for the
  card they were just shown, and the identity they typed evaporated.

  The interaction bridge now persists every raise into the session's durable
  `metadata.pendingInteraction` (interaction id + kind + spec — the full
  validation contract), retired when the turn ends. `submit_interaction` with no
  live park validates against that record exactly as it would against the park
  (mismatched `interactionId` still rejected, per-field validation still routed
  to the kind's server-side validator, invalid submits leave the record for a
  retry) and resolves it there: submitted values retire the record fail-closed
  and then run the kind's host effect; declined retires it with an ack. No
  continuation turn — the host effect is the durable outcome, and a dead raise's
  model acknowledgment is forgone rather than fabricated.

  `attach_session_identity` now also writes through to storage: a captured
  contact (name/email/phone) used to live only in one pod's map, so any pod roll
  forgot a visitor who had just introduced themselves — even on the same pod.

  Two-instance tests (`tests/durable_interactions.rs`) drive the real
  `handle_frame` on two `AppState`s over one storage adapter: submit-on-the-other-
  pod attaches the identity durably and retires the record, decline retires it,
  mismatched ids are still rejected, and the negative control proves
  `NO_PENDING_INTERACTION` is still reachable with no record. Positive control:
  with the durable read disabled, both cross-instance tests fail reproducing the
  production error while the negative control still passes.

  No wire-protocol change.

- 2904a8a: feat(server): a host can install the turn's `AgentExecutor` — the missing seam on the emitted reply

  `TurnRequest::executor` has been a public field since ADR-030, and
  `runner::turn_executor` has always honored it, but the server's **sole**
  `TurnRequest` construction site (`handler.rs`) hardcoded `executor: None`. So the
  seam existed on paper and was unreachable in practice: nothing outside this crate
  could supply one.

  That gap is what left chat-ws with no host-side seam on the emitted text. When the
  runner owns the whole turn and streams plain text from inside the published crate,
  a host has no point at which to inspect what the agent said next to what it
  actually did — so the TS general agent's post-response guard (which STRIPPED an
  escalation claim when `notify_humans` had not fired) and the voice stall-reply
  retry had nowhere to run. The consequence was live: an agent could tell a customer
  "I've passed it along" with nothing behind it, and the only available fix was
  prompt prevention, not enforcement.

  `AppState::with_executor` installs one, and the handler passes it onto every turn.
  Two things arrive through it:

  - a **durable backend** (ADR-030) — the case the trait was written for; and
  - a **decorator**: an executor that delegates to `InProcessExecutor` and then
    inspects or edits the returned `Conversation` before the runner reads its final
    assistant message. `Conversation.messages` is public and carries the turn's tool
    calls, so this is the one place a host can guard a reply against the tools that
    actually ran.

  One boundary is worth stating plainly rather than discovering later: tokens the
  turn streamed have already left over the events channel by the time the
  conversation is returned, so an edit here changes the persisted message and the
  `eventual_response` — not what already streamed. A decorator that needs the stream
  too can pass its own channel down and forward.

  Default behavior is unchanged: `None` is still the in-process executor, which is a
  verbatim delegation to `Agent::run_with_channel`. The lambda flavor keeps `None`
  alongside its other `None` injection seams (it has no `AppState` to install one
  on).

  `rust/smooth-operator-server/tests/executor_seam.rs` drives the real
  `handle_frame` offline with an escalation-guard executor and pins both halves: the
  installed executor is the one that runs the turn and its rewrite reaches BOTH the
  persisted outbound message and the `eventual_response`; with no executor installed
  the model's text survives byte-for-byte.

## 1.58.5

### Patch Changes

- b3ad578: fix(server): a pending write-confirmation survives the pod that parked it (th-db0816)

  The write-confirmation park was a channel into a turn running on ONE pod. A
  visitor whose refresh reconnected them to a different replica — or whose pod
  rolled mid-park — got `NO_PENDING_CONFIRMATION` for an approval the agent had
  just asked them to give, and the approved write was silently lost. With 2-6
  replicas and no session affinity, that is the expected outcome for any
  reconnect, not a rare race.

  The confirmation bridge now mirrors every park into the session's durable
  `metadata.pendingConfirmation` (tool name + arguments + prompt + requestedAt)
  through the same `SessionUpdate.metadata` write-through the session registry
  already uses: storage is the truth, the in-process channel map is the same-pod
  fast path. `confirm_tool_action` that finds no live sender reads the record
  FRESH from storage and resolves it there:

  - **Approved** → the record is retired first, fail-closed (a record that cannot
    be cleared could execute a write twice, so a failed clear surfaces as a
    retryable storage error instead of proceeding), then a continuation turn runs
    through the normal `send_message` path. A one-shot, server-side pre-approval
    for exactly the recorded tool lets the re-issued call execute instead of
    parking a second time — it is granted only by the resolving handler and never
    readable from the wire, so a client cannot smuggle a confirmation bypass into
    a frame.
  - **Denied** → the record is retired and the confirm is acked; the parked tool
    never runs anywhere (a dead park cannot execute, and a still-parked twin on
    another pod resolves to its timeout rejection).
  - Records expire on the same 300s clock as the in-process park, so a
    pod-death orphan cannot authorize a stale write later.

  The same-pod path is unchanged (sender fed, turn resumes in place) except that
  it now also retires the durable record immediately on resolution.

  `rust/smooth-operator-server/tests/durable_confirmation.rs` drives the real
  `handle_frame` on TWO `AppState`s sharing one storage adapter — the same
  two-instance shape as the session-registry fix. The approval test asserts its
  premise (pod B holds no live park), that the continuation actually executes the
  gated tool WITHOUT a second `write_confirmation_required`, and that the record
  is retired; the decline test asserts retire-without-a-turn; the negative
  control proves `NO_PENDING_CONFIRMATION` is still reachable with no record, so
  the positive tests ride on the record path and nothing else. Positive control:
  with the durable fallback disabled, both cross-instance tests fail reproducing
  the exact production error, while the negative control still passes.

  No wire-protocol change.

## 1.58.4

### Patch Changes

- a8cb4bb: fix(server): an anonymous widget visitor who gives an email is no longer locked out of its own session

  Seen in production on smoo.ai, as a total outage of the public chat.
  `create_conversation_session { agentId, userEmail }` answered 200, and the very
  next `send_message` on the same socket answered `SESSION_NOT_FOUND` for a
  session that plainly existed. `userEmail` alone was the trigger — the same
  create without it, or with only `userName` or `browserFingerprint`, streamed
  fine.

  The widget's pre-chat form collects name + email, so the email lands on the
  visitor's own `user` participant, and `may_read_conversation` counts any `user`
  participant with a non-blank email as making the conversation **owned**. A
  public widget visitor has no verified principal, which on a multi-user
  deployment is `UserScope::Denied`, whose arm was `!owned`. So the visitor was
  owner-checked against an identity it does not have, and denied the session it
  had itself created one frame earlier. The widget's recovery path then created a
  fresh session carrying the same email and was denied identically — so real
  visitors saw an unbounded retry loop and "We couldn't reach the chat", not a
  transient blip. This is th-909995 recurring for the emailful case, against
  `server::anonymous_scope`'s own assertion that "it can still create a fresh
  session, so the anonymous widget flow keeps working": it could create, but not
  use.

  An anonymous connection can never satisfy an ownership check, so it no longer
  faces one — narrowly:

  - The exception applies only to a read reached **by id** (new `Reach::ById`),
    where the unguessable session/conversation id is the visitor's entire
    capability, exactly as it was before scoping shipped.
  - `list_conversations` (`Reach::Listing`) stays strict for everyone. Anonymous
    listing falls back to the SEED org, which is precisely where widget
    conversations pool, so granting the exception there would have leaked
    visitors' chats to each other. A negative control caught that before it
    shipped.
  - Keyed on "no verified principal" (`auth_org.is_none()`, set only by the
    tokenless and degraded-token branches of `resolve_ws_access`), not on the
    scope — an authenticated principal whose token carries no `email` claim still
    fails closed.
  - The tenant boundary is untouched, and the fused `SESSION_NOT_FOUND` from the
    storage-blip work still leaks nothing: "not found" and "not yours" remain
    byte-identical.

  Fixed at the single `may_read_conversation` chokepoint, so `send_message`,
  `get_session`, `get_conversation_messages`, `confirm_tool_action`,
  `submit_interaction`, `verify_otp` and conversation resume all change together.

  `rust/smooth-operator-server/tests/user_scoping.rs` gains the create-**with**-
  `userEmail`-then-send round trip, which is what every existing test missed: they
  exercised capture and ownership separately and never the round trip a real
  visitor makes. It asserts the session row exists after the create (the failure
  was always an authorization denial, never a failed create), then that the send
  reaches the turn. Four negative controls ride with it: `SESSION_NOT_FOUND` is
  still producible for that same caller on an unknown id, an authenticated
  emailless principal still cannot reach an owned session, another authenticated
  user still cannot reach the visitor's session or see it in a list, and an
  anonymous connection still cannot enumerate authenticated users' conversations.

  No API change.

## 1.58.3

### Patch Changes

- 330680e: fix(server): a storage blip is no longer reported as `session '<id>' not found`

  `AppState::load_session` hydrates a session from storage when the local
  per-pod registry misses (th-ca579c) — the normal path for a returning visitor
  whose WebSocket lands on a pod that has never seen their session. That read
  collapsed `Err` into `None`, so a transient Postgres failure was
  indistinguishable from a session that genuinely does not exist. Every caller
  renders `None` as `session '<id>' not found`, so a backend hiccup told a live
  visitor on smoo.ai, in the chat bubble, that their conversation was gone. Seen
  in production.

  `load_session` now returns `anyhow::Result<Option<Session>>` and
  `handler::scoped_session` propagates it. The three outcomes are distinct at the
  user-visible boundary:

  - `Ok(Some(session))` — unchanged.
  - `Ok(None)` — not found, **or** not yours: still the identical
    `SESSION_NOT_FOUND` / `NO_PENDING_*` event, byte for byte, so there is no
    existence oracle to enumerate other users' session ids with.
  - `Err(_)` — storage could not answer: a retryable `STORAGE_ERROR` ("session
    lookup is temporarily unavailable, please try again"), which is not an
    existence claim and leaks nothing (a storage failure is independent of
    whether the id is real or ours). The underlying error is logged server-side,
    not sent to the client.

  All six session-id-taking actions route through the one chokepoint and switch
  together: `get_session`, `get_conversation_messages`, `send_message` and
  `verify_otp` (previously `SESSION_NOT_FOUND`), plus `confirm_tool_action` and
  `submit_interaction` (previously `NO_PENDING_CONFIRMATION` /
  `NO_PENDING_INTERACTION`, which for a parked turn was equally wrong — the park
  is still there, and a retry still resolves it).

  `rust/smooth-operator-server/tests/session_storage_blip.rs` drives the real
  dispatcher against a storage adapter whose `get_session` fails on demand and
  pins both halves: a blip is never rendered as not-found on any of the six
  actions, and a genuinely unknown id still is (a fix that made everything
  retryable would leave clients retrying an id that will never resolve).

  **Host-facing API change**: `AppState::load_session` returns
  `anyhow::Result<Option<Session>>` instead of `Option<Session>` — hosts calling
  it directly add a `?` or a `match`.

## 1.58.2

### Patch Changes

- f62ee95: fix(ingestion): the chunking contract (G2) — and the four ways the chunker was wrong

  Gap **G2** was recorded as open ("our knowledge store assumes pre-chunked text"),
  but the `Chunker` the connectors feed shipped with G1. What was actually missing
  was the **contract suite** the gap doc asked for — and writing it found the
  chunker wrong on four counts. Every one of them fails _silently_: no error, no
  panic, just worse retrieval or a chunk the embedding API quietly truncates.

  `rust/ingestion/tests/chunking.rs` (16 tests) pins chunk count / dense indices /
  stable ids, overlap as a shared run of words, metadata + title + source + acl
  propagation onto every chunk, oversized spill with no word lost, UTF-8 integrity
  at the boundary, and degenerate configs. Fixed against it:

  - **`max_chars` is a hard cap on the emitted chunk, overlap included.** Overlap
    was prepended on top of a chunk that already filled the cap, so a default
    500/64 chunker emitted chunks of up to 565 characters. The cap is the contract
    with the embedding model's input limit — over it, the API truncates rather
    than rejects, so the tail of a chunk is dropped from the index with nothing
    logged. Overlap now comes out of the packing budget.
  - **Text without spaces now spills.** Splitting on word boundaries only meant a
    Chinese, Japanese or Thai document — or a long URL, or a minified blob — had
    exactly one "word", so it came back as a **single unbounded chunk**. A 50k-char
    document became one chunk, embedded truncated, retrievable as nothing useful.
    The fallback cuts on **character** boundaries.
  - **CRLF documents split on paragraphs.** `"\r\n\r\n"` contains no `"\n\n"`, so
    every Windows-authored file and many HTTP-fetched pages arrived as one giant
    paragraph and lost all paragraph structure. Content is CRLF-normalized first.
  - **A markdown heading is a hard chunk boundary.** A chunk spanning two sections
    attributes section A's text to section B's heading at retrieval time.

  Characters, never bytes, throughout: slicing to a byte offset to hit a character
  cap cuts an em-dash or an emoji in half — a panic in Rust, silent mojibake in a
  port to the other four languages.

  Proof of red: reverting `chunker.rs` in place, leaving the tests, fails 6 of 16.
  The UTF-8 guard needed a second pass and is worth calling out — built from
  space-separated words it passed against a deliberately byte-slicing
  implementation, because spaced text never reaches the character-split path at
  all. Rebuilt on unspaced mixed-width text it panics on that same mutant. A guard
  that cannot fail is the defect, not the reassurance.

  Rust only. Chunking runs server-side in the ingestion crate; the other four
  languages have no ingestion pipeline to port it into, so there is nothing to
  port yet. No public API change — `Chunker::new` / `chunk` keep their signatures.

## 1.58.1

### Patch Changes

- 2fc47ab: Add a deterministic search-quality regression suite and formalize the judged evals into a scored regression layer (feature gap G4).

  **The half that gates CI.** `rust/evals` now ships a retrieval-quality eval that needs no LLM, no key, and no network: a frozen 20-document corpus is seeded through the real ingest→chunk→embed→store pipeline, a frozen 20-query labeled set runs through the real `knowledge_search` tool, and the ranked results are scored with recall@3, recall@5, and MRR against hand-written thresholds. It is deliberately ungated — no `SMOOTH_AGENT_E2E`, no feature flag, no `#[ignore]` — so it runs on every PR and catches a chunker change, an embedder swap, or a rerank bug the day it lands.

  Four permanent degradation tests prove the suite can actually go red: half the corpus dropped, 48-char chunking, first-paragraph-only extraction, and a reranker with its comparator reversed each breach the thresholds the gate enforces.

  **The judged half.** Every eval scenario now declares a typed `Competency` (grounding, anti-hallucination, tool use, multi-turn reasoning, safety, tone), and a new `regression` suite rolls all 15 scenarios up into a per-competency `Scorecard` with its own floor — so a drop in grounding no longer averages away against a rise in tone. `SMOOTH_AGENT_EVAL_MODEL` lets the agent model be swept, and `SMOOTH_AGENT_EVALS_REQUIRED=1` turns "skipped for want of credentials" into a hard failure.

  **Nightly CI.** `.github/workflows/nightly-evals.yml` runs the judged suite across a model matrix, appends each night's scorecard to a cached score history, and renders the trend into the job summary. It fails loudly when the gateway key is missing rather than reporting a green no-op, and nothing in it parses a test log.

## 1.58.0

### Minor Changes

- 88598c3: fix(security): enforce tenant isolation on the by-id session paths and the knowledge store (feature gap G7)

  Closes G7 with a **shared** conformance suite — `rust/adapters/multitenancy_suite.rs`,
  one body run by the in-memory, Postgres and DynamoDB adapters — plus a
  server-level suite driving the real `handle_frame` from an attacker in another
  org. Writing it found two live cross-tenant holes.

  **1. Cross-tenant session access on every by-id path (WS server + Lambda).**
  The connection's org was resolved only to _stamp_ newly created sessions. Every
  by-id action — `get_session`, `get_conversation_messages`, `send_message`,
  `confirm_tool_action`, `submit_interaction`, `verify_otp`, `rename_conversation`,
  and conversation resume — went through `may_read_conversation`, which checks the
  **owner email** and never the org. Its deliberate ownerless-is-open rule (a
  conversation with no `user` participant carrying an email stays readable, so
  anonymous principals keep their own sessions) is exactly the embeddable widget's
  default state, so an attacker authenticated to org B who learned an org-A session
  id could read that session, replay its whole history through a turn, retitle its
  conversation, and resume it (minting a session bound to the victim's org, which
  then flows into the turn's `ToolProviderContext`). The Lambda transport had **no**
  check at all — `dispatch::get_session` / `send_message` acted on whatever
  `storage.get_session` returned.

  Fixed at the chokepoints: `scoped_session` and `may_read_conversation` now take
  the connection's `auth_org` and refuse a row belonging to another tenant
  (indistinguishably from not-found), and the Lambda gained the same check off the
  frame's verified principal. A connection with **no** verified org (anonymous /
  tokenless — the widget's normal state) is unchanged.

  **2. Knowledge was not tenant-isolated on the in-memory adapter, and the admin
  connector-index path ingested org-blind.** `AclKnowledgeStore` filtered by
  user/group only, on the assumption that the wrapped store was already
  org-partitioned — true for Postgres/DynamoDB, false for the in-memory adapter and
  for any third-party adapter using the `knowledge_for_access` trait default. And
  `POST /admin/connectors/{id}/index` ingested through the org-blind `knowledge()`
  handle for every tenant: Postgres wrote `organization_id = NULL` (which the
  org-filtered read can never match, so connector-ingested knowledge silently
  returned nothing) and DynamoDB wrote whichever partition the adapter was
  constructed for.

  - `AclKnowledgeStore` now records each document's owning org (from the
    `org_id` metadata the ingestion pipeline stamps, falling back to the org the
    ingesting handle is bound to) and enforces the tenant boundary **before** the
    ACL.
  - `DynamoKnowledgeBase` honours `AccessContext::organization_id` for the query
    partition and the document's own `org_id` for the ingest partition, mirroring
    what `PgKnowledgeBase::with_access` already did.
  - `PgKnowledgeBase::ingest` prefers the document's `org_id` over the handle's, so
    the org-blind handle still lands rows in the right tenant.
  - The admin index run ingests through `knowledge_for_access`.

  **Behavior change worth reading before upgrading.** A retrieval whose
  `AccessContext` carries an org now sees **only** documents recorded as that org's
  — matching the Postgres backend's existing SQL pre-filter, so all three backends
  finally agree. A document ingested through the raw `knowledge()` handle with no
  `org_id` metadata belongs to no tenant and is therefore invisible to a turn that
  has one. If you seed knowledge directly, either stamp `org_id` on the document or
  ingest through `storage.knowledge_for_access(&AccessContext::default().with_organization_id(org))`
  — which is what the reference server's seeding and the admin index path do.

## 1.57.2

### Patch Changes

- 72b35b6: fix: make `cancelled` terminal on the servers, and bound a .NET turn on the client

  Two terminal states that were not terminal.

  **`cancelled` was advisory, not terminal (th-8628bf).** Cancellation is cooperative
  everywhere: `JoinHandle::abort()`, `context` cancellation and `CancellationToken` all
  take effect at the next yield point, not immediately. A turn that is executing rather
  than suspended keeps running — and an `await` on an already-completed future does not
  yield either — so work kept happening after the client had been sent the terminal
  `cancelled` (499).

  - **Python** was the worst: the Rich Interaction raise tool and the SEP extension host's
    `ui/confirm` park both caught `asyncio.CancelledError` alongside their own timeout and
    returned normally. asyncio only treats a task as cancelled if the `CancelledError`
    propagates out of it, so that _un-cancelled_ the turn: it resumed, ran the next model
    call, persisted an assistant reply and emitted an `eventual_response` for a requestId
    the client had been told ended at 499. Both sites now re-raise; their own
    `TimeoutError` — a genuinely different thing — still degrades to "no answer".
  - **Rust** now drops post-cancel frames at the connection writer, the one point every
    frame passes through. Per-emit-site checks could not do this: frames leave a turn from
    the runner's stream loop, from inside a raise tool, from the write-confirmation gate
    and from the turn tail. The spawned turn also carries a cancelled flag, raised before
    the abort and before `cancelled` goes out, that its tail re-reads immediately before
    each side effect — including the OTP dispatch, which is not a frame and so cannot be
    covered by the writer gate.
  - **Go** gates the turn's sink once, for the same reason: a raise tool calls `sink()`
    straight from inside `Execute`, on the engine's goroutine, with no context check of its
    own. `offerOtp` also moved from the connection's context to the turn's, and re-checks
    immediately before dispatching — a host `OtpService` is under no obligation to honor
    the context, and this is a real code to a real person. The outbound persist re-checks
    too, because a store may ignore the context (the in-memory one takes it as `_`).

  **A .NET turn had no timeout (th-10ff63).** `SmoothAgentClientOptions` exposed only
  `RequestTimeout`, with no counterpart to TypeScript's `turnTimeout`, Go's
  `DefaultTurnTimeout` or Python's `turn_timeout`. A turn the server accepted but never
  terminated hung for the life of the process — no error, no diagnostic, and a leaked entry
  in the client's turn table. `TurnTimeout` now defaults to the same 120s and faults the
  turn with a `TurnTimeoutException`; `Timeout.InfiniteTimeSpan` disables it.

  The same file's `_ = _transport.SendAsync(...)` sat inside a try/catch that could never
  run: `SendAsync` is async, so a send failure faults the returned task instead of throwing
  at the call site, and the discarded task was never observed. The turn was therefore never
  aborted and leaked with no error. It is now awaited on a helper that aborts the turn.

## 1.57.1

### Patch Changes

- 3f25540: test: fence the ingest→ACL chain at the pipeline seam, not just the GitHub connector

  The guarantee that a connector's document ACL survives ingestion — `RawDocument::acl`
  → chunk → structured `DocAcl` → `AclKnowledgeStore` side table → `AclReader` — was
  asserted end to end in exactly one place: `github_connector.rs`. That test is real, but
  it is a _connector_ test. Delete or rewrite the GitHub connector and the ingest half of
  G3 loses its only fence, silently, with the ingestion contract test still green.

  `ingestion_contract.rs::ingested_acls_gate_retrieval_for_every_connector` asserts the
  same chain at the pipeline seam over a `MockConnector`, so it holds for every connector
  present and future: a document ingested for `group-eng` is readable by a principal
  carrying that group and returns **nothing** for `group-fin` or for anonymous, while a
  document ingested with no ACL stays org-public. Each negative assertion is paired with
  the entitled-principal positive control on the same query, so a pipeline that stored
  nothing cannot satisfy "nothing leaked" vacuously — the failure mode this repo has
  shipped before.

  Verified red before green: with `DocAcl::for_groups(...).attach_to(document)` reverted
  in `pipeline.rs`, the new test fails with `group-fin must not read the group-eng doc,
got 1 hits` — the exact G3 cross-user leak — while the pre-existing contract test stays
  green, which is what made the gap invisible.

  No production behavior changes. `docs/Planning/Feature Gaps.md` §G1/§G2/§G9 are updated
  to record what actually shipped (the `Connector` seam, `MockConnector`, and the file /
  web / github connectors landed some time ago and were never marked), the `pull` →
  `Vec<RawDocument>` deviation from the planned `Stream<Document>` and why it should be
  re-shaped before the SaaS connectors rather than after, and what remains: the connector
  long tail, format extraction, and the nightly job that would actually run the gated
  `external` tier.

## 1.57.0

### Minor Changes

- 396a3b6: The session registry is no longer per-pod: `AppState` hydrates a session from
  storage on a local miss, and `otpVerified` is persisted rather than kept in one
  pod's memory (th-ca579c).

  A visitor on smoo.ai asked the agent "do you do websites?" and got
  `Error: session '<uuid>' not found` in the chat bubble. The widget's
  returning-visitor resume POSTs `/internal/resume-by-fingerprint`, which primes
  the session on whichever pod served that HTTP request, then opens a WebSocket —
  which the load balancer sends to an arbitrary pod. The registry was
  `Arc<RwLock<HashMap<String, Session>>>`, so the second pod had never heard of the
  session and `scoped_session` reported it missing. With 2 replicas that is roughly
  half of returning visitors; the HPA goes to 6.

  `AppState::load_session` now falls back to `StorageAdapter::get_session` and
  primes the local map, so any pod can serve any session and the map is a cache
  rather than the source of truth. It is called from `scoped_session` — the
  ownership check every session-bearing frame already passes through — which keeps
  the synchronous readers (`session_authenticated`, `session_contact`,
  `session_supports`) working untouched. A storage error is logged and returns
  `None` rather than being reported as "no such session": a blip must not become an
  existence claim that a human reads as an error.

  `SessionUpdate` gains `metadata`, and the in-memory / Postgres / DynamoDB
  adapters honour it. Without that field there was no way to write session metadata
  back through the adapter at all, which is why `otpVerified` was memory-only:
  a caller who completed OTP on one pod was silently unverified on the next frame
  if the load balancer moved them, and after every roll — while the gate itself
  worked exactly as designed.

  **Why session metadata and not conversation metadata.** The workflow step pointer
  (th-c12df5) and `supports` (th-13df6d) both moved to conversation metadata for
  this same durability reason, so the precedent points there. `otpVerified` is
  deliberately different: it is an authentication result, and conversation scope
  would let any later session in the conversation inherit it. The consuming host
  must also clear it on any resume that recognises a BROWSER rather than a person —
  smooai's fingerprint resume has a 30-day TTL, and a fingerprint is a recognition
  hint, never a credential.

  The write is local-first then through to storage, and a storage failure is logged
  rather than raised: the turn in flight has already verified the human, and
  failing there would refuse service to someone who just proved who they are. The
  cost is that the verification may not survive a pod hop — exactly the pre-fix
  behaviour, so a degradation rather than a regression.

  Still per-pod, and correctly so: `pending_confirmations` and
  `pending_interactions` hold channel senders into a turn parked in that process.
  Those cannot move to shared storage — the parked turn is the state — and making
  them survive a pod switch is durable execution, not a shared map.

## 1.56.7

### Patch Changes

- 2aee08a: fix(all five): a reconnect no longer silently turns Rich Interactions off

  `supports` — the client render-capability list that gates the **entire** Rich
  Interactions framework — was kept somewhere that a reconnect destroys, in every
  implementation.

  A **reconnect is a resume**: the client re-opens the socket and re-issues
  `create_conversation_session` with the same `conversationId`, which mints a
  **new session id** on a **new dispatcher**. So unless the client re-declared
  `supports` on every single reconnect, the server forgot the client could render
  cards at all and every interaction kind quietly fell back to conversational
  collection — no error, no event, nothing on the wire to notice. The parked-card
  flow (raise tool → `interaction_required` → `submit_interaction` → resume)
  simply stopped happening. Reconnects are routine (network blips, mobile
  backgrounding, deploys), so a shipped feature was degrading in the field with no
  signal.

  - **Rust** kept it in `Session.metadata.supports` — the per-pod session registry.
  - **Go** (`FrameDispatcher.supports`), **Python** (`_session_supports`) and
    **.NET** (`_sessionSupports`) kept a per-connection map, also never pruned.
  - **TypeScript** already stored it through the `SessionStore`, but on the
    **session** record, so a resumed session started empty just the same.

  The session was already the wrong home, and the repo had said so once: th-c12df5
  moved the workflow step pointer off it for exactly this reason ("this per-pod
  session map resets on reconnect/pod hop"). `supports` now lives on the
  **conversation** in all five, mirroring whatever conversation-scoped mechanism
  each store already had — Rust/Go/Python/TypeScript write `clientSupports` into
  `conversations.metadata_json`; .NET follows its own store's documented hold for
  `currentStepId`/`otpVerified` (session-row metadata) under the same key name.

  A list the frame **does** declare always wins, including `[]` — which is now how
  a text-only channel resuming a rich conversation opts out, and the opt-out is
  durable so the next reconnect that omits the key cannot resurrect the old
  capabilities. Because `[]` and an absent key now mean different things, each port
  had to stop collapsing them (`*[]string` in Go, `IReadOnlyList<string>?` in .NET,
  `undefined` vs `[]` across the TS store interface).

  The rule lives in the `supports` description in
  `spec/actions/create-conversation-session.schema.json` — the source of truth —
  and the TS/Go/Python/.NET wire types are regenerated from it rather than
  restating it by hand. Each implementation adds a reconnect test that drives a
  **second, fresh dispatcher over the same store**, and each was verified to fail
  against its own pre-fix code.

## 1.56.6

### Patch Changes

- 36d321b: go/typescript-server: bump `smooth-operator-core` to 1.13.2 and retire the last `knownDivergences` marker.

  Core 1.13.2 (th-6fdd1c) fixes the mock LLM provider's streamed text chunker, which split on **byte** boundaries in Go and **UTF-16 code unit** boundaries in TypeScript — cutting multi-byte characters in half and turning each fragment into `U+FFFD` once serialized.

  That was the one defect still keeping the shared conformance corpus from full parity: `interaction-choices-park-resume` streams `"Pro it is — pulling that quote up."`, whose 36 bytes put a 3-way chunk boundary in the middle of the em-dash, so the Go server accumulated `"Pro it is ��� pulling that quote up."`.

  With the bump, **`spec/conformance/scenarios/` now carries no `knownDivergences` marker at all** — all 18 scenarios pass on all five servers (Rust, Go, TypeScript, Python, .NET). The marker did exactly what it was designed to do: it was re-pointed at the real remaining defect rather than deleted, and CI expired it automatically the moment that defect was fixed, failing with `remove go from knownDivergences … it now passes`.

  Go moves 1.8.9 → 1.13.2 and TypeScript's lockfile 1.8.6 → 1.13.2; Python and .NET were already unaffected by the chunker bug and are untouched.

## 1.56.5

### Patch Changes

- 12dcb43: go/typescript/python/dotnet-server: emit `interaction_required` **before** the raise tool's `toolCall` `stream_chunk`, matching the Rust reference.

  For a Rich Interaction, the Rust server emits the park event first; Go, TypeScript, Python and .NET all emitted the raw `toolCall` chunk first. A client that renders tool calls therefore showed "calling `request_identity_intake`…" before the card it was calling for ever appeared — framework internals leaking ahead of the semantic event.

  Ruled a port bug rather than a protocol variant, on the ports' own evidence: all five already defer the gated tool's chunk until after the prompt for the **other** park type (`hitl-write-confirmation`, which all five have always passed). The four were internally inconsistent between their two park paths while Rust was consistent across both.

  The fix reuses each port's existing write-confirmation mechanism rather than inventing a second one: suppress the chunk in the engine stream loop on a tool-name predicate, then re-emit it from the park path immediately after the park event. Go already deferred interaction raises but re-emitted at the top of the raise tool, ahead of the park — that emit moved. TypeScript, Python and .NET gained the predicate (`isInteractionRaise` / `_is_interaction_raise` / `IsInteractionRaise`) matched against the hosted kinds' `request_<kind>` names — deliberately **not** the generic `submit_interaction` tool, whose chunk has no park event to follow and would simply be dropped. Every non-park exit of the raise tool (parse error, conversational fallback) emits the chunk immediately, so `interaction-conversational-fallback` is unaffected.

  Verified against the shared conformance corpus, which pinned this order and marked the four as known divergences: `interaction-park-resume`, `interaction-declined`, `interaction-invalid-retryable` and `interaction-stale-id-rejected` now pass on all five servers, and their `knownDivergences` markers are removed. Rust is unchanged.

  `interaction-choices-park-resume` keeps a `["go"]` marker, re-pointed at a different bug that was only reachable once the ordering was fixed: `splitIntoChunks` in `smooth-operator-core` Go slices the mock reply by bytes, so the em-dash in that scenario's reply is split mid-rune and arrives as U+FFFD. It needs a one-line upstream fix (slice `[]rune`) and a core release.

## 1.56.4

### Patch Changes

- cd6d3da: dotnet-server: stop three client-reachable inputs from killing a connection, faulting a turn, or pinning a turn slot forever.

  **A malformed frame killed the WebSocket.** `FrameDispatcher.DispatchAsync` read `action` and `requestId` with `JsonNode.GetValue<string>()` _outside_ its `try`, and that method throws `InvalidOperationException` on a type mismatch ("An element of type 'Number' cannot be converted to a 'System.String'"). It is neither `OperationCanceledException` nor `WebSocketException`, so it escaped both catches in the connection pump and propagated out of the endpoint delegate: a client sending `{"action":123}` dropped the socket with no error event and no diagnostic. The reads now yield `null` for missing OR wrong-typed, mirroring the Rust reference's `as_str()` → `None` → a clean `VALIDATION_ERROR`, and honouring the stated contract that "protocol-level failures are surfaced as `error` events, never as hard errors that drop the connection". The same reader replaces every other unguarded `GetValue<T>()` over untrusted JSON (frame handlers, interaction kinds, model-produced tool args, agent config) — those were inside the guard, so they merely degraded a `VALIDATION_ERROR` into a generic `INTERNAL_ERROR`.

  **`submit_interaction` with a null inside `values` faulted the turn and left it parked.** An explicit JSON `null` _sets_ a `[JsonPropertyName]`-annotated `List<T>`/`string` property rather than leaving its `= new()` initializer alone, so `{"values":{"answers":null}}` — and `answers[].header` / `answers[].options` — dereferenced null inside `ChoicesKind.Validate`, outside its `catch (JsonException)`. That reached the dispatcher's generic handler as a terminal `INTERNAL_ERROR` with no `Resolve`, so the turn stayed parked for the full 300s timeout and the client was never told what to fix. The choice DTOs now absorb an explicit null at the property (fixing every reader, not just the one path), and both `Validate` call sites run through a guard that turns any unexpected validator exception into a **retryable** `interaction_invalid` — the contract Rust's `choices.rs` already kept, now held for kinds added later too.

  **A parked write-confirmation had no timeout.** The gate awaited a bare `TaskCompletionSource<bool>`, and `ConfirmationRegistry` never times out. A client that closes the tab without closing the socket never triggers teardown, so `RejectAll` never runs and the parked turn holds the connection's single turn slot indefinitely: every later `send_message` returns `TURN_IN_PROGRESS` and the graceful drain hangs. The gate now backstops the park at 300s — the same constant, for the same reason, as the Rust reference's `CONFIRMATION_TIMEOUT` and the interaction park that already had one — and times out **denied**, matching the fail-closed disconnect path. A verdict arriving in the same instant as the deadline still wins.

  Regression tests, each failing without its fix: `MalformedFrameFieldType_ErrorsOrAnswers_WithoutDroppingConnection` (drives a real WebSocket and asserts the socket still serves a ping afterwards), `RichPath_NullValuesShape_IsRetryableInvalid_NotInternalError` (four null shapes, each asserting the park survived and a valid resubmit still resumes the turn), and `UnansweredConfirmation_TimesOut_DeniesTheTool_AndFreesTheTurnSlot` plus its `ConfirmationAnsweredInTime_StillApproves` control. The timeout test injects a short backstop and asserts on settled outcome — turn finished, registration gone, slot accepts a new message — never on elapsed wall-clock.

## 1.56.3

### Patch Changes

- aab1498: ts-server: stop a cancelled turn's late teardown from hanging the turn that replaced it.

  Cancellation in the TypeScript server is cooperative: `cancelActiveTurn()` fires the abort and frees the connection's turn slot synchronously, but the cancelled turn itself runs on until its next stream event — which, if it is sitting in a slow tool, can be a long time. The client is free to start a new turn on that session immediately. When it did, and the new turn parked on a write-confirmation, the cancelled turn's eventual `finally` cleared the registration out from under it: the client's `confirm_tool_action` came back `NO_PENDING_CONFIRMATION` and the new turn **never resumed**. Nothing else settles a confirmation short of a disconnect, so the turn hung for the life of the connection. `interactionPark.clear` had the identical shape; there a parked card silently degraded to `no_response` after the 5-minute interaction timeout instead of hanging outright.

  The registries key on `sessionId`, not on turn identity, so all three teardown clears (the runner's confirmation clear, and the dispatcher's SEP-confirmation + interaction clears) are now turn-scoped, reusing the guard the dispatcher already applied to the active-turn slot itself: the runner skips its clear when its own `cancelSignal` fired, and the dispatcher clears only while it still holds the slot. A cancelled turn has nothing of its own left to drop anyway — `cancelActiveTurn` settles its confirmation and its parked interaction when it fires the abort. Rust never had this: `handle.abort()` drops the turn future, so its `(cfg.clear)` statements never run on the aborted path.

  `ConfirmationRegistry.clear()` and `InteractionParkRegistry.clear()` now **settle** the deferred they drop (rejected / `no_response`) instead of deleting it silently, so any future clear-site can only ever deny an awaiting turn, never strand it. That is the verdict `rejectAll()` already uses and the contract the SEP `ui/confirm` bridge already documented ("the turn ends and it resolves false") without delivering.

  Regression tests in `typescript/server/test/late-teardown.test.ts`, both failing without the fix — one at the `TurnRunner` level whose barrier is the cancelled turn's own rejection (no timing at all), one over a real socket driving the full reported sequence. They differ from the existing `cancel-unpark` test on the two points that matter: turn 1 is parked in a **slow tool** rather than at the confirmation, and turn 2 **registers a confirmation of its own** rather than being a plain text answer.

## 1.56.2

### Patch Changes

- 90eb056: go-server / dotnet-server: make cancelling a turn a **stop** button rather than a **mute** button.

  After a client cancels, the Go and .NET servers walked away from the turn — the runner returns at its first cancellation check and the connection's sink is gagged — but the **agent loop kept running**. The engine folds every tool failure back to the model as a tool result and iterates, and after a cancel that failure is the `context canceled` a tool returns, or the denial the write-confirmation gate returns once `TryCancelActiveTurn` unparks it. So the loop went on to another model call and acted on the answer, with every trace of it discarded. Rust, TypeScript and Python unwind properly; this brings the two ports in line.

  Neither engine's loop has a cancellation check of its own, and cancellation in Go/.NET is cooperative rather than the preemptive future-drop the Rust reference gets for free. The loop is therefore stopped at the one place it re-enters shared state — the model call: the turn's chat client is wrapped so a cancelled context fails the call instead of issuing it, which unwinds `RunStream` / `RunStreamingAsync` and ends the turn. In production the gateway client would have failed that call on its own cancelled context; the servers now stop the turn themselves instead of relying on the transport to do it.

  This also clears the `DATA RACE` the Go race detector reports on the shared conformance corpus's `cancel-mid-turn` scenario, where the cancelled turn's goroutine and the next turn's goroutine drove the engine's mock provider concurrently — independent proof that the cancelled turn was still running.

  Regression tests: `TestCancelledTurnMakesNoFurtherModelCall` (Go) and `ACancelledTurn_MakesNoFurtherModelCall_SoTheNextTurnKeepsItsResponse` (.NET). Both assert the model-call count at a settle point rather than on timing, and both fail without the fix.

## 1.56.1

### Patch Changes

- 26deec3: rust reference: fix two P1 defects in the Rich Interactions park/resume lifecycle — a park that outlived its cancelled turn, and an outcome channel that let one card answer another card's question.

  **A cancelled turn left its park registered (th-6fbab2).** `cancel` and a mid-turn client disconnect both resolve to `handle.abort()`, which drops the turn future at its current `.await` — and a turn parked on an interaction (or a confirmation, or an extension's `ui/confirm`) is sitting on exactly such an await. Every teardown in the runner was written as a statement _after_ that await, so it was skipped precisely when a park was outstanding: the registration in `AppState.pending_interactions` outlived the turn, and the visitor's still-rendered card could be submitted against it indefinitely. Each submit ran the kind's host effect first, so `identity_intake` stamped `userName` / `contactEmail` / `contactPhone` onto the session before the resume discovered nobody was listening — repeatably, with nothing left behind to notice. The runner now arms the teardown as an RAII guard before the turn can park, so it runs on the aborted path too, and `submit_interaction` runs the host effect only after the parked raise was actually resumed. Ports without RAII want `try { … } finally { clear() }`.

  **One turn's raises shared an uncorrelated outcome channel (th-d121f5).** Every `request_*` raise in a turn cloned the same outcome receiver and parked on a bare `recv()` — no interaction id, no kind check. A raise that timed out (the visitor took longer than the 5-minute park) left its card on screen and its registration live; clicking it later queued an outcome that the turn's _next_ raise consumed as its own answer — a different question, and potentially a different kind, so a `choices` selection could resolve an `identity_intake` card. The interaction id is now minted by the raise tool (not the bridge) and carried on both halves of the channel as `InteractionRaise` / `InteractionResolution`; a park consumes only the resolution matching the raise it is waiting on. Anything else belongs to a park that is already dead and is dropped, without extending the waiting park's original deadline.

  API: `InteractionRaise` and `InteractionResolution` are new; `interaction_channel()` and `RegisterInteraction` now carry them in place of bare `InteractionRequest` / `InteractionOutcome`.

## 1.56.0

### Minor Changes

- 0f9e90f: Fix silently-dropped protocol frames in the Go, Python, .NET and TypeScript clients, and add the `submit_interaction` verb to Go/Python/.NET.

  The generated wire types for `stream_preamble`, `stream_reasoning`, `interaction_required` and `interaction_invalid` existed in every language, but the hand-maintained dispatch unions were never updated. Every one of those frames was rejected by each client's own frame guard and then discarded by its dispatch loop — Go's `ParseServerEvent` returned `UnknownEventError` and the read loop `continue`d, Python's `parse_event` raised and `_handle_frame` swallowed it, and .NET's `ServerEventConverter` threw a `JsonException` the client caught and dropped.

  Impact: a session declaring a Rich Interaction (`identity_form`, `choice_chips`) parked a turn that Go/Python/.NET consumers never saw — the turn hung to the turn timeout (and forever on .NET, which has none). `stream_preamble` and `stream_reasoning` are emitted by the production server today, so preamble and reasoning tokens were being discarded by all four clients, TypeScript included.

  - **Go** — added the four event discriminators plus `As*` accessors, `ActionSubmitInteraction`, and `Client.SubmitInteraction`.
  - **Python** — added the four members to `EventType` and the `ServerEvent` union, `submit_interaction` to `ActionType` and `ClientAction`, `SmoothAgentClient.submit_interaction`, and the missing entries in `validate.py`'s schema maps.
  - **.NET** — added `StreamPreambleEvent`, `StreamReasoningEvent`, `InteractionRequiredEvent`, `InteractionInvalidEvent`, wired them into `ServerEventConverter` and `EventTypes.All`, and added `SubmitInteractionAction` + `SmoothAgentClient.SubmitInteractionAsync`.
  - **TypeScript** — added the missing `stream_reasoning` to `EVENT_TYPES`/`ServerEvent` and to `validate.ts`'s event→schema map. (The shipped web-chat example already had a `case 'stream_reasoning'` that could never fire.)

  Each language also gains a drift guard that derives the expected discriminator set from `spec/events/*.schema.json` and `spec/actions/*.schema.json` at test time, so a future event schema that isn't wired into a dispatch union fails the build instead of being dropped at runtime.

## 1.55.3

### Patch Changes

- ec1b9c4: go-server: fix the P1 connection deadlock (and permanent goroutine/socket leak) when a client disconnects mid-turn.

  The per-connection outbound sink is a **bounded** channel (64), and the writer goroutine `return`ed on its first failed `conn.Write`. With no reader left, a streaming turn's 65th `send` blocked forever — and `send` holds `sendMu` across that send, whose only escape (`ioCtx`) is cancelled by `teardown`, which is itself waiting on `WaitForTurns()`. Circular wait: the turn never reached its `ctx.Err()` check even though `CancelTurn()` had already fired, so the connection goroutine and its `s.conns` WaitGroup entry leaked for the life of the process and `Shutdown()` never returned. The read loop and backplane wedged with it on `sendMu`.

  The writer now keeps **draining** (discarding) the sink once the socket is dead instead of returning — matching the Rust reference, whose unbounded `sink_tx` can never block a turn on a dead socket.

  Also from the same review:

  - **Panic containment.** There was no `recover()` anywhere in `go/`, and a turn runs on a bare goroutine — so one panicking host store/config/hook killed the whole process and dropped every other live connection. A panicking turn now settles as a clean `INTERNAL_ERROR` with the connection still usable. (A panic inside a _tool_ is still fatal: the engine runs the tool loop on its own recover-less goroutine in `smooth-operator-core`, so that guard belongs there.) The optional preamble goroutine is guarded too.
  - **Nil-deref crash path.** `TurnRunner.Run` called `stream.Events()` with no nil check, so an `AgentExecutor` returning `(nil, nil)` panicked the turn; it now fails the turn cleanly.

  Regression tests: `TestClientDisconnectMidStreamDoesNotWedgeTurn` (asserts `Shutdown()` actually returns after an RST mid-burst — it hangs without the fix; note `-race` cannot catch a wedged goroutine) and `TestPanickingTurnDoesNotKillTheProcess`.

## 1.55.2

### Patch Changes

- 6120535: dotnet-server: port the `identity_intake` Rich Interaction kind + add the kind-routed host-effect seam.

  The .NET server now hosts a second Rich Interaction kind alongside `choices`:
  `identity_intake` (structured name/email/phone lead capture, capability `identity_form`).
  It mirrors the Rust reference: the `request_identity_intake` raise tool, a server-side
  validator (required-field presence, email shape, phone → E.164 normalization, per-field
  errors reported one-pass), and the conversational fallback directive for text-only channels.

  This wave also adds the one framework piece the `choices` wave omitted: a **kind-agnostic
  host-effect seam**. `IInteractionKind` gains an optional `ApplyEffect` hook (no-op default,
  so `choices` is unaffected); the caller resolves the kind from the DI-provided
  `InteractionCatalog` and runs its effect without knowing which kind it is. It fires on BOTH
  submit paths — the rich `submit_interaction` frame (`FrameDispatcher`) and the generic
  `submit_interaction` tool (the conversational fallback, wired through the turn runner).
  `identity_intake`'s effect stamps the captured, normalized contact onto a session-keyed
  in-memory overlay (`SessionIdentityRegistry`) — the C# analog of the Rust reference's
  in-memory session metadata (`userName` / `contactEmail` / `contactPhone`) — which the OTP
  contact seam now reads alongside the create-session email, so a captured email/phone (phone →
  SMS) becomes OTP-contactable on the next turn.

  Tests: validator unit tests (+ shared-fixture cross-check against the Rust reference) and WS
  park/resume integration tests on both the rich and conversational-fallback paths that assert
  the host effect stamped the session and left it OTP-contactable.

## 1.55.1

### Patch Changes

- f1ff0f9: go-server: port the `identity_intake` Rich Interaction kind + add the kind-routed host-effect seam.

  The Go server now hosts a second Rich Interaction kind alongside `choices`:
  `identity_intake` (structured name/email/phone lead capture, capability `identity_form`).
  It mirrors the Rust reference: the `request_identity_intake` raise tool, a server-side
  validator (required-field presence, email shape, phone → E.164 normalization, per-field
  errors), and the conversational fallback directive for text-only channels.

  This wave also adds the one framework piece the `choices` wave omitted: a **kind-agnostic
  host-effect seam**. A kind may implement the optional `InteractionEffect` interface;
  `attachInteractionEffect` runs it after a valid submit and is a no-op for kinds without one
  (so `choices` is unaffected). It fires on BOTH submit paths — the rich `submit_interaction`
  action (dispatcher) and the generic `submit_interaction` tool (the conversational fallback,
  newly added to the turn runner). `identity_intake`'s effect stamps the captured, normalized
  identity onto the session metadata (`userName` / `contactEmail` / `contactPhone`), the same
  keys the pre-chat create path stashes and the OTP contact seam reads, so a captured email/
  phone becomes OTP-contactable on the next turn.

  Tests: validator unit tests (+ shared-fixture cross-check against the Rust reference) and a
  WS park/resume integration test on both paths that asserts the host effect stamped the
  session and left it OTP-contactable.

## 1.55.0

### Minor Changes

- 2127731: feat(web): `identity_intake` Rich Interaction card (name/email/phone lead capture) for the React SDK + web-chat example

  The `identity_intake` interaction kind (structured name/email/phone lead capture,
  capability `identity_form`) now has a web renderer. `IdentityIntakeCard` (exported
  from `@smooai/smooth-operator/react`) renders one labelled input per `spec.fields`
  in order — `name` → text, `email` → email, `phone` → tel — marking required
  fields, focusing the first input on mount, and honoring a per-field `label`
  override. Submit builds the canonical `{ name?, email?, phone? }` values (only the
  fields the visitor filled in, trimmed) and resumes the parked turn via the existing
  `submitInteraction()` verb; a Decline path sends `declined: true`. Server-side
  `interaction_invalid` errors re-render per field (bad email, missing required,
  phone-normalization message) with the turn still parked and the input flagged
  `aria-invalid` + wired to the error via `aria-describedby`.

  The `interactionCards` registry (`kind` → card) now carries both `choices` and
  `identity_intake`; it moved to its own `components/interactionCards.ts` module so
  each card file owns only its card, and its value type is a loose common
  `InteractionCardProps` so a dynamic `interactionCards[kind]` lookup renders any
  kind with no per-kind code. The web-chat example declares the `identity_form`
  capability alongside `choice_chips` in `create_conversation_session`, and its
  existing overlay-slot lookup renders the card unchanged.

  No protocol/client change — the generic `submit_interaction` verb already speaks
  every kind.

## 1.54.3

### Patch Changes

- 29f1d87: Make Go, Python, TypeScript and .NET tool spans self-identifying, and record cost on all four.

  All four engines omitted `gen_ai.system` from the tool span. OTLP consumers that gate on that attribute — SmooAI's does — therefore discarded every tool span those engines ever emitted, exactly as the Rust engine did. Each now carries its own `gen_ai.system`, `gen_ai.operation.name` (literally `chat` / `tool`, taken verbatim by ingest), `gen_ai.conversation.id`, and `smooai.org_id` where the engine has one. Child spans need their own copies: an ingest merges resource attributes with _that span's_ attributes and does not inherit from the parent.

  Cost reaches the span for the first time in all four: `gen_ai.usage.cost_usd` when positive, otherwise `smooai.gen_ai.cost_unavailable` = `"unpriced"`. A gateway zero means the model is unpriced, not free, so it is never recorded as a cost.

  Two fabricated values found and removed rather than ported:

  - .NET returned a literal `TurnUsage(0, …)` on the cost fallback path, under an XML doc note reading _"0 means 'nothing priced it', not 'free'"_ — the comment was correct and the code shipped the zero anyway.
  - .NET published `input_tokens = 0` because it guarded on `sawUsage` alone; a usage chunk with null counts sets that flag while both totals collapse to `0`. The other three engines guard on the counts themselves.

  Each engine has a test that fails without its change, verified by reverting and restoring.

## 1.54.2

### Patch Changes

- 3da620b: Emit usage provenance, cost provenance, and the gateway response id on the turn span.

  Closes a hole in the previous release. That change gated `gen_ai.usage.input_tokens` on `prompt_tokens > 0`, which worked only against the _streaming_ estimator — the one that hardcodes prompt tokens to zero. Core has a **second** estimator on the non-streaming path that derives prompt tokens from the request JSON length, so it produces a plausible non-zero count. The old gate published that invented number as a measurement; a test restoring it prints the invented `372`.

  Now reads `AgentEvent::Completed.usage_estimated` (core 1.10) instead of inferring provenance from the value. A flag carries the fact; a heuristic guesses at it, and this is the second bug today caused by inferring provenance from a plausible-looking number.

  Adds:

  - **`gen_ai.usage.cost_source`** — `gateway` or `estimated`, set alongside `cost_usd`. Without it a billable figure is indistinguishable from a guess against a local price table, which matters on a metered SKU.
  - **`gen_ai.response.id`** — the gateway's `chatcmpl-…`, previously discarded at deserialization on all four LLM paths. It joins `LiteLLM_SpendLogs.request_id`, whose row carries the gateway's authoritative dollars _and_ real token counts — so it matters most exactly when the counts on the span are absent.

  Requires core 1.10; the wire protocol is unchanged (the flags are telemetry-only, pinned by a key-count assertion).

## 1.54.1

### Patch Changes

- fdd94bb: Port the Rich Interactions runtime + the `choices` kind (AskUserQuestion) to the .NET (C#) server, at parity with the Rust reference.

  The C# server now hosts a kind-agnostic interaction framework (`IInteractionKind` / `InteractionCatalog` / a session-keyed `InteractionParkRegistry` generalizing the write-confirmation park/resume) and the `choices` kind (`request_choices` raise tool, `validate_choices`, conversational fallback, capability id `choice_chips`). A turn on a `choice_chips`-capable session parks emitting `interaction_required` and resumes on a `submit_interaction` frame (invalid values → retryable `interaction_invalid`, never a terminal error); text-only sessions degrade to the enumerated conversational directive and submit via the `submit_interaction` tool. Validated against the shared `spec/interactions/choices.schema.json` conformance fixtures.

- bdcab4e: Port the Rich Interactions runtime + the `choices` kind (AskUserQuestion) to the **Go** LocalServer, mirroring the Rust reference (PR #475) — wave 2 of the polyglot rollout.

  The Go server now hosts a kind-agnostic interaction framework (`InteractionKind` / `InteractionKinds` catalog / a per-connection park-resume `InteractionRegistry`, the analog of the write-confirmation `ConfirmationRegistry`) plus the `choices` kind. Each turn registers one `request_<kind>` raise tool per hosted kind: on a session that declared the kind's render capability (`supports` at `create_conversation_session`) the raise **parks the turn** — the tool blocks awaiting a `submit_interaction` while the server emits `interaction_required` — and on a text-only channel it degrades to the kind's conversational fallback directive. A new `submit_interaction` dispatcher action routes the visitor's values to the kind's server-side validator: invalid → retryable `interaction_invalid` (the turn stays parked), valid → the parked raise resumes with the canonical payload. The `choices` validator (`validateChoices`) enforces the same rules as the Rust reference and validates against the shared `spec/interactions/choices.schema.json` conformance fixtures. Capability id: `choice_chips`.

## 1.54.0

### Minor Changes

- baaf07a: feat(web): `choices` Rich Interaction card (AskUserQuestion) for the React SDK + web-chat example

  The `choices` interaction kind (structured multiple-choice ask, modeled on Claude
  Code's AskUserQuestion) now has a web renderer. `ChoicesCard` (exported from
  `@smooai/smooth-operator/react`) renders each question's `header`, prompt, and
  option chips — radios when `multiSelect` is false, checkboxes when true — plus a
  free-text **"Other"** escape hatch per question that is always available. Submit
  builds the canonical `{ answers: [{ header, options?, other? }] }` values and
  resumes the parked turn via the existing `submitInteraction()` verb; a Decline
  path sends `declined: true`. Server-side `interaction_invalid` errors re-render
  per question with the turn still parked.

  A minimal `interactionCards` registry (`kind` → card) is exported so a client
  looks the card up by kind; `choices` is registered there. The web-chat example
  declares the `choice_chips` capability in `create_conversation_session` and
  renders the card in its overlay slot above the composer.

  Also regenerates `src/generated/types.ts` from `spec/` (adds `ChoicesSpec` /
  `ChoicesValues` / `ChoicesPayload`; picks up the already-merged optional-`agentId`
  and `choice_chips` spec descriptions). No protocol/client change — the generic
  `submit_interaction` verb already speaks every kind.

## 1.53.1

### Patch Changes

- e0077e4: Stop exporting fabricated token counts on the turn span, and adopt the cross-language `cost_unavailable` attribute.

  `gen_ai.usage.input_tokens` was published whenever _either_ count was non-zero. That `||` was the whole bug: when the gateway drops the usage chunk, `smooai-smooth-operator-core`'s `collect_stream` fabricates the struct — `prompt_tokens` hardcoded to `0` and `completion_tokens` estimated as `content.len() / 4` — so the fabricated struct always had a non-zero completion count, and the `||` published `input_tokens = 0` beside it. LiteLLM drops that chunk for `smooth-*` aliases, so this was the common path, not an edge case: no streamed turn has ever exported a measured token count.

  The estimated output count looked plausible precisely _because_ it is derived from the reply text — an estimate computed from the output cannot help but track the output. Only the zeroed input half looked obviously wrong.

  Now gated on `prompt_tokens > 0` alone: both counts are exported, or neither. Absent is honest; `0` is a lie. The underlying fabrication still needs fixing in core.

  Also adds `smooai.gen_ai.cost_unavailable = "unpriced"`, set instead of `gen_ai.usage.cost_usd` when no cost could be established — the same attribute name and value the TypeScript emitters use, so a consumer never has to special-case per engine.

## 1.53.0

### Minor Changes

- 0e8f36e: Add the `choices` Rich Interaction kind (a structured multiple-choice ask, modeled on Claude Code's AskUserQuestion) as the second reference kind in the Rust implementation, plus its shared JSON-Schema contract the other servers + web SDK mirror.

  An agent raises `request_choices` with 1–4 questions, each `{ question, header (short ≤12-char label), options: [{ label, description }] (2–4), multiSelect? }` and a `reason`. On a channel that declared the **`choice_chips`** capability the turn parks and the client renders chips/menus (`interaction_required { kind: "choices" }`); on text/voice channels the same raise degrades to an enumerated conversational directive. Every question carries an implicit free-text `other` escape hatch (mirroring AskUserQuestion's ever-present "Other"), so the visitor can always answer outside the enumerated options.

  Server-side validation (`validate_choices`, shared by the card path's WS handler and the fallback path's `submit_interaction` tool): every question answered, each selected label offered, single-select takes exactly one pick (label XOR `other`), multi-select one or more; invalid submits return retryable per-question `interaction_invalid` errors, never a terminal error. `ChoicesKind` is registered in the default `InteractionRegistry`. The canonical contract lives in `spec/interactions/choices.schema.json` (Spec / Values / Payload) with conformance fixtures; the other-language servers follow as parity work.

## 1.52.3

### Patch Changes

- aa9af9e: Fix multi-tenant org scoping in `handle_send_message`, and make LLM turn spans carry cost and tool spans self-identifying.

  **Org scoping (#470).** `handle_send_message` bound `org_id` twice: once correctly from the conversation (used to resolve the per-org gateway key) and again ~120 lines later as `SEED_ORG_ID`, shadowing it. Everything downstream of the second binding — the org persona override and the host tool provider's scope — saw `reference-org` on a multi-tenant host, for every tenant. Because the gateway key came from the _first_ binding, per-org billing and per-org LLM keys stayed correct, which is why the split went unnoticed. Now reuses the derived org and falls back to the seed org only when it is empty, so the single-org reference/local flavor is unchanged.

  **Turn cost (#471).** The gateway's authoritative per-response cost was already parsed by `smooai-smooth-operator-core` and carried to `TurnUsage.cost_usd`, but nothing recorded it on the span, so `gen_ai.usage.cost_usd` was never emitted and consumers showed "cost not measured". The turn span now records it. A non-positive or non-finite value is dropped rather than written as `0` — a gateway `0` means the model is _unpriced_, not free, and recording it as a real cost would silently under-bill.

  **Self-identifying tool spans (#471).** Tool spans carried neither `gen_ai.system` nor `gen_ai.operation.name`, and OTLP consumers that gate on `gen_ai.system` therefore discarded every tool span ever emitted. They now carry `gen_ai.system`, `gen_ai.operation.name`, `gen_ai.conversation.id` and `smooai.org_id`, so a tool span is both ingestable and joinable to its conversation. Fixed at both emission sites — `runner::run_streaming_turn` and `KnowledgeChatRuntime::run_turn` — which had the identical gap.

## 1.52.2

### Patch Changes

- a992a84: feat(dotnet): client-initiated turn cancellation — SmoothAgentClient.Cancel() + MessageTurn.Cancel()

  The wire protocol and all five servers already honor the `cancel` frame / `cancelled` event,
  but the .NET client had no way to send it — the only stop was `MessageTurn.Abort()`, a local
  force-close that faults the turn and never hits the wire.

  Add the missing client surface, mirroring the TypeScript SDK (#459):

  - `SmoothAgentClient.Cancel(requestId, sessionId?)` sends the `cancel` frame (fire-and-forget).
  - `MessageTurn.Cancel()` is the ergonomic "stop THIS turn" convenience, carrying the turn's own
    requestId + originating sessionId.
  - A terminal `cancelled` event settles the matching turn as a user-stop: it **resolves** (never
    faults) — the async iterator ends cleanly after yielding the terminal `CancelledEvent`,
    `MessageTurn.Completion` yields `null`, and `MessageTurn.Cancelled` is `true` (with
    `CancelledEvent` carrying status 499). Errors still throw, so a UI can tell a stop from a failure.
  - `CancelAction` / `CancelledEvent` are now first-class in the `ClientAction` / `ServerEvent`
    unions and the `ProtocolValidator` schema maps.
  - Idempotent: a `Cancel` with no active turn, or a `cancelled` with no matching turn, is a
    harmless no-op that does not throw.

  `MessageTurn.Completion` is now `Task<EventualResponseEvent?>` (null on a user-stop). The
  `IChatClient` facade contract is unchanged — its `GetResponseAsync` surfaces a mid-flight native
  cancel as `OperationCanceledException`, the idiomatic MEAI "generation stopped" signal.

## 1.52.1

### Patch Changes

- 888cbef: feat(go): client-initiated turn cancellation — Client.Cancel() + MessageTurn.Cancel()

  The wire protocol and all five servers already honor the `cancel` frame / `cancelled`
  event, but the Go client had no way to send it. Add the missing client surface, mirroring
  the TypeScript reference (#459):

  - `Client.Cancel(CancelParams{RequestID, SessionID?})` sends the `cancel` frame.
  - `MessageTurn.Cancel()` is the ergonomic "stop THIS turn" convenience — it cancels using
    the turn's own requestId + originating sessionId. Fire-and-forget, idempotent.
  - A terminal `cancelled` event settles the matching turn as a user-stop: the event is
    delivered on `Events()`, the channel closes cleanly, `Wait` resolves WITHOUT an error,
    and `MessageTurn.Cancelled()` reports `true` so a UI tells a user-stop apart from a
    failure. Errors still go the error path.
  - `cancel` is now first-class in the `ActionType` set and `cancelled` in the `EventType`
    set (with `ServerEvent.AsCancelled()`), not stringly-typed.
  - Idempotent: a cancel with no active turn, or a `cancelled` with no matching turn, is a
    harmless no-op.

## 1.52.0

### Minor Changes

- f5493e9: Add client-initiated turn cancellation (the "Stop button") to the Python async client SDK, mirroring the TypeScript client.

  - `SmoothAgentClient.cancel(request_id=..., session_id=None)` sends a `cancel` frame for an in-flight `send_message` turn.
  - `MessageTurn.cancel()` is the ergonomic "stop THIS turn" convenience — it cancels using the turn's own `request_id` + originating `session_id`.
  - The terminal `cancelled` event now settles the matching `MessageTurn` as a **user-stop**: the turn _resolves_ (never raises), `await turn` yields the `Cancelled` event, the async iterator ends cleanly, and `turn.cancelled` is `True` so callers can tell a user-stop apart from an error.
  - `CancelRequest` / `Cancelled` are now first-class members of the `ClientAction` / `ServerEvent` unions (and the validator's schema maps).

  Idempotent: cancelling with no active turn — or receiving a `cancelled` with no matching in-flight turn — is a harmless no-op.

## 1.51.1

### Patch Changes

- f89f5a6: fix(dotnet): cancel discards a HITL-parked confirmation so the parked turn drops cleanly

  Cancelling a turn parked at a write-confirmation (HITL) freed the slot and emitted `cancelled`
  correctly, but left the parked `Task` lingering: the park awaits a bare `TaskCompletionSource<bool>`
  from `ConfirmationRegistry.Register` that is NOT linked to the per-turn cancellation token, so
  cancelling the CTS never completed that await. The parked task stayed alive (silently gagged by the
  `Cancelled` flag) until the next `Register`/disconnect evicted its pending confirmation.

  `FrameDispatcher.TryCancelActiveTurn` now discards the cancelled turn's pending confirmation
  (`_confirmations.Resolve(turn.SessionId, approved: false)`) after cancelling the CTS, so the parked
  await unblocks immediately (resolves denied; the result is dropped because the sink is gagged and
  `_turn` is already null). To reach the session id from the cancel path, `ActiveTurn` now carries a
  `SessionId`, stamped where the turn is created in `HandleSendMessageAsync`. Mirrors the Rust
  reference dropping the confirmation future on `handle.abort()`. No behavior change for a non-parked
  cancel or the no-active-turn no-op. An xUnit parity test drives a turn to `write_confirmation_required`,
  cancels it, and asserts `cancelled` is emitted, a later `confirm_tool_action` returns
  `NO_PENDING_CONFIRMATION`, the slot is freed, and no stray events leak from the abandoned turn.

## 1.51.0

### Minor Changes

- 090af14: Add client-initiated turn cancellation (the "Stop button") to the TypeScript client SDK.

  - `SmoothAgentClient.cancel({ requestId, sessionId? })` sends a `cancel` frame for an in-flight `send_message` turn.
  - `MessageTurn.cancel()` is the ergonomic "stop THIS turn" convenience — it cancels using the turn's own `requestId` + `sessionId`.
  - The terminal `cancelled` event now settles the matching `MessageTurn` as a **user-stop**: the turn _resolves_ (never rejects), `await turn` yields the `Cancelled` event, the async iterator ends cleanly, and `turn.cancelled` is `true` so the UI can tell a user-stop apart from an error.
  - `CancelRequest` / `Cancelled` are now first-class members of the `ClientAction` / `ServerEvent` unions (and the validator's schema maps).

  Idempotent: cancelling with no active turn — or receiving a `cancelled` with no matching in-flight turn — is a harmless no-op.

## 1.50.3

### Patch Changes

- 0f1ead2: feat(go): env-gated durable-executor selection seam on the Go server turn path (th-137b91, Q parity)

  The Go server drove every turn by calling `SmoothAgent.RunStream` directly, so there was no single
  place a durable backend (ADR-030) could plug in — parity gap with the Rust server's `turn_executor`.
  `TurnRunner.Run` now drives the turn through the engine's `AgentExecutor` seam:
  `turnExecutor(r.executor, os.Getenv("SMOOTH_AGENT_DURABLE_EXECUTOR")).ExecuteStreaming(...)`.

  The durable backend is DEPENDENCY-INJECTED via the new `TurnRunner.executor` field (nil by default),
  so the server binary keeps no compile-time dependency on any Temporal package. Selection mirrors the
  Rust `durable_requested` opt-in exactly (`1`/`true`/`on`/`yes`, case- and whitespace-insensitive):
  the injected executor is used only when the env opts in AND one was supplied; otherwise the
  zero-infra `InProcessExecutor`, a verbatim delegation to `RunStream`, so a default deployment behaves
  exactly as before. Requesting durable mode with nothing injected warns and falls back rather than
  silently pretending the turn is durable. Unit tests cover the full selection matrix with a fake
  injected executor.

- d8a6d43: feat(python-server): env-gated durable-executor selection seam (th-137b91, Q parity)

  The Python server ran every turn by calling `SmoothAgent.run_stream` directly, so there was no single
  place a durable backend (ADR-030) could be selected — unlike the Rust server's `turn_executor`
  (`runner.rs`). `TurnRunner` now routes each turn through the engine's `AgentExecutor` seam, chosen
  once in `select_turn_executor`: a durable backend is dependency-injected as an opaque `AgentExecutor`
  (so the server keeps no hard dependency on the Temporal package), and it is used only when
  `SMOOTH_AGENT_DURABLE_EXECUTOR` opts in (`1/true/on/yes`). With nothing injected — the default — the
  turn runs on `InProcessExecutor`, a verbatim delegation to `run_stream`, so behavior is unchanged.
  Asking for durable mode with nothing injected warns and falls back rather than silently pretending a
  turn is durable. `durable_requested` is split out for a testable parse. Tests cover the parse table,
  the selection logic, and two real turns driven through a fake injected executor.

## 1.50.2

### Patch Changes

- 6cd5ea4: feat(dotnet): emit `gen_ai.chat` / `gen_ai.tool` OpenTelemetry spans on the turn path (th-873430, M parity)

  The .NET server emitted no OpenTelemetry spans, so its turns were invisible in the observability
  studio next to the Rust server's. This brings it to parity: `TurnRunner.RunAsync` now opens a
  `gen_ai.chat` activity per turn — carrying `gen_ai.system`, `gen_ai.request.model`,
  `gen_ai.conversation.id`, `gen_ai.agent.name`, and, on completion, the `gen_ai.usage.input_tokens` /
  `gen_ai.usage.output_tokens` counts — and each tool call opens a child `gen_ai.tool` activity with
  `gen_ai.tool.name` and the (secret-redacted) `gen_ai.tool.call.arguments`. The attribute keys and
  span names are byte-identical to the Rust `smooth_operator::telemetry` module.

  Spans flow from a named `ActivitySource` ("smooth-operator"); the host wires an OTLP exporter only
  when `OTEL_EXPORTER_OTLP_ENDPOINT` is set (the same env gate as the Rust host's `init_telemetry`),
  so a collector-less run has zero telemetry overhead. `gen_ai.request.model` is read from the same
  `SMOOTH_AGENT_MODEL` → `SMOOAI_MODEL` → `SMOOTH_MODEL` env chain the host resolves the gateway model
  from. An xUnit test mirrors `tests/telemetry.rs` with an in-memory `ActivityListener`.

## 1.50.1

### Patch Changes

- 7dab99d: feat(python): durable Postgres + pgvector knowledge store (polyglot parity item L)

  Adds `PostgresVectorKnowledge` and `PostgresAclKnowledge` to the Python server —
  the sibling of the Rust `PgKnowledgeBase` and the .NET `PostgresKnowledgeBase` /
  `PostgresAclKnowledgeStore`. Documents are embedded (core's offline `HashEmbedder`
  by default) and stored in the shared `knowledge_vectors` table; retrieval ranks by
  pgvector cosine distance and returns core `KnowledgeHit`s. The ACL variant persists
  a `{public, users, groups}` ACL in the `acl` jsonb column and filters by the
  requester's entitlements **in SQL**, so a restricted document is never fetched —
  closing the knowledge/ACL-knowledge parity gap that previously stood at
  Rust + .NET only. Contract tests mirror the .NET suites, running against a real
  `pgvector` container and skipping cleanly when Docker is unavailable.

## 1.50.0

### Minor Changes

- b1802ae: feat(ts-server): emit gen_ai OpenTelemetry spans on the turn/tool path (M parity)

  The TypeScript server emitted no OpenTelemetry spans; the Rust server emits
  `gen_ai.chat` / `gen_ai.tool` spans on the turn/tool path. This brings the TS
  server to parity (polyglot item M, th-873430).

  `TurnRunner.run` now opens a `gen_ai.chat` span per turn carrying the same
  attributes as the Rust runner — `gen_ai.system` (`smooth-operator`),
  `gen_ai.request.model`, `gen_ai.conversation.id`, `gen_ai.agent.name`
  (`smooth-agent-chat`), and `smooai.org_id` (threaded from the session) — records
  `gen_ai.usage.input_tokens` / `gen_ai.usage.output_tokens` from the terminal
  `done` event, and emits a child `gen_ai.tool` span per tool call with the tool
  name and its redacted `gen_ai.tool.call.arguments`.

  New `typescript/server/src/telemetry.ts` holds the GenAI attribute-key constants,
  a `redactToolArguments` scrub (secret-named JSON keys → `[REDACTED]`, length-
  capped) mirroring the Rust `redact_tool_arguments`, and an `initTelemetry()`
  that is env-gated on `OTEL_EXPORTER_OTLP_ENDPOINT` exactly like the Rust
  `init_telemetry` — set ⇒ an OTLP (HTTP/protobuf) exporter is registered, unset ⇒
  no-op spans, no collector needed. `main()` calls it at boot.

  A new `test/telemetry.test.ts` drives a real streaming turn against an in-memory
  span exporter and asserts the `gen_ai.chat` (with org) and child `gen_ai.tool`
  (with args) spans — the TS parity of `smooth-operator-server/tests/telemetry.rs`.

  Green: tsc + 320 server tests.

## 1.49.2

### Patch Changes

- 544cac9: feat(python): emit gen_ai OpenTelemetry spans on the Python server's turn/tool path (M parity)

  The Python server emitted no OpenTelemetry spans while the Rust server emits
  `gen_ai.chat` / `gen_ai.tool` spans on the turn path. `TurnRunner.run` now opens a
  `gen_ai.chat` turn span (`gen_ai.system`, `gen_ai.request.model`,
  `gen_ai.conversation.id`, `gen_ai.agent.name`, `smooai.org_id`, and, on completion,
  `gen_ai.usage.input_tokens` / `output_tokens`) and a child `gen_ai.tool` span per
  tool call (`gen_ai.tool.name` + redacted `gen_ai.tool.call.arguments`), mirroring the
  Rust `run_streaming_turn` span points and attribute names so the observability studio
  groups Python + Rust + TS turns together. A tracer provider is installed at server
  boot, env-gated on `OTEL_EXPORTER_OTLP_ENDPOINT` (unset ⇒ zero external deps; the OTLP
  exporter is the optional `otel` extra).

## 1.49.1

### Patch Changes

- 09a0223: fix(dotnet): reject `create_conversation_session` without an `agentId` (th-68897a)

  Follow-up to the P1 slice. `agentId` is REQUIRED by the Request schema and the generated
  client type is non-optional, so absent-or-blank is a **malformed request**, not an
  agentless session. The old code fabricated a UUID; P1's first pass stopped fabricating
  but silently stored NULL. Both accept a malformed request and differ only in what they
  write down — neither validates a required inbound field.

  Both .NET entry points now reject, reusing the existing error path: the WebSocket
  `create_conversation_session` handler emits `VALIDATION_ERROR "missing 'agentId'"`, and
  `IServerInitiatedTurns.StartTurnAsync` throws `ArgumentException`. Both reject **before
  the store is touched**, so a rejected request persists nothing — a rejection that still
  writes a row is the same bug wearing an error message.

  The nullable column and property from the P1 slice stay: honest for rows predating this
  check, just no longer reachable from the create path.

  Seventeen test frames across eleven files created sessions without an `agentId` and now
  supply one. Two were relying on the absence in a way worth naming: `OtpTests` passed an
  explicitly blank `"agentId":""`, and the extension tool-filter tests leaned on the
  fabricated GUID resolving against a permissive config resolver.

  534 pass.

- 26d783e: fix(go,ts,python): reject a create_conversation_session with no agentId

  Mirrors core#434 (`bfd3911`). `agentId` is `required` in
  `create-conversation-session.schema.json` and the generated client type is
  non-optional, so absent-or-blank is a **malformed request**, not an agentless
  session. The original code fabricated a UUID; th-68897a's first pass stopped
  fabricating but silently stored NULL. Both accept a malformed request and differ
  only in what they write down — neither validates a required inbound field.

  All three now answer `VALIDATION_ERROR` / `Missing 'agentId'` and persist nothing,
  using each server's existing error-emission path.

  **One entry point per server, not two.** Rust needed both its WS handler and its
  Lambda dispatcher; Go, TypeScript and Python have no Lambda — the only request
  boundary is the dispatcher's `create_conversation_session` case. The two-store
  pattern from th-68897a applied to the _fabrication_, which lived in each store;
  _validation_ belongs at the request boundary, and the stores have no request id or
  sink to answer on.

  Everything from th-68897a stays: the column is still nullable, the field still
  optional, Go still uses `""`-as-absent. That remains honest for rows written before
  this check — it is just no longer reachable from the create path.

  **14 pre-existing tests were sending malformed creates** and are corrected here,
  which is the real finding. One of them (`test_send_message_without_gateway_errors_cleanly`)
  didn't fail — it **hung**, waiting forever for an `immediate_response` that is now an
  `error`. The rest span conversation scoping, resume, file transfer, graceful drain,
  skills, tool hooks, turn round-trip and the preamble. None of them ever needed an
  agentless session; they just never had to supply the field.

  Three tests per language: absent, `""` and `"   "` are each rejected **and** nothing
  is persisted — a rejection that still writes a row is the same bug wearing an error
  message — plus one asserting a real agentId still works.

  Green, all exit 0: Go `vet` + `go test`, TypeScript `tsc` + 317 tests, Python ruff +
  323 tests.

- e45cc21: Rust now validates the shared conformance fixtures.

  Go, TypeScript, Python and .NET all check `spec/conformance/fixtures.json` against the schemas
  it declares. Rust did not — which made the reference implementation the one implementation that
  could not catch a spec/code divergence. th-68897a shipped against a stale `required` list and
  Rust noticed nothing; .NET failed on first contact purely because it validates.

## 1.49.0

### Minor Changes

- bfd3911: Reject `create_conversation_session` when `agentId` is absent or blank.

  `agentId` is required by the Request schema and the generated client type is non-optional, so
  an absent or blank one is a malformed request — not an agentless session. The original code
  fabricated a UUID for it; th-68897a's first pass stopped fabricating but silently stored NULL.
  Both skip the validation that belongs at that boundary. It is now a `VALIDATION_ERROR`, in
  both the WebSocket handler and the Lambda dispatcher.

  The column and field stay nullable. That is th-68897a's real win — nothing is ever invented —
  and it remains honest for rows written before this validation existed. It is simply no longer
  reachable from the create path.

### Patch Changes

- 1fa1901: fix(dotnet): stop inventing an agent id for agentless sessions (th-68897a)

  Session creation filled a missing `agentId` with a fresh GUID, so every agentless session
  pointed at an agent that never existed. Nothing failed loudly — the fake id flowed into
  the participant's `internal_id` and the per-agent config lookup and resolved to nothing.

  `StoredSession.AgentId` and `conversation_sessions.agent_id` are now nullable, blank and
  whitespace read as absent, and both stores (in-memory and Postgres) stop fabricating.
  Minting `AgentParticipantId` is untouched — that mints a real participant row, not a
  dangling reference.

  The nullable type surfaced the exact path the bug hid in: `IAgentConfigResolver.ResolveAsync`
  was being handed the fabricated GUID at two call sites. Both now skip the lookup when there
  is no agent, matching the Rust handler.

  **A spec conflict this exposes, raised rather than papered over:** the session descriptor is
  emitted WITHOUT `agentId` when there is no agent, matching Rust's `skip_serializing_if` — but
  `spec/actions/create-conversation-session.schema.json` still lists `agentId` as **required**,
  typed `string`. So an agentless descriptor is not spec-valid in _any_ language after this
  change; null and omission are both invalid. The .NET spec-validity test now names an agent
  (the case the spec actually describes) and a separate test pins the agentless shape.

  Two existing tests were asserting the bug and are inverted: one asserted the store minted an
  agent id, and the extension tool-filter tests were passing no `agentId` and relying on the
  minted GUID resolving against a static config resolver.

- 1aff9aa: fix(go,ts,python): stop inventing an agent id for agentless sessions

  Mirrors core#429 (`7b93496`). Session creation filled a missing `agentId` with a
  fresh UUID, so every agentless session pointed at an agent that never existed.
  Nothing failed loudly — the fabricated id flowed into the backplane agent target
  and the per-agent config lookup and quietly resolved to nothing.

  `agent_id` is now nullable, the session field optional, and blank/whitespace reads
  as absent rather than becoming a literal empty-string agent. Absence is propagated
  instead of papered over: the per-agent config resolver isn't called at all for an
  agentless session, and `agentId` is omitted from the wire rather than sent as
  null/empty.

  **Two fabrication sites per server, not one** — the in-memory store and the
  Postgres store each had it, so the test covers both. Testing one would have left
  the other broken.

  Two things the Rust change did that deliberately have **no** counterpart here,
  verified rather than mirrored blind:

  - The agent participant's `internal_id` — none of these three ever sets it. Their
    participant INSERTs don't name the column, so there was nothing to propagate.
  - `agentParticipantId` stays a fresh UUID. That mints a legitimate participant row;
    only `agentId` was the bug.

  `Checkpoint.agent_id` is a different type and is untouched.

  Go keeps `AgentID string` rather than adding a pointer: `""` is already that struct's
  absent, the store already branched on `agentID == ""`, and blank-is-absent is the
  specified semantics, so there is no state a pointer would distinguish. The column is
  NULL in the database either way, coalesced at the one read site.

  One test per language, against real Postgres containers: a session created with a
  blank agent id reads back absent from **both** stores, the column is NULL rather than
  an empty string standing in for one, and it survives the round trip instead of
  returning a uuid.

  Green, all exit 0: Go `vet` + `go test`, TypeScript `tsc` + 313 tests, Python ruff +
  319 tests.

- 0550e64: Make `agentId` optional in the session schemas.

  th-68897a stopped fabricating an `agentId` when the caller names no agent, but the spec still
  listed it as required — so after that change there was no spec-valid way to describe an
  agentless session: `null` fails the type, omission fails `required`. Every server emitting the
  honest shape was technically out of spec.

  `agentId` is dropped from `required` in `spec/domain/session.schema.json` and
  `create-conversation-session.schema.json#/$defs/Response`. It keeps its type for when present;
  absence is represented by omitting the field, which is already what the Rust and .NET servers
  emit.

## 1.48.0

### Minor Changes

- 7b93496: Stop fabricating an `agent_id` when the caller names no agent.

  Session creation filled a missing `agentId` with a fresh UUID, so every agentless session
  pointed at an agent that had never existed. `Session.agent_id` is now `Option<String>` and the
  `conversation_sessions.agent_id` column is nullable: absent is the honest answer, and a
  fabricated reference is not.

  Both entry points had the same bug — the WebSocket handler and the Lambda dispatcher — and
  both are fixed. A blank or whitespace-only `agentId` now also reads as absent rather than
  becoming a literal empty agent.

  BREAKING for direct Rust consumers of `smooth_operator::domain::Session`: `agent_id` is now
  `Option<String>`.

### Patch Changes

- 459b4b4: Push the Postgres adapter's data integrity into the schema.

  The json columns (`metadata_json`, `analytics_json`, `metadata`) are now `NOT NULL DEFAULT
'{}'`, so "absent" has one representation on read instead of two. `platform` and session
  `status` gain CHECK constraints alongside the ones `direction` and participant `type` already
  had, and the timestamps that were fully nullable on `conversation_sessions` are now
  `NOT NULL DEFAULT now()`.

  A bare DEFAULT was not enough: the inserts pass an explicit NULL for an absent optional, and
  a DEFAULT only fires on an omitted column, so the inserts coalesce in SQL.

  Rust only — the Go, TypeScript, Python and .NET stores keep their own copies of this DDL and
  are being brought across separately against this as the reference.

- b5dd15d: feat(dotnet): let the Postgres schema enforce what the store was only hoping (th-5a5181 P2)

  The .NET slice of the adapter-integrity wave, mirroring the Rust reference (#425):
  `metadata_json` / `analytics_json` / `metadata` become `JSONB NOT NULL DEFAULT '{}'`
  so "absent" has one representation on read instead of two; `platform` and session
  `status` gain CHECK constraints alongside the ones `direction` and participant `type`
  already had (status stays optional — a NULL passes a CHECK); and
  `conversation_sessions.created_at` / `updated_at` / `last_activity_at` become
  `NOT NULL DEFAULT now()` like every other table's.

  Two places this slice differs from the Rust one, both deliberate:

  **No `coalesce` in the INSERTs.** Rust needed it because its inserts pass an explicit
  NULL for an absent `Option`, and a bare `DEFAULT` only fires on an OMITTED column. This
  store omits those columns entirely, so the DEFAULT already fires — a coalesce here would
  be dead SQL. The only json column it writes explicitly is
  `conversation_sessions.metadata`, which is always a serialized object.

  **`platform` was `'smooth-operator'`, which the new CHECK rejects.** That is the product
  name, not a channel, and it is not in the shared platform vocabulary — applying the CHECK
  without fixing it would have failed every conversation insert. Now `'web'`, matching the
  Go store, which is what a browser WebSocket chat is.

  The migration block also closes the constraints on a LEGACY database: `CREATE TABLE IF
NOT EXISTS` is a no-op there, so the DDL's NOT NULLs would never have applied. It
  backfills the nulls then `SET NOT NULL` / `SET DEFAULT`, so a migrated database ends up
  with the same guarantees as a fresh one. This is the only server with a migration block,
  so it is the only one that can. The CHECKs are deliberately NOT retrofitted — a legacy
  row's platform is `'smooth-operator'`, so adding that constraint would fail init on
  exactly the databases that need migrating.

  Tests: a session created with no metadata reads back `{}` rather than null across
  conversations, messages and participants; a migrated legacy database reports its columns
  as NOT NULL; and the CHECKs reject `'smooth-operator'` and an unknown session status.
  70 green against real pgvector containers.

- 0bf93f7: feat(go,ts,python): mirror the P2 schema integrity constraints into the three servers

  Mirrors core#425 (Rust adapter, `459b4b4`) into `go/server/postgres_store.go`,
  `typescript/server/src/postgresStore.ts` and
  `python/.../postgres_store.py` — identical DDL in all three:

  - `metadata_json` / `analytics_json` / `metadata` → `JSONB NOT NULL DEFAULT '{}'::jsonb`
    on conversations, participants, messages and sessions, so "absent" has ONE
    representation on read instead of two.
  - `platform` gains a `CHECK` over the ten known values; `status` gains a `CHECK`
    over `active` / `idle` / `ended`. Status stays optional — NULL passes a CHECK, so
    the value is constrained without the column becoming required.
  - `conversation_sessions.created_at` / `updated_at` / `last_activity_at` →
    `NOT NULL DEFAULT now()`.

  **No `coalesce()` was needed in any of the three, unlike Rust.** The Rust adapter
  names every column in its INSERTs and passes an explicit NULL for an absent
  `Option`, and `DEFAULT` only fires on an _omitted_ column — hence its four
  coalesces. These three servers **omit** the json columns from every
  conversation-domain INSERT, so the DEFAULT fires on its own. The one JSONB they do
  pass explicitly (`conversation_sessions.metadata`) is always a serialized object,
  never NULL, in all three. Verified against real Postgres containers rather than
  assumed.

  Also skipped, per the Rust PR: the `(organization_id, browser_fingerprint)` index.
  No server here queries that column.

  Two tests per language, run against real containers: absent json reads back as `{}`
  (the one that fails if either the `NOT NULL DEFAULT` or the omit-from-INSERT
  regresses), and an unknown platform is rejected by the CHECK.

  Green: Go `vet` + full `go test` (14 postgres tests), TypeScript `tsc` + 312 tests,
  Python ruff + 318 tests. All exit 0.

  Note carried over from the Rust PR: the DDL is `CREATE TABLE IF NOT EXISTS`, so
  these constraints apply to newly-created databases. An existing table is unchanged
  until someone writes a migration — same as Rust, and worth a follow-up rather than
  a silent assumption.

## 1.47.1

### Patch Changes

- 5329fa9: Restore `seq` paging in the Postgres adapter, and stop calling it a mirror of the monorepo.

  The previous release dropped `seq` from the Rust adapter to make its schema apply against the
  smooai monorepo database. That goal is closed: the deployed operator persists through a
  separate private adapter over the real tables (ADR-041), so this crate is the OSS operator's
  own STANDALONE store and the two schemas are allowed to differ.

  Dropping `seq` therefore bought nothing and left Rust paging on `(created_at, id)` while the
  Go, TypeScript, Python and .NET stores still paged on `seq`. `seq` is back — a
  database-assigned counter cannot tie, which makes it a stronger paging key — and all five
  implementations agree again.

  The `from`/`to` participant-id columns and their join are kept: de-denormalizing a
  `ParticipantRef` blob that could carry a stale name and type is an integrity win independent
  of which database this points at.

## 1.47.0

### Minor Changes

- 5eee462: Make the Postgres adapter's schema apply against the real smooai database.

  `schema.rs` claimed to mirror the monorepo and did not. `conversation_messages` declared
  `from_ref`/`to_ref` JSONB where the monorepo has `from`/`to` participant FK columns, and a
  `seq BIGSERIAL` the monorepo has never had — so `CREATE INDEX ... (conversation_id, seq)`
  aborted schema init with `column "seq" does not exist`, and every server applying that schema
  failed at boot against the real database.

  The adapter now uses the real column names, stores participant ids rather than a denormalized
  JSON blob (a `ParticipantRef`'s type and name come from joining `conversation_participants`),
  and pages on `(created_at, id)` instead of a `seq` counter — a stable total order that needs no
  extra column. The module doc now lists what still differs instead of claiming parity.

### Patch Changes

- 9d4ccc0: feat(dotnet): associate session/user/org/agent so the backplane actually fans out

  core#414 and core#418 built .NET's backplane up to a target-shaped
  `Publish(Target, event)` with a `target → connections` index, and left the last
  step explicit: only `Target("connection", …)` had entries, so the other four
  kinds resolved to zero connections and `POST /admin/publish` 501'd them. This is
  that step — the associations — so all five targets deliver for real.

  Additive, no reshaping: `Associate(connectionId, target)` moves onto `IBackplane`
  (it existed as a private helper), and `Attach` keeps seeding the connection
  target, so connection delivery is unchanged and needed no special case.

  The lifecycle wiring is what makes it more than an index. `Attach`/`Detach` were
  already in `PumpAsync`; this adds `user`/`org` at connect from the
  **authenticated principal** — never a frame field — and `session`/`agent` as
  sessions resolve. That hook goes on `FrameDispatcher.ScopedSessionAsync`, already
  the security chokepoint every sessionId-bearing action routes through, plus
  session creation, so no handler can work with a session the backplane does not
  know about. `Associate` is idempotent because that chokepoint runs on every
  sessionId-bearing frame.

  `delivered` stays truthful, and the 501s go away rather than being papered over:
  a `session` target with nothing associated now returns a real
  `{"delivered": 0}` — the type IS routable, so 501 would be the lie now. It was
  correct only while a connection-id registry could not resolve it.

  One hazard the fan-out introduces and this fixes at the source: `Publish` handed
  the SAME `JsonObject` to every sink. That was fine at one sink per target; with
  many it lets one connection's sink corrupt every other connection's frame, since
  `JsonObject` is mutable. Each sink now gets its own `DeepClone`, which also makes
  the route's pre-publish clone redundant, so it's gone.

  7 new tests, and core#414's 501 theory is rewritten to assert real delivery for
  all four kinds. The one that earns its keep drives a **real WebSocket** through
  `create_conversation_session`, publishes to that connection's
  session/user/org/agent, and asserts the events **land on the socket** — not that a
  counter moved — then that after close the session is unroutable again. That is the
  test that fails if the association wiring silently never runs.

  Delivery coverage is now Rust, Python and .NET on full fan-out; Go and TypeScript
  remain connection-only with an honest 501, and are next.

- fff7978: refactor(dotnet): make `IBackplane.Publish` target-shaped so the fan-out is additive

  `Publish(string connectionId, event)` became `Publish(Target target, event)`, with
  `Target(Kind, Id)` as a record. `InMemoryBackplane` now resolves a target to a set of
  connections via a `Dictionary<Target, HashSet<string>>`, so `Publish` is **already
  correct for all five target kinds** — the other four simply have no entries yet and
  return 0. Associating a session/user/org/agent with its connections is the cross-pod
  fan-out work, and it plugs in by seeding that index without touching `Publish` again.
  `POST /admin/publish` still 501s the four, which keeps that a route-level statement
  ("not deliverable here") rather than a backplane limitation.

  `Detach` now tears down every association, not just the sink, via a reverse index — a
  leaked association resolves to a dead socket and would inflate `delivered` forever.

  No behavior change: `connection` targets deliver exactly as before, all 518 tests pass
  unchanged.

- d91ee14: feat(dotnet): `GET /admin/model-costs` and `POST /admin/publish`, closing .NET to full admin parity

  The two admin routes that landed for the other engines while the .NET server was
  catching up on the console surface. `model-costs` went to Go, TypeScript and Python;
  `publish` went to Go and TypeScript. .NET had neither, which left it the only engine
  missing `model-costs` and one of two missing `publish`. With these it serves the whole
  shared admin surface.

  **`GET /admin/model-costs`** is ungated, exactly as in Rust: gateway pricing is not
  org-sensitive and cost badges must render on a tokenless local connection. It maps the
  gateway's `/model/info` into `{ "<model>": { inputCostPerToken, outputCostPerToken,
tier, useCases, maxOutputTokens } }` via a new pure `ModelInfo.MapModelInfo`, fetched
  at most once per process. Two details are load-bearing and both are tested: an omitted
  field stays **null rather than defaulted**, because a `0` cost would render a
  free-model badge on a paid model; and only a **success** is cached, because caching a
  failure would pin an empty map for the life of the process and leave every badge
  missing until a restart even after the gateway recovered.

  **`POST /admin/publish`** pushes a realtime event to a target over a new `IBackplane`
  connection registry — the plug point for non-AI publishers (job status, ingestion
  progress, notifications) that need to reach a connected client without going through an
  agent turn. Admin-gated. `connection` targets deliver for real and report a truthful
  `delivered` count of 0 or 1 taken from the sink registry. `session` / `user` / `org` /
  `agent` answer a hard **501 `UNSUPPORTED_TARGET` with no `delivered` field at all**: a
  connection-id registry cannot route them, and `{"delivered": 0}` would let a caller
  read "accepted, reached nobody" as success for an event that was never routable. When
  the cross-pod fan-out lands, each target flips from a 501 to a real count.

  `IBackplane` + `InMemoryBackplane` are the first backplane in `dotnet/` — the other
  engines each had one and .NET did not. Ported from the Go shape and deliberately
  synchronous: every operation is a dictionary access plus a channel write, and the
  TS/Python `attach`/`detach` are async only because those ecosystems default to it. The
  WebSocket host attaches each connection's outbound channel as its sink and detaches on
  teardown; the detach is the half that matters, since a leaked sink would report
  `delivered: 1` into a channel whose socket is long gone, and it is covered by a test
  that drives a real WebSocket and asserts the registry empties.

  Also fixes a latent crash in the existing `ModelInfo.ParseCeilings`, found while
  writing the mapper's malformed-payload tests: indexing a `JsonNode` that is not an
  object **throws**, so a gateway payload carrying a scalar `model_info` (or a scalar
  entry, or a non-string `model_name`) took the parse down instead of reading as "no
  ceiling". It was contained only by `FetchCeilingAsync`'s catch-all; the pure function is
  public and threw. Both parsers now coerce at every level.

  One .NET-specific quirk worth recording: a top-level `JsonObject` handed to
  `Results.Ok` serializes to an **empty body**. It round-trips correctly as a property —
  which is why a connector's `config` was fine and nothing caught this earlier — so
  `model-costs` writes `ToJsonString()` directly. Left as `Results.Ok`, the route would
  have returned a permanently empty `200`, indistinguishable from "the gateway is down".

  Verified by 14 new tests (3 pure mapper cases, 1 sequential route test covering
  ungated + degrade-to-empty + cache-only-success against a real stub gateway, 8 publish
  cases including the four unroutable targets, 1 WebSocket attach/detach lifecycle, plus
  the fail-closed table row) and by exercising all 16 route combos against a booted C#
  host: no 404s, no 5xx.

- b6a7590: feat(go,ts): full 5-target backplane fan-out — all five servers now match

  Closes the inverted parity this workstream surfaced: Rust, Python and .NET
  delivered to `connection` + `session`/`user`/`org`/`agent`, while Go and
  TypeScript were connection-only and answered `501 UNSUPPORTED_TARGET` for the
  other four. That 501 was honest — a connId→sink registry genuinely cannot route a
  session id — but it left the reference ahead of two of its ports.

  Both now carry the reference's fan-out, built the same way in each:

  - `Target{Kind, ID}` — a comparable struct in Go (a map key by value); an
    interface in TypeScript, keyed internally as `kind\0id` because a colon
    separator would collide on ids that legitimately contain one (an org name, an
    email).
  - `Associate(connId, target)` links conn↔target in **both** directions, so
    `Detach` tears every association down rather than leaking one that resolves to
    a closed socket. Idempotent: the session chokepoint runs on every
    sessionId-bearing frame, so a re-association must not double-count.
  - `Publish(target, event)` replaces `Publish(connId, event)`; `Attach` seeds
    `("connection", connId)`, so connection delivery is unchanged and needed no
    special case. TypeScript keeps `publish`/`associate` optional on the interface,
    so a third-party backplane predating them still gets the honest 501 — that one
    really cannot route.

  The lifecycle wiring is the load-bearing half: `user`/`org` at connect from the
  **authenticated principal** — never a frame field — and `session`/`agent` as
  sessions resolve.

  **TypeScript needed a chokepoint it did not have.** Go, Python, Rust and .NET each
  funnel every client-supplied sessionId through one guard (`scopedSession` /
  `_visible_session` / `ScopedSessionAsync`); TypeScript re-derived the ownership
  check at three call sites. All three were byte-identical, so they now route
  through a new `scopedSession`. That is where association lives, for the same
  reason the ownership check belongs there: one place covers every handler. Worth
  noting on its own — a missing funnel is exactly the shape of th-1b7ed0.

  `delivered` stays truthful in both. A `session` target with nothing associated now
  returns a real `{"delivered": 0}`, because the type IS routable — 501 would be the
  lie now. It was correct only while the registry could not resolve it.

  11 new tests, and each server's 501 test is rewritten to assert real delivery for
  all four kinds. The registry tests port Rust's `backplane.rs`, including the
  idempotent-associate case that the hot chokepoint path makes matter.

  Delivery coverage is now identical across all five servers:

  | Server     | connection | session / user / org / agent |
  | ---------- | ---------- | ---------------------------- |
  | Rust       | yes        | yes                          |
  | Python     | yes        | yes                          |
  | .NET       | yes        | yes                          |
  | Go         | yes        | yes                          |
  | TypeScript | yes        | yes                          |

- 38e2051: feat(server): make the turn executor injectable per turn (ADR-030)

  `TurnRequest` gains an optional `executor: Option<Arc<dyn AgentExecutor>>`. `None` —
  every existing caller — runs the turn in-process exactly as before, so this changes
  no behavior.

  Two reasons it is a per-turn field rather than something the runner constructs or
  holds process-globally:

  - **It keeps Temporal out of this crate.** This crate publishes to crates.io, and
    cargo refuses to publish a crate declaring a git or path dependency even behind an
    off-by-default feature. With the executor injected, an unpublished deployment crate
    can build the durable executor and pass it in, and nothing Temporal-shaped ever
    appears in this manifest.
  - **Durable mode is meant to be opted into per conversation**, which a process-global
    handle could not express. A process-global would also repeat the exact mismatch
    that makes the durable backend hard to adopt today — its activity worker holds one
    global registry while this server builds a per-turn, per-org, ACL-scoped one.

  `turn_executor` now takes the injected value: supplied ⇒ used verbatim, and
  `SMOOTH_AGENT_DURABLE_EXECUTOR` is not consulted. Nothing supplied ⇒ the in-process
  executor, with the env var still warning rather than silently pretending a turn is
  durable.

  This is foundation only. It does **not** make a parked write-approval survive a
  browser refresh — there is still no client-side `AgentExecutor` in
  `smooai-smooth-operator-temporal` to inject, and building one is blocked on two open
  design questions: a workflow-backed turn has no token-delta path (so it cannot feed
  the runner's event translator), and `AgentTurnInput` carries neither prior messages
  nor a per-turn tool registry.

## 1.46.8

### Patch Changes

- 67f77c0: feat(python): real backplane fan-out + `POST /admin/publish` (all five targets)

  The Python server could not deliver a realtime event to a connected client at
  all. Its backplane held connection-id **strings** in a set — no sink, so nothing
  to deliver _to_, and no `associate`/`publish` at all — and it had no
  `/admin/publish` route. Non-AI publishers (job status, ingestion progress,
  notifications) had no way to reach a socket without going through an agent turn.

  Ports the Rust reference's `rust/smooth-operator/src/backplane.rs` in full,
  including its **5-target fan-out**:

  - `attach(conn_id, sink)` registers the connection's outbound sink; re-attach
    replaces it, so a reconnect under the same id never leaves a dead socket
    receiving. `Target("connection", conn_id)` is always reachable.
  - `associate(conn_id, target)` records a conn↔target link in both directions, so
    `detach` can tear all of them down.
  - `publish(target, event)` delivers to every connection for that target and
    returns the count of **local** deliveries.

  Wired into the real connection lifecycle, which is the part that makes it more
  than a registry: the sink is attached after it exists (a connection registered
  without one would report a delivery it cannot make), `user`/`org` are associated
  at connect from the authenticated principal, and `session`/`agent` are associated
  as sessions resolve. That hook sits in `_visible_session` — already the single
  funnel every sessionId-bearing action goes through — plus session creation, so no
  handler can work with a session the backplane does not know about.

  `POST /admin/publish` is new and Admin-gated, matching Rust's wire contract:
  `{"target": {"type", "id"}, "event": {...}}` → `{"delivered": n}`. **Unlike the
  Go and TypeScript servers, which route by connection id only and honestly 501 the
  other four kinds, all five targets are deliverable here** — this backplane has
  the fan-out to back them.

  `delivered` stays truthful: it counts this process's sockets, so `0` means
  "nobody on this pod", never a fabricated success. A publish to an unknown target
  returns `{"delivered": 0}` rather than pretending, and a bad body is a 400 rather
  than a silent no-op.

  One deliberate design note: `publish` snapshots the matching sinks under the
  registry lock and calls them **outside** it. The sinks are non-blocking enqueues,
  but a host's sink is arbitrary code and invoking it under the lock would let one
  bad sink deadlock every connection.

  11 new tests. The registry tests port Rust's `backplane.rs` unit tests; the route
  tests port its `tests/admin_publish.rs`; and one end-to-end test drives a real
  connection loop through `create_conversation_session`, then publishes to its
  session/user/org/agent and asserts the events **land on the socket** — not just
  that a counter incremented — and that detach makes it unroutable again. That last
  one is the test that would catch the association wiring silently never running.

  .NET is untouched: it has no backplane, no connection registry and no
  `/admin/publish`, so it needs its own build-out plus coordination with the .NET
  admin work rather than being folded in here.

## 1.46.7

### Patch Changes

- 6b61d58: fix: the Postgres stores must report the conversation's org, or the new org gate silently falls open

  PRs #405/#408 made organization the OUTER scope on conversation access, but only
  wired the in-memory stores and the dispatcher gate. The Postgres stores kept
  returning sessions with the org field unset, and the gate treats an unrecorded
  org as "fall through to ownership" — deliberately, so rows predating org capture
  don't lock their owners out. The result: on the one backend that actually holds
  several organizations' data, a cross-org read of an ownerless conversation was
  allowed again, given only a session id.

  It compiled, and every test passed: the Postgres store tests never go through the
  gate, and the gate tests never use a Postgres store. Nothing failed loudly.

  Go, TypeScript and Python now read `organization_id` off the session row (and, in
  TS, off `getConversation`) and stamp it on the returned session. The data was
  already being persisted correctly — only the read path dropped it — so this is a
  read fix, with no schema change and no migration.

  One regression test per language, each using an OWNERLESS conversation on purpose:
  with an owned one, ownership alone blocks the cross-org read and the test passes
  without proving anything about the org check. The Go test drives the real
  `ConversationScope.Allows` rather than just asserting the field, and is
  mutation-verified — reverting the fix fails it with `OwnerOrg = ""`.

## 1.46.6

### Patch Changes

- 3a7dcdd: fix(go): scope conversations by organization, not just owner

  `ConversationScope.Allows` consulted ownership only. Because an **ownerless**
  conversation is deliberately reachable (th-909995 Option B keeps anonymous,
  emailless-authenticated and legacy sessions usable), an ownerless conversation
  belonging to **another org** was readable by anyone holding its conversation id —
  authorization resting on an unguessable UUID, which leaks through logs, referrers
  and screenshots.

  Org is now the OUTER scope, checked before ownership: `Unscoped` (auth-disabled
  dev) still short-circuits first, a conversation from another org is invisible
  regardless of ownership, and ownerless conversations stay reachable **within
  their own org** — so th-909995 is preserved rather than reverted. The owning org
  is recorded on the conversation at creation and carried on `StoredSession` so the
  dispatcher chokepoint can check it.

  A conversation with **no** org recorded falls through to the ownership check, so
  rows created before org capture are not locked away from the people who own them.

  TypeScript and Python have the same gap and follow in their own PRs.

- bcc6e4a: fix(ts,python): scope conversations by organization, not just owner

  The same gap Go closed in #405. The ownership gate consulted the owner only, and
  because an **ownerless** conversation is deliberately reachable (it keeps
  anonymous, emailless-authenticated and legacy sessions usable), an ownerless
  conversation belonging to **another org** was readable by anyone holding its id —
  authorization resting on an unguessable UUID.

  Org is now the OUTER scope, checked before ownership: an auth-disabled server is
  still fully unscoped, a conversation from another org is invisible regardless of
  ownership, and ownerless conversations stay reachable **within their own org**.
  The owning org is recorded on the conversation at creation, never rewritten on
  resume, and carried on the stored session so the dispatcher chokepoint can check
  it. A conversation with no org recorded falls through to the ownership check, so
  rows predating org capture stay reachable by their owners.

## 1.46.5

### Patch Changes

- 8cd539a: test: assert `eventual_response.usage` token counts in the shared scenario corpus, 5/5 servers

  `basic-streaming-turn` now asserts `data.data.usage.promptTokens` = 10 and
  `data.data.usage.completionTokens` = 5. All five native servers produce it, so
  per-turn token reporting is a real cross-language invariant instead of a
  "not yet assertable" note in the corpus README.

  Three things had to land first, two of them upstream:

  - the five mock providers agree on scripted usage (`smooth-operator-core`, released);
  - the Rust and C# scenario runners compare JSON numbers by value (#381);
  - **the Rust runner must push a `StreamEvent::Usage(scripted_usage())` into the
    stream it synthesizes.** The sibling mocks build their own final usage chunk
    from the FIFO script; Rust's `MockLlmClient` takes an explicit event list, and
    without that event the engine falls back to estimating completion tokens from
    the reply's _length_ (~4 chars/token) — the `0/5` that moved whenever a
    scenario's text changed. Verified load-bearing: removing the event fails the
    scenario with `data.data.usage.promptTokens = 0 != 10`.

  The Rust and Go servers' core dependency is bumped to a release carrying the
  aligned mocks (the other three already floated past it). Note that the registries
  number this package independently — the release wave that published the aligned
  mocks was crates.io 1.8.0 and npm/PyPI/NuGet 1.7.15 — so a cross-repo bump needs
  the version read out of each ecosystem's lockfile, not one number copied across.

  `costUsd` is deliberately left unasserted — it depends on which model a server
  names and which pricing table its engine carries, and those legitimately differ.
  The corpus README now says so explicitly rather than filing it as a gap.

## 1.46.4

### Patch Changes

- 1be263d: feat(python): durable Postgres storage — sessions, conversations and the admin stores survive a restart

  The Python server was memory-only: every session, conversation, message and
  `/admin/*` connector config, agent setting and indexing run vanished on restart.
  It now has a Postgres backend, selected with `SMOOTH_AGENT_STORAGE=postgres` plus
  `SMOOTH_AGENT_DATABASE_URL` (falling back to `DATABASE_URL`, but only once
  `postgres` has been asked for explicitly — an ambient `DATABASE_URL` alone can
  never change where data goes). Unset, or `memory`, keeps the in-memory stores
  exactly as they were; an unknown value, or `postgres` with no connection string,
  raises rather than silently falling back to memory, because losing durability
  quietly is the failure worth shouting about.

  This completes the trio started in the Go (#386) and TypeScript (#392) servers.
  One `PostgresStore` implements both the existing `SessionStore` and the new
  `AdminStore` seam (`admin.py`'s three containers move behind an ABC;
  `InMemoryAdminStore` stays the default). The schema is the Rust reference
  adapter's, copied verbatim, so all five servers share one set of tables rather
  than each inventing a dialect.

  Nothing new was invented to hold Python-specific state. A conversation's owner is
  the email on its `user` participant row, which is what Rust's
  `list_conversations_by_org_and_user` already filters on; the per-conversation
  workflow step lives in `conversations.metadata_json` and the per-session OTP bit
  in `conversation_sessions.metadata`, where Rust already keeps `otpVerified`. The
  one addition is `org_id` on `indexing_runs` via `ALTER TABLE ... ADD COLUMN IF NOT
EXISTS`, because the Rust `IndexingStore` is not org-scoped but the `/admin/*` run
  list must be.

  Org isolation needed a carrier, since `SessionStore` had no org anywhere.
  `create_session` and `list_conversations` gained a keyword-only `org_id`
  defaulting to `DEFAULT_ORG_ID` — the same `"public"` a principal without an `org`
  claim already carries in `auth.py`. That default is a specific org, never "all
  orgs": the auth-disabled unscoped list is still confined to its own org, because
  widening ownership must not widen tenancy. Existing callers and the in-memory
  store are unaffected — the memory store accepts the argument and ignores it, being
  single-tenant by construction.

  `asyncpg` is an optional dependency (the `postgres` extra); nothing imports
  `postgres_store` unless the env var selects it, so the memory path needs no
  database driver installed.

  Fourteen tests cover it against a real Postgres via testcontainers: a write→read
  round-trip through a second connection (the durability claim itself), message
  ordering and limits, resume ownership with no existence oracle, ownerless
  conversations staying reachable, the scoped and unscoped conversation lists, org
  isolation driven with the same email in two orgs, workflow-step and OTP-bit
  persistence (including clearing each without disturbing the other keys), and CRUD
  plus org isolation for all three admin stores. They skip cleanly when Docker is
  unavailable; three need no Docker at all and are the guard that the in-memory path
  stays the default.

## 1.46.3

### Patch Changes

- dcea5e5: fix(rust): clear every clippy 1.96 failure in the workspace, so the gate can be turned on

  Nine hard errors under `-D warnings` on clippy 1.96, all pre-existing and all
  invisible because the clippy step is `continue-on-error`. Main goes red the moment
  GitHub's stable runner reaches 1.96.

  - **`unnecessary_sort_by` ×6** — `adapters/in-memory` (3) and `adapters/dynamodb` (3).
    `sort_by(|a, b| …cmp…)` → `sort_by_key`, with `Reverse` on the two newest-first
    sorts. Same ordering, no behavior change.
  - **`too_many_arguments`** — `smooth-operator-server/src/protocol.rs::eventual_response`.
    Eight flat wire fields, one arg per emitted JSON key; given the same
    `#[allow]` its sibling builders in `handler.rs` and `server.rs` already carry,
    rather than a refactor that would touch all ten call sites for a lint heuristic.
  - **`while_let_loop`** — a `loop` + `let … else { break }` in a test's socket read,
    now the `while let` clippy asked for.
  - **`derivable_impls`** — `AuthMode`'s hand-written `Default` is now `#[derive(Default)]`
    - `#[default]`.

  The last three only surfaced once the adapter errors were fixed: the build failed
  before those crates were ever reached, so "fix the sort_by sites" and "clippy is
  clean" were not the same job.

  `cargo clippy --workspace --all-targets -- -D warnings` exits 0 on 1.96; 650 tests
  pass; `cargo fmt --check` clean.

- 35d7954: fix(ts,python,dotnet): make `eventual_response.usage.costUsd` real instead of always 0

  The TypeScript, Python and .NET servers injected a raw SDK client into the engine —
  `openai`'s `OpenAI`/`AsyncOpenAI`, and MEAI's OpenAI adapter. Every one of those
  parses the HTTP response and throws the headers away, and the gateway reports
  per-request cost ONLY in a response header. So core's cost-header parser (shipped
  across all five engines in core#121) had nothing to read, and every turn on these
  three servers reported `costUsd: 0`. Go was already correct because it injects core's
  own `GatewayClient`; Rust because it builds its own `LlmClient`.

  All three now inject the header-reading client core ships:

  - **TypeScript** — `createGatewayClient({ baseURL, apiKey })`. This also deletes the
    optional lazy-`import('openai')` dance, its swallow-everything `catch`, and a
    hand-rolled `createStream` adapter that had no way to carry a cost at all. `openai`
    now arrives transitively through core.
  - **Python** — `GatewayLlmProvider(client=…)`, wrapping the `AsyncOpenAI` the server
    already built so the base-url-optional branch stays the single place that decides
    the endpoint.
  - **.NET** — `GatewayChatClient`, which also drops the `Microsoft.Extensions.AI.OpenAI`
    package reference from the host entirely.

  .NET needed a second fix: its `TurnRunner` hardcoded `new TurnUsage(0, …)`, so the
  client swap alone would have changed nothing. It now folds the gateway cost the
  client surfaces on each streaming update's `AdditionalProperties`, summed across the
  turn's model calls. The `ponytail:` comment that documented the old always-zero
  behaviour is updated rather than left to mislead.

  Absent-and-zero handling is unchanged and now tested at the server boundary: a header
  that is PRESENT and reports `0` is not locked in as a real $0 — it falls through
  exactly as an absent header does.

  Tests are real turns against a real local HTTP+SSE gateway in each language, asserted
  on what the protocol actually emits rather than inside the engine — TS 5, Python 5,
  .NET 5. Each language additionally pins the WIRING (that the server hands the engine a
  header-reading client), because the pipeline tests inject the client directly and
  would stay green if the server regressed to the raw SDK. Both halves were
  mutation-tested: reintroducing the bug fails exactly the intended tests.

  Core minimums move to the lowest version per registry that actually ships the clients:
  npm `^1.8.4`, PyPI `>=1.8.3`, NuGet `1.7.16` (NuGet has no 1.8.x; PyPI's 1.8.4 has no
  installable files).

- 521caa2: feat(dotnet): serve the full `/admin/*` management API, so the console renders against .NET

  The C# host shipped four admin routes — `/admin/health`, `/admin/me`, a repo-listing
  `/admin/connectors`, and its own `POST /admin/reindex`. The console needs sixteen, so
  four of its five pages 404'd and .NET was the one engine the management UI could not
  drive. This adds the ten missing route families (eleven method+path combos): the
  conversations list and per-conversation messages, indexing runs, document sets, the
  full connector CRUD plus `POST /admin/connectors/{id}/index`, and settings GET/PUT.

  Two of the existing routes changed shape to match the contract, because the .NET-only
  shapes could never have rendered. `/admin/me` now answers `{userId, orgId, role}`
  instead of `{sub, org, role, groups}`, and `/admin/connectors` now lists persisted,
  org-scoped connector configs rather than the env-configured GitHub repos — those are
  still reachable through `POST /admin/reindex`, which is unchanged and remains this
  host's own extra route. Shapes are built against `console/lib/types.ts`, not the Rust
  struct field names: those read snake_case in source but serialize camelCase, so
  copying them yields a server that passes its own tests and renders nothing.

  The auth gate is the one the other four servers use, and it fails closed in both
  directions. A missing bearer token is 401 even on a no-auth server; a token an
  auth-enabled server cannot verify is 401 rather than an anonymous grant; below the
  required role is 403. `AUTH_MODE=none` resolves to an **Admin** principal, matching
  Rust's `NoAuthVerifier` — without that the console 403-walls against a local server,
  which is exactly as useless as the 404s this closes. The gate also now resolves
  through the same `IAuthVerifier` seam the WebSocket host uses, so a host running with
  `SMOOTH_LOCAL_TOKEN` authenticates admin requests instead of silently falling back to
  the `TokenAccessResolver`.

  Org scoping lives in the handlers, not the store: a cross-org id 404s identically to
  an unknown one, so the API is never an existence oracle for another org's rows, and
  the internal owner key is `[JsonIgnore]`d off every response. `GET /admin/settings`
  returns defaults on a miss rather than 404.

  Backed by an in-memory `AdminStores` for now, as in the Go and TypeScript servers. A
  host can register its own instance in DI to pre-seed or share that in-memory state,
  but that is not a storage swap point: `AdminStores` is sealed with get-only
  collections, so durable storage will need this class changed. It is the first admin
  store in `dotnet/` — there was none before, so converging one onto Rust's
  `ADMIN_SCHEMA` is now a real follow-up rather than something to converge against
  today. Document sets are honestly empty and a connector index run records zero
  documents — this server has no per-connector ingestion pipeline yet, and inventing
  counts would render a lie.

  Verified by 28 xUnit integration tests over real HTTP against a booted host
  (auth-fail-closed on every gated route, role rank in both directions, the no-auth
  admin grant and that it does not leak into an auth-enabled server, org isolation, and
  each response shape), and by booting the Next.js console with `CONSOLE_AUTH=dev`
  against the C# host: all five pages render, and the connector create → edit → index →
  delete and settings-save flows round-trip through the UI, with zero `/admin/*` 404s.

## 1.46.2

### Patch Changes

- b4614a9: feat(go,ts): `POST /admin/publish`, with an honest `delivered` count

  The second Rust-only admin route (epic th-9e792d item R). Admin-gated; pushes a
  realtime event to a connected client without going through an agent turn — the
  plug point for non-AI publishers (job status, ingestion progress, notifications).

  **`delivered` never lies.** These servers' backplanes are a connection-id → sink
  registry, so only `connection` targets are routable. Rust additionally fans out
  to `session` / `user` / `org` / `agent` over a richer backplane; here those
  answer **501 `UNSUPPORTED_TARGET`** and carry no `delivered` field at all, rather
  than a misleading `{"delivered": 0}` that a caller would read as "accepted,
  reached nobody" for an event that was never routable. A genuine 0 — a routable
  `connection` target that simply isn't attached — is still reported as 0.

  To make the count truthful, `Backplane.Publish` now returns the number of sinks
  reached (Go: signature change, no existing callers; TypeScript: a new **optional**
  `publish` method, so a third-party backplane that predates it stays valid and the
  route answers 501 when it's absent).

  Python and .NET are deliberately excluded: their backplanes hold no sink to
  deliver to, which is connection-lifecycle work rather than an admin-route port.

## 1.46.1

### Patch Changes

- 27d32d5: feat(ts): durable Postgres storage — sessions, conversations and the admin stores survive a restart

  The TypeScript server was memory-only: every session, conversation, message and
  `/admin/*` connector config, agent setting and indexing run vanished on restart.
  It now has a Postgres backend, selected with `SMOOTH_AGENT_STORAGE=postgres` plus
  `SMOOTH_AGENT_DATABASE_URL` (falling back to `DATABASE_URL`, but only once
  `postgres` has been asked for explicitly — an ambient `DATABASE_URL` alone can
  never change where data goes). Unset, or `memory`, keeps the in-memory stores
  exactly as they were; an unknown value, or `postgres` with no connection string,
  throws rather than silently falling back to memory, because losing durability
  quietly is the failure worth shouting about.

  One `PostgresStore` implements both the existing `SessionStore` and the new
  `AdminStore` seam (`admin.ts`'s three maps move behind an interface;
  `InMemoryAdminStore` stays the default). The schema is the Rust reference
  adapter's (`rust/adapters/postgres/src/schema.rs`) copied verbatim —
  `conversations`, `conversation_participants`, `conversation_messages`,
  `conversation_sessions`, `connector_configs`, `agent_settings`, `indexing_runs` —
  so all five servers share one set of tables rather than each inventing a dialect.
  Everything is `CREATE ... IF NOT EXISTS`, so whichever server boots first creates
  them. The one addition is `org_id` on `indexing_runs`, added with
  `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` (the same idempotent back-fill the Rust
  schema uses for `knowledge_vectors.acl`), because the Rust `IndexingStore` is not
  org-scoped but the `/admin/*` run list must be.

  Nothing new was invented to hold TypeScript-specific state. A conversation's owner
  is the email on its `user` participant row, which is what Rust's
  `list_conversations_by_org_and_user` already filters on, and `contactEmail` /
  `otpVerified` / `currentStepId` live in `conversation_sessions.metadata`, where
  Rust already keeps `otpVerified`.

  Org isolation needed a carrier, since `SessionStore` had no org anywhere.
  `createSession`, `getConversation` and `listConversations` gained a trailing
  `orgId` that defaults to `DEFAULT_ORG_ID` — the same `'public'` a principal
  without an `org` claim already carries in `auth.ts`. That default is a specific
  org, never "all orgs": widening ownership must not widen tenancy, and the
  auth-disabled unscoped list is still confined to its own org. Existing callers and
  the in-memory store are unaffected — the memory store accepts the argument and
  ignores it, being single-tenant by construction.

  Fourteen tests cover it against a real Postgres via testcontainers: a write→read
  round-trip through a second connection (the durability claim itself), message
  ordering and limits, resume ownership with no existence oracle, ownerless
  conversations staying reachable, the scoped and unscoped conversation lists, org
  isolation driven with the same email in two orgs, workflow-step and OTP-bit
  persistence, and CRUD plus org isolation for all three admin stores. They skip
  cleanly when Docker is unavailable; three need no Docker at all and are the guard
  that the in-memory path stays the default.

## 1.46.0

### Minor Changes

- 552b826: Converge the C# server's Postgres store onto the shared schema.

  The C# server had invented its own tables: `conversation_identity_state` and
  `conversation_workflow_state` side tables, a narrower `conversation_sessions` carrying its own
  `user_email` column, and no `conversations` or `conversation_participants` at all. It now reads and
  writes the same shape as the Rust source of truth (`rust/adapters/postgres/src/schema.rs`) and the
  Go store, so one database can be driven by any of the servers.

  The per-session bits the side tables held moved into `conversation_sessions.metadata` under the key
  names the other servers already read (`contactEmail`, `otpVerified`, `currentStepId`), and the
  conversation owner now lives on the `user` participant rather than a duplicated session column — so
  a resumed session reports the original owner instead of whoever resumed it.

  A legacy database migrates in place on store init: the tables are widened, the side tables and
  `user_email` are backfilled into `metadata` and `conversation_participants`, and the invented tables
  are dropped. The store's `ISessionStore` surface is unchanged.

## 1.45.3

### Patch Changes

- 66c1c27: feat(go,ts,python): `GET /admin/model-costs`, so the console's cost badges render

  One of the two Rust-only admin routes (epic th-9e792d item R). The Go,
  TypeScript and Python servers now serve it on the same contract as
  `rust/smooth-operator-server/src/admin.rs`.

  Ungated, as in Rust — gateway pricing is not org-sensitive and the badges must
  render on a tokenless local connection. The gateway's `/model/info` is fetched at
  most once per process (pricing is stable), and **only a success is cached**: any
  gateway or transport failure degrades to `{}` with a 200 and leaves the cache
  unset so the next request retries. A missing badge beats a broken page, and one
  blip must not pin an empty map for the process lifetime.

  Field mapping mirrors Rust's `map_model_info`: entries without a `model_name` are
  skipped, and every field is **null when the gateway omits it** rather than
  defaulted — a $0 default would render a free-model badge on a paid model.

  `POST /admin/publish` is deliberately not in this PR: the backplanes differ per
  server (Go routes by connection id only, TypeScript has sinks but no publish,
  Python holds no sink at all), so a faithful port needs a decision on what
  `delivered` may report rather than silently returning 0.

## 1.45.2

### Patch Changes

- ff794e3: feat(go): durable Postgres storage — sessions, conversations and the admin stores survive a restart

  The Go server was memory-only: every session, conversation, message and `/admin/*`
  connector config, agent setting and indexing run vanished on restart. It now has a
  Postgres backend, selected with `SMOOTH_AGENT_STORAGE=postgres` plus
  `SMOOTH_AGENT_DATABASE_URL` (falling back to `DATABASE_URL`). Unset — or `memory` —
  keeps the in-memory stores exactly as they were; an unknown value, or `postgres`
  with no connection string, is a hard error rather than a silent fall back to
  memory, because losing durability quietly is the failure worth shouting about.

  One `PostgresStore` implements both the existing `SessionStore` and the new
  `adminStore` seam. The schema is the Rust reference adapter's
  (`rust/adapters/postgres/src/schema.rs`) copied verbatim — `conversations`,
  `conversation_participants`, `conversation_messages`, `conversation_sessions`,
  `connector_configs`, `agent_settings`, `indexing_runs` — so all five servers share
  one set of tables rather than each inventing a dialect. Everything is
  `CREATE ... IF NOT EXISTS`, so whichever server boots first creates them.

  Nothing new was invented to store Go-specific state. A conversation's owner is the
  email on its `user` participant row, which is what the Rust adapter's
  `list_conversations_by_org_and_user` already filters on, and the per-session bits
  (`contactEmail`, `otpVerified`, `currentStepId`) live in
  `conversation_sessions.metadata`, where Rust already keeps `otpVerified`. The one
  addition is an `org_id` column on `indexing_runs`, added with
  `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` (the same idempotent back-fill the Rust
  schema uses for `knowledge_vectors.acl`): the Rust `IndexingStore` is not org-scoped
  but the `/admin/*` run list must be.

  Isolation is enforced in SQL, in the selection rather than after a limit.
  `ConversationScope` gained an `OrgID`, carried from the authenticated principal, so
  every conversation read is scoped by org first and owner second; the in-memory store
  ignores it and is unchanged. A conversation in another org — like one owned by
  another user — is invisible to `list_conversations` and unresumable, and reports
  identically to one that never existed, so neither can be used as an existence oracle.

  Thirteen tests cover it against a real Postgres via testcontainers: a write→read
  round-trip through a second connection (the durability claim itself), message
  ordering and limits, resume ownership with no oracle, ownerless conversations
  staying reachable, the auth-disabled unscoped list, org isolation with the same email in two orgs, workflow-step and
  OTP-bit persistence, and CRUD plus org isolation for all three admin stores. They
  skip cleanly when Docker is unavailable, and two of them need no Docker at all —
  they are the guard that the in-memory path stays the default.

## 1.45.1

### Patch Changes

- 61ec3c1: feat(server): run turns through the engine's `AgentExecutor` seam (ADR-030)

  `runner::run_streaming_turn` no longer calls `Agent::run_with_channel` directly. It
  resolves an executor via the new `turn_executor()` and drives the turn through the
  engine's `AgentExecutor` seam instead. With the default `InProcessExecutor` that IS
  the same call — a verbatim delegation — so every deployed turn is byte-for-byte what
  it was, and the full server test suite passes unchanged.

  What the indirection buys is a single place to select a durable backend. The runner
  wraps the turn in roughly two hundred lines of event translation, confirmation- and
  interaction-bridge teardown, and OTel span emission; without the seam, a durable
  backend would have to be threaded through all of it. Now it plugs in at one function.

  Durable mode is opt-in via `SMOOTH_AGENT_DURABLE_EXECUTOR` and off by default. Today
  it still resolves to in-process and logs a warning, because the durable backend lives
  in `smooai-smooth-operator-temporal`, which is `publish = false` in the engine repo:
  this crate consumes the engine from crates.io and is itself published, so it can take
  neither a git nor a path dependency on it. Warning and falling back is deliberate — a
  turn the client believes will survive a disconnect, but won't, is worse than no
  durable mode at all.

  To be clear about what this does and does not fix: it does **not** yet stop a parked
  write-approval from dying on a browser refresh. That park is in-process and bounded at
  about five minutes, and it stays that way until the Temporal executor plugs in here.
  When it does, the engine's `AgentTurnWorkflow` gates approval-required tools on
  durable `approve_tool` / `deny_tool` signals, so the pending decision lives in workflow
  history rather than in the process serving the socket. This change is the precondition
  for that, not the fix itself.

  Also raises the `smooai-smooth-operator-core` floor to 1.7.10, the first published core
  carrying the `executor` module.

## 1.45.0

### Minor Changes

- 1b466d2: Unify the server env-var contract across all five implementations behind `SMOOTH_AGENT_*`.

  The five language servers read divergent names for the same settings — Go and Python
  took a combined `SMOOTH_OPERATOR_BIND`, TypeScript split it into `SMOOTH_OPERATOR_HOST` +
  `SMOOTH_OPERATOR_PORT`, .NET had its own `SMOOTH_DATABASE_URL` / `SMOOTH_AUTH_MODE` family
  and no bind var at all (it took ASP.NET's `:5000`), and Rust read a bare `AUTH_MODE`. Every
  implementation now reads the canonical `SMOOTH_AGENT_*` names that Rust, the Helm chart,
  the container images and the docs already used, so switching engines no longer means
  relearning the config surface.

  Each host keeps its previous names as **aliases** — the canonical name wins when both are
  set — so no existing deployment breaks. Go's default port also moves from `8793` to `8787`,
  matching the other four processes and all five container images.

  The `SMOOAI_GATEWAY_URL` / `SMOOAI_GATEWAY_KEY` pair is deliberately unchanged: it is the
  wider SmooAI gateway contract shared with launchers and benches, and was already identical
  in all five hosts.

## 1.44.6

### Patch Changes

- 20ea52a: test(rust,dotnet): compare scenario-corpus JSON numbers BY VALUE, not by representation

  The Rust and C# scenario-parity runners strict-compared JSON numbers, in
  **opposite** directions: `serde_json::Value`'s `PartialEq` compares a `Number`'s
  internal discriminant, so Rust emitted `0.0` and rejected an integer `0`, while
  `JsonNode.DeepEquals` compares representation, so C# emitted `0` and rejected
  `0.0`. Go (marshal → `float64` → `DeepEqual`), Python (`0 == 0.0`) and
  TypeScript (both parse to `number`) already compared loosely.

  That split alone made `eventual_response.usage` unassertable in the shared
  corpus — its `costUsd` is a float that a matcher naturally writes as `0` — so no
  scenario could name a numeric field without picking a spelling that two of the
  five runners would reject.

  Both runners now compare numbers by value and recurse structurally through
  arrays and objects; everything that is not a number keeps exact equality, so a
  number still never equals its string. Each runner gains an offline unit test
  pinning this, independent of the corpus.

  Test-only: no server or protocol behavior changes.

## 1.44.5

### Patch Changes

- e504744: feat(python): the `/admin/*` management API, so the console works against the Python server

  The management console 404s against the Python server: only Rust and C#
  implemented the admin API. The Python server now serves the same 14 endpoints the
  console's typed client calls, on the same wire contract as
  `rust/smooth-operator-server/src/admin.rs` — same paths, camelCase JSON, and the
  `{"error":{"code","message"}}` envelope.

  Auth matches Rust: Bearer token → verify → role-rank gate, 401 for a missing or
  invalid token and 403 for an insufficient role, `/admin/health` ungated.
  `AUTH_MODE=none` grants Admin exactly as Rust's `NoAuthVerifier` does, so the
  console doesn't 403-wall against a local server; an auth-enabled server is
  unaffected.

  **One deviation, forced by the transport.** Rust, Go, C# and TypeScript serve
  `/admin/*` and `/ws` on one port. `websockets`' handshake parser accepts GET only
  and raises on any non-zero `Content-Length`, so its `process_request` hook cannot
  serve a POST/PUT API — the request never reaches it. The admin API therefore
  listens on its own port (default: ws port + 1, override with `admin_port`, read
  back via `Server.admin_port`). The console configures its admin base URL
  separately from the WS URL, so this is a config value, not a code change.

## 1.44.4

### Patch Changes

- 7cd7c4e: feat(ts): the `/admin/*` management API, so the console works against the TypeScript server

  The management console 404s against the TypeScript server: only Rust and C#
  implement the admin API. The TS server now serves the same 14 endpoints the
  console's typed client calls, on the same wire contract as
  `rust/smooth-operator-server/src/admin.rs` — same paths, camelCase JSON, and the
  `{"error":{"code","message"}}` envelope. Plain HTTP previously answered every
  request with a 426; `/admin/*` is now handled and everything else still is.

  Auth matches Rust: Bearer token → verify → role-rank gate, 401 for a missing or
  invalid token and 403 for an insufficient role, `/admin/health` ungated. Reads
  are Curator, writes Admin. `AUTH_MODE=none` (the local dev flavor) grants Admin
  exactly as Rust's `NoAuthVerifier` does — without it the console 403-walls
  against a local server. An auth-enabled server is unaffected.

  Conversations and messages come from the existing session store; connector
  configs, settings and indexing runs are in memory for now (the durable storage
  adapter is a separate workstream) and every row is org-scoped.

## 1.44.3

### Patch Changes

- 16767d1: feat(go): the `/admin/*` management API, so the console works against the Go server

  The management console 404s against the Go server: only Rust and C# implement the
  admin API. The Go server now serves the same 14 endpoints the console's typed
  client calls, on the same wire contract as
  `rust/smooth-operator-server/src/admin.rs` — same paths, camelCase JSON, and the
  `{"error":{"code","message"}}` envelope.

  Auth matches Rust exactly: `Authorization: Bearer <token>` → verify → role-rank
  gate, 401 for a missing/invalid token and 403 for an insufficient role, with
  `/admin/health` ungated. Ranks are basic=0 / curator=1 / admin=2; reads are
  Curator, writes are Admin. `AUTH_MODE=none` (the local dev flavor) grants Admin
  exactly as Rust's `NoAuthVerifier` does — without it the console 403-walls
  against a local server, which is as useless as the 404s. An auth-enabled server
  is unaffected.

  Conversations and messages are served from the existing session store. Connector
  configs, settings and indexing runs are held in memory for now (the durable
  storage adapter is a separate workstream); every read and write is org-scoped, so
  one org can never see or mutate another's rows.

## 1.44.2

### Patch Changes

- b6c6b84: feat(rust): forward-port the host-approver seam + its drain fix to main

  `runner::HostApprover` and `LocalServerBuilder::host_approver` (th-be3f55), plus
  the one-drain-task starvation fix that followed them (th-2105e9), only ever
  existed on the `th-daemon-memory-seam` side lineage. smooth's daemon consumes
  both, which is why smooth pinned `smooth-operator-server`/`-svc` to a git rev off
  that branch — and that rev predates the `..` fixes, so it cannot compile against
  core >= 1.7.3. Pinning the branch was what pinned core at `=1.7.2`.

  Bringing both commits onto main dissolves that side lineage, so smooth can point
  at main and take current cores.

  **th-be3f55 — host-approver seam.** The companion to `tool_hooks`: that seam lets
  a host install a permission gate, but a gate that can only allow or deny is a gate
  that must run in Bypass. Supplying the receiving ends of the hook's approver
  channel gives its `Ask` the same treatment the tool-pattern HITL already has — the
  turn parks, the client is sent `confirm_tool_action_required`, and
  `confirm_tool_action` resumes it. Unset ⇒ unchanged (a host `Ask` still fails
  closed). The confirmation config is now built when EITHER tool patterns are
  configured OR a host approver is supplied; it was patterns-only before.

  **th-2105e9 — one drain task, not one per turn.** Each turn's bridge locked the
  shared request receiver for its whole lifetime, so a turn parked awaiting a human
  held it and starved every other turn on the connection.

## 1.44.1

### Patch Changes

- 9011d5e: fix(rust): add `..` to the last four exhaustive `AgentEvent` matches

  #328 added `..` to the two `ToolCallComplete` matches that core 1.7.3's new
  `details` field actually broke. Four sibling matches on the same enum were still
  exhaustive, so the NEXT field added to any of those variants breaks every
  consumer that unifies the dependency graph on a newer core — the same failure,
  just deferred:

  - `smooth-operator/src/runtime.rs` — `ToolCallStart` (in `tool_arguments_for`)
  - `smooth-operator-server/src/runner.rs` — `TokenDelta`, `ReasoningDelta`, `ToolCallStart`

  These compile fine today; the fix is purely so they keep compiling. Construction
  sites in tests are deliberately left alone — `..` doesn't apply there, and a new
  field should break a mock that claims to build a complete event.

## 1.44.0

### Minor Changes

- 58c2648: fix(rust): the workflow step-attempt cap never fired — a shadowed binding fed it a permanent 0

  `handler.rs` carried the step-attempt-cap capture block **twice, back to back**, with
  identical comments. The two differed in exactly one field: the first took the count
  from `loaded_attempts` (durable conversation metadata), the second from
  `state.session_step_attempts(session_id)` (the per-pod in-memory session map). Rust
  shadowing meant the second won.

  That silently reverted th-c12df5, which had deliberately moved the workflow pointer
  _and_ its attempt count onto durable conversation storage precisely because the per-pod
  map "reset them to step 0 every turn, freezing the workflow at the first step so the
  judge/cap could never advance it".

  It was worse than the old behaviour, though. The two stores used different metadata
  keys — `persist_workflow_step` writes `workflowStepAttempts`, while the session
  accessor read `stepAttempts`, a key nothing ever wrote, and its writer
  (`set_session_step_attempts`) had no callers anywhere in the repo. So the surviving
  binding fed `apply_step_cap` a **permanent 0**: `next_attempts` never reached
  `WORKFLOW_STEP_ATTEMPT_CAP`, and a workflow step the judge never accepts could loop
  forever — exactly the pathological-visitor case (th-d57a1d) the cap exists to bound.

  The fix keeps the durable source and deletes the duplicate. `AppState::session_step_attempts`
  and `AppState::set_session_step_attempts` are **removed** rather than left in place:
  both were vestigial from the pre-th-c12df5 design, neither had a live caller, and the
  getter's doc comment still advertised itself as feeding the cap. Leaving them is what
  let the wrong source get wired in, so removing them makes the mistake unavailable
  rather than merely un-made. This also clears three `unused variable` warnings that had
  been emitted on every build.

  Covered by a regression test that drives the real per-turn pipeline
  (`load_workflow_step` → `apply_step_cap` → `persist_workflow_step`), reloading from
  storage each turn as a reconnect or pod hop would, and asserting the held step
  force-advances exactly at the cap. Verified by mutation: feeding it a zeroed count
  fails the test the same way the bug did.

## 1.43.0

### Minor Changes

- 108ab71: feat(go,python,ts,dotnet): emit `eventual_response.usage` — a spec field that was implemented 1-of-5

  `eventual-response.schema.json` has defined an optional `usage` object
  (`{ costUsd, promptTokens, completionTokens }`) since cost reporting landed, but
  only the Rust server ever put it on the wire. The other four engines all track
  per-turn token accounting already — the data existed and was dropped on the floor
  at the last hop, so a client on any non-Rust server could not accumulate session
  cost at all.

  Each server now captures the turn's accumulated usage from its engine's terminal
  completion event and threads it onto `eventual_response`, matching the Rust
  reference's semantics exactly: the key is **omitted entirely** when the engine
  reported no usage, so the event stays byte-identical for clients that predate the
  field.

  - **Go** — captured at `core.StreamDone` off `AgentRunResponse`, carried on `TurnResult.Usage`.
  - **Python** — captured at `DoneEvent` off `response.usage` / `response.cost_usd`.
  - **TypeScript** — captured at the `done` stream event off `response.usage` / `response.costUsd`.
  - **C#** — the engine's `RunStreamingAsync` surfaces no terminal usage total, so the
    server accumulates the model's own `UsageContent` chunks over the turn, which is
    the same total once the stream ends. Token counts are real; `costUsd` is 0 until a
    pricing table is wired (see below).

  Two limitations worth knowing, both pre-existing and documented in
  `spec/conformance/scenarios/README.md` rather than papered over:

  - **`costUsd` is 0 on every non-Rust server.** None of them wires a pricing table
    onto its engine, so the cost tracker prices every call at 0. Only Rust reports a
    real figure, which it reads from the gateway's cost header. The token counts are
    unaffected.
  - **The scenario corpus cannot assert `usage` yet.** The five mock LLM providers
    disagree on what a scripted turn reports (Go/Python/TS 0/0, Rust 0/5, C# 10/5),
    and the Rust and C# scenario runners strict-compare JSON numbers in opposite
    directions (`0.0` vs `0`). Aligning the mocks in `smooth-operator-core` and
    comparing numbers by value would make `usage` a real parity assertion. Until then
    each server's protocol unit tests cover the contract that matters — omitted when
    absent, all three fields when present.

## 1.42.0

### Minor Changes

- 8772758: Postgres storage: fix the turn-loop deadlock and wire knowledge retrieval end-to-end, plus runnable docker-compose examples.

  The Postgres storage backend deadlocked on **every** turn and wedged the whole server (even `GET /health` hung). Root cause: the adapter exposes the engine's _synchronous_ stores (`CheckpointStore`, `KnowledgeBase`, `Memory`, and the admin `SettingsStore` / `ConnectorConfigStore` / `IndexingStore`) over an async `deadpool` pool via a `run_blocking` bridge that spawned the future on the **server's main runtime** and then blocked the calling worker on a channel. Persona resolution calls `PgSettingsStore::get` on every turn, so every turn hit it; the spawned future never got driven and the turn never completed (confirmed by a stack sample: worker parked in `run_blocking → mpsc recv`).

  Fixes (`rust/adapters/postgres`, `rust/smooth-operator-server`):

  - **Dedicated bridge runtime.** The sync-over-async bridges now spawn their futures on a process-wide multi-threaded runtime whose workers are always free, so the turn's blocking wait always resolves. This is the actual deadlock fix.
  - **Checkpointer.** `PostgresAdapter::checkpoints()` returns the engine's in-memory `MemoryCheckpointStore` instead of the sync r2d2 `PostgresCheckpointStore` (whose blocking `Drop`/I/O was the first-found offender). OLTP + knowledge stay Postgres-durable; only crash-resume of an in-flight turn degrades.
  - **Embeddings URL.** `GatewayEmbedder` appended `/v1/embeddings` to a base URL that already ends in `/v1` (`…/v1/v1/embeddings` → 404 on every retrieval). It now appends `/embeddings`.
  - **Seeding on Postgres.** `SMOOTH_AGENT_SEED_KB=1` was silently ignored for the Postgres/DynamoDB backends (`seed_knowledge` was typed to the in-memory adapter). It now seeds through the `StorageAdapter` trait, scoped to the reference org so the multi-tenant query path actually matches the seeded rows.

  Verified end-to-end against `pgvector/pgvector:pg16`: streaming replies, durable conversations, human-in-the-loop approval, and knowledge retrieval with citations all work on Postgres storage; `/health` stays responsive across rapid turns.

  Also adds two docker-compose example stacks — `examples/web-chat` (Vite/React PWA) and `examples/tui-chat` (terminal client) — each `docker compose up` with Postgres (pgvector) + the operator + a client, BYO OpenAI-compatible gateway via a single `.env`. The server image now defaults `SMOOTH_AGENT_BIND=0.0.0.0` (the correct container default; the process default stays loopback).

## 1.41.0

### Minor Changes

- caa3678: TS server: resolve `send_message.skill` server-side (Rust PR #338 parity).

  The TS server carried the `skill` field on the wire and ignored it — its own 1.39.0 changelog said so outright ("the TS / Python / Go / .NET servers ignore the field for now"). It now resolves the skill and composes it into the turn:

  - **`skills.ts`** — `isValidSkillName` (ASCII alphanumerics + `-`/`_`, ≤128 chars, making `..`, `/`, `\` and NUL _unrepresentable_ rather than filtered), `stripFrontmatter` (drops the discovery-metadata YAML so the model sees only instructions; unterminated frontmatter is returned untouched rather than swallowing the file), `skillSection`, and `resolveSection`.
  - **`SkillResolver`** — the host seam, via `serve({ skillResolver })`.
  - **`DirSkillResolver`** — `<root>/<name>/SKILL.md` over the `:`-separated roots in `SMOOTH_SKILLS_DIR`, first root wins. `serve()` prefers an explicit resolver, else `DirSkillResolver.fromEnv()`, mirroring Rust's `install_skill_resolver_from_env`. Neither ⇒ no resolver, so a multi-tenant deploy never serves host skills by accident.
  - **Fail-CLOSED**, unlike `images`: an unresolvable skill returns `error { code: "SKILL_NOT_FOUND" }` and the turn does not run — resolved _before_ the 202 ack, so a client never sees "accepted" for a turn that was never going to happen. A blank/whitespace `skill` is treated as absent, matching Rust's trim-then-filter.
  - The body is appended to the **system prompt**, last, so it is the most salient instruction into the turn while the persisted user message stays exactly what the user typed — skill prose never accumulates in history to be replayed on every later turn.

  Tests: all five Rust `skills.rs` unit tests ported under their Rust names, plus over-the-socket coverage for fail-closed (asserting the model is never called), system-prompt-not-user-message placement (including that frontmatter never reaches the model), and blank-as-absent with a resolver installed. 254 server tests green.

  Backward compatible: an absent `skill` field is byte-for-byte the previous behavior.

## 1.40.0

### Minor Changes

- 15dab99: Release the .NET server work that has been sitting on `main` unpublished: the file-transfer contract (#348) and server-side skill resolution (#352).

  Neither shipped, and the cause was a changeset-target slip rather than anything wrong with the code. `@smooai/smooth-operator-server` is the **TypeScript** server npm package; the .NET NuGet `SmooAI.SmoothOperator.Server` takes its `<Version>` from `scripts/sync-versions.mjs`, which stamps the **lockstep anchor** `@smooai/smooth-operator` (`typescript/package.json`) onto every non-npm manifest. #348's changeset named only `@smooai/smooth-operator-server`, so release #349 bumped the TS package to 1.8.0 and left the anchor at 1.39.0 — and the .NET package was never republished. The sibling TS PR #346 got this right, naming both packages.

  Net effect for consumers: `SmooAI.SmoothOperator.Server` 1.39.0 has no `TurnContext`, no directive sink, and no `images[]`/`files[]` ingest, even though `main` has had all of it since 2026-08-11. Bumping the anchor stamps the csproj and publishes the NuGet, so 1.39.0 consumers get file transfer **and** skill resolution in one release.

  No source change — this is purely the release plumbing the two .NET PRs missed.

## 1.39.0

### Minor Changes

- d41643e: File transfer (TS server): implement the PR #342 contract to Rust parity. `send_message` now parses `images[]` and `files[]`: images are attached to the turn's user message as OpenAI `image_url` content parts (via a `withUserImages` request-body wrapper, since the published core takes a plain-string message and reuses it for retrieval), while files are surfaced — never sent to the model — on a new per-turn `ToolContext`. A new optional `toolProvider` seam (mirroring the Rust `ToolProvider`) hands host tools that context, including a directive sink; a host tool that writes `ctx.directive` (e.g. a `send_file` directive) has it drained onto `eventual_response.directive` (last-write-wins). All attachment parsing is fail-soft. The client SDK's generated protocol types gain the `images`/`files` request fields and the `directive` response field.

## 1.38.0

### Minor Changes

- cfba8bf: MCP client: configured Model Context Protocol servers now surface as engine tools.

  New `mcp` module with `McpConfig` / `McpServerConfig` (the `[[servers]]` TOML shape Smooth already writes to `~/.smooth/mcp.toml`, extended with `url` + `bearer_token` for streamable HTTP) and `McpToolProvider`, a `ToolProvider` that connects each server, lists its tools, and registers them as `mcp__<server>__<tool>`. Because it goes through the existing `ToolProvider` seam, MCP tools land in the same per-turn `ToolRegistry` as the built-ins, so a host's `ToolHook`s (permission gate, Narc) apply with no extra wiring. `ChainedToolProvider` composes a host's own provider with the MCP one.

  Both transports are supported — stdio (spawned child process) and streamable HTTP with an optional bearer token. `env`, `args`, `url` and `bearer_token` expand `${env:VAR}` at connect time so secrets stay out of config files. Calls are bounded by a per-server timeout (60s default) and MCP tool-level errors map to engine tool errors. A server that will not connect is logged and skipped — its tools are simply absent from the turn, never a crash.

  Tools only in this version: resources, prompts, sampling, and `notifications/tools/list_changed` are not implemented.

## 1.37.0

### Minor Changes

- cd9b686: `send_message` grows an optional `skill` field — the engine resolves and composes the skill, not the client.

  Until now every client resolved a skill itself and prepended its markdown body to the message text. That put prose on the wire, and it persisted the skill body into conversation history, where it was replayed as context on every subsequent turn. The wire now carries the intent (`skill: "code-review"`) and the server does the work.

  - **`skills::SkillResolver`** — the host seam, installed via `AppState::with_skill_resolver` or `LocalServerBuilder::skill_resolver`.
  - **`skills::DirSkillResolver`** — the working default: `<root>/<name>/SKILL.md` over the `:`-separated roots in `SMOOTH_SKILLS_DIR`, first match wins, YAML frontmatter stripped. Unset ⇒ no resolver, so a multi-tenant deploy never serves host skills by accident.
  - The resolved body becomes a **system-prompt section** for that turn only, so the persisted user message stays exactly what the user typed.
  - **Fail-closed**, unlike `images`: an unresolvable skill returns `error { code: "SKILL_NOT_FOUND" }` and the turn does not run. Names are restricted to `[A-Za-z0-9_-]{1,128}`, making path traversal unrepresentable.

  Backward compatible: an absent `skill` field is byte-for-byte the previous behavior. Rust is the reference implementation; the TS / Python / Go / .NET servers ignore the field for now (the same staging `images` is in).

## 1.36.12

### Patch Changes

- f6a7652: fix(dotnet): log the exception behind every `INTERNAL_ERROR`, and read the shared `SMOOAI_*` gateway env vars

  Two bugs, one of which hid the other.

  **th-e7ef23 — the swallowed exception.** Both places `FrameDispatcher` turns an exception into the
  protocol's `INTERNAL_ERROR` (the dispatcher's outer guard and the spawned turn's guard) discarded the
  exception entirely. A server whose every turn failed logged _nothing_ — no exception, no stack, at any
  level. Both sites now route through `LogInternalError`, which records the action, the requestId and the
  exception (with stack) at `Error` level, falling back to stderr when no `ILogger` is wired. The wire
  message is unchanged: still generic, still leaks no detail to the client.

  **th-df7007 — every .NET turn returned `INTERNAL_ERROR`.** The host read the gateway URL/key/model from
  `SMOOTH_GATEWAY_URL` / `SMOOTH_GATEWAY_KEY` / `SMOOTH_MODEL`, but rust, go, ts and python all read the
  `SMOOAI_*` spelling. Any launcher or bench using the shared contract left the .NET host keyless, so every
  turn hit the gateway with the literal key `"unset"` and got back `HTTP 401 … LiteLLM Virtual Key expected`
  — surfaced as a bare `INTERNAL_ERROR` with zero tool calls. The host now reads `SMOOAI_GATEWAY_URL` /
  `SMOOAI_GATEWAY_KEY` / `SMOOAI_MODEL` first, with the `SMOOTH_*` names kept as a fallback for existing
  deployments.

  **Same swallow in the siblings.** The Go dispatcher dropped the `err` at all seven of its
  `INTERNAL_ERROR` sites and the Python dispatcher dropped the exception at both of its own; both now
  log it (Go through a single `internalError` chokepoint using `log/slog`, Python via
  `logging.exception`). TypeScript already logged via `console.error`; Rust puts the detail on the wire.

## 1.36.11

### Patch Changes

- ce26442: Send temperature 1.0, not 0.0 — many frontier models reject anything but their default

  A growing set of models accept only their default temperature and 400 the entire
  request ("Unsupported value: 'temperature' does not support 0 with this model").
  The symptom does not look like a config error: the server boots, accepts the turn,
  every LLM call 400s, and the user sees an assistant that silently says nothing.

  A per-model allowlist would be provably wrong — `gpt-5.1` rejects while `gpt-5.2`
  accepts, `gpt-5.4` accepts while `gpt-5.4-pro` rejects. `1.0` was accepted by all
  12 models tested across 6 families, so it is the one value that works everywhere.

  Centralised as `config::DEFAULT_TEMPERATURE`. The cleaner long-term shape is
  `Option<f32>` on `LlmConfig`, so the request omits the field entirely and takes
  each provider's own default.

## 1.36.10

### Patch Changes

- 379cb28: feat(engine): `StorageAdapter::memory_for_access` seam wires durable auto-recall into every turn

  The engine core already supported memory auto-recall (`AgentConfig::with_memory` →
  `build_context_messages` → `memory.recall(msg, 5)`), but the server never wired it:
  `StorageAdapter` exposed `checkpoints()`/`knowledge()` but no memory accessor, and
  the runner built each turn's `AgentConfig` without `.with_memory(...)`. This adds
  `StorageAdapter::memory_for_access(&access) -> Option<Arc<dyn Memory>>` (defaulting to
  `None`, so every existing backend is byte-for-byte unchanged — hosted auto-recall stays
  a deliberate opt-in, not a side effect), and the runner now calls `.with_memory(...)`
  whenever the adapter returns `Some`. The in-memory conformance adapter gains a
  `with_memory(...)` builder + override so the seam is exercised end-to-end. This lights
  up Big Smooth's durable auto-recall: its single-tenant SQLite adapter overrides
  `memory_for_access` to return its store, so remembered preferences are injected into
  every turn without the agent calling `recall` (th-374b27).

## 1.36.9

### Patch Changes

- d43971d: Add a per-step attempt cap to the conversation-workflow judge so a guided assessment can't stall forever on one step. The judge only advances on `yes`; when a step's criteria demand evidence the judge never accepts, the step re-asks indefinitely and a multi-step flow (e.g. the public Transformation Posture agent) never reaches its scoring / lead-capture step (th-d57a1d). The step pointer already persists and advances correctly — this adds the missing escape hatch: `apply_step_cap` force-advances to the next step after `WORKFLOW_STEP_ATTEMPT_CAP` (3) consecutive non-advancing turns, resetting the counter on any advance. The counter persists in session metadata (`stepAttempts`) alongside the existing `currentStepId` pointer. With tuned criteria the cap rarely fires — it's the safety net for a pathological non-answering visitor.

## 1.36.8

### Patch Changes

- 156c6ab: feat(ts server): `toolHooks` seam plumbs consumer-supplied ToolHooks into every turn's tool registry

  The TypeScript server gains a `toolHooks` option on `ServerOptions` (and
  `serveLocal`), forwarded verbatim through `FrameDispatcher` → `TurnRunner` →
  the engine's `AgentOptions.toolHooks`. Consumer-supplied `ToolHook`s run around
  every dispatched tool: `preCall` before execution (a throw blocks the call) and
  `postCall` after with a mutable result it may redact. Unlike `tools`, hooks
  bypass the per-agent enabled-tools filter and auth gating — they observe/redact
  every call. Empty ⇒ behaviour unchanged. This is the server half of the
  polyglot ToolHook parity work, mirroring the Rust `LocalServerBuilder` hook seam
  feeding the per-turn `ToolRegistry`. Requires `@smooai/smooth-operator-core`
  with the `ToolHook` lifecycle.

## 1.36.7

### Patch Changes

- 799124c: SECURITY (Rust server): scope conversations by owner only when an owner exists (Option B, th-909995)

  `may_read_conversation` mapped every principal without an email — an anonymous connection to an
  auth-enabled server, or a token carrying `sub`/`org`/`role` but no `email` — to `UserScope::Denied`
  and refused it everything. The session such a connection creates is ownerless by construction, so
  it was locked out of its own session: empty `list_conversations`, resume refused, `get_session` /
  `get_conversation_messages` / `send_message` all `SESSION_NOT_FOUND`. The identical rule in the
  .NET twin hung CI on a WebSocket ACL test and was reverted in #309.

  Option B: a conversation that HAS an owner (a `user` participant with a non-blank email) is still
  owner-checked, case-insensitively; one with NO owner is readable, as it was before scoping shipped.
  `Denied` matches no non-empty owner, so the reported P0 stays closed: authenticated A cannot read,
  resume, or `send_message` into authenticated B's owned session, and a refusal appends nothing to
  B's log. `Err(_) => false` (a storage error is a denial), the `UserScope` enum, and the
  `scoped_session` chokepoint at all 7 call sites are unchanged, as is the unauthenticated
  `LocalServer` / smooth-daemon embedding (`UserScope::Unscoped`).

  Option A (`email ?? sub`) was rejected: Go's anonymous principal uses the literal sub `"anonymous"`
  for every visitor, so keying on `sub` would pool all anonymous visitors and leak their chats to
  each other.

  `list_conversations` now applies that same predicate per conversation instead of the
  `list_conversations_by_org_and_user` storage pushdown, which cannot express "mine or ownerless" —
  so the list can never disagree with what `get_session` will hand over. Still filtered before the
  limit, so pages are never silently short.

## 1.36.6

### Patch Changes

- 86465a1: SECURITY (Go): fix conversation ownership check to close cross-user access without locking out anonymous and emailless principals.

  The previous rule (`s.Unscoped || (s.Email != "" && ownerEmail == s.Email)`) denied everything to any connection whose principal carried no email claim. On an auth-enabled server that population is real — `AnonymousPrincipal` has no email, and plenty of IdPs issue tokens without one — so those callers got an empty conversation list, a refused resume, and a refused `send_message` on the session they had just created. That is an outage for anonymous/public-agent chat, a supported scenario. The identical rule in the .NET sibling hung CI on a WebSocket ACL test that authenticates without an email claim, forcing a revert there (#309); Go had no equivalent test, which is why it went unnoticed.

  `ConversationScope.Allows` now owner-checks only conversations that HAVE an owner:

  - a session with an owner is readable and writable only by that same principal — this keeps the reported P0 (authenticated A reaching into authenticated B's owned session) closed on both the read and the write path;
  - a session with no owner (anonymous, emailless-authenticated, or legacy auth-disabled) stays reachable, since there is no owner to enforce on behalf of.

  Keying ownerless sessions on `sub` instead was considered and rejected: `AnonymousPrincipal.Sub` is the literal string `"anonymous"` for every visitor, so it would pool all anonymous conversations into one shared bucket and leak them to each other.

  Email comparison is now case-insensitive (`strings.EqualFold`), matching the .NET and Python siblings — OIDC providers vary on the casing of the email claim.

  Unchanged: the `scopedSession` chokepoint every sessionId-taking handler routes through, the identical `SESSION_NOT_FOUND` for not-yours vs never-existed (no existence oracle), selection-side filtering for the conversation list, and the auth-disabled unscoped path.

## 1.36.5

### Patch Changes

- 5d0a499: SECURITY (Python server): scope sessions by owner only when an owner exists (Option B, th-909995)

  The th-8fe998 scoping rule fail-closed on any principal without an email claim. On an
  auth-enabled server that locked out **anonymous connections and authenticated-but-emailless
  principals entirely** — the session they had just created was ownerless, so `list_conversations`
  returned empty, resume minted a fresh conversation, and `get_session` / `get_conversation_messages`
  / `send_message` all answered `SESSION_NOT_FOUND`. The identical rule in the .NET twin hung CI on
  a WebSocket ACL test and was reverted in #309.

  Option B: a session that HAS an owner is still owner-checked (case/whitespace-insensitive email
  match); a session with NO owner — anonymous, emailless, or predating ownership — is reachable, as
  it was before scoping shipped. An emailless scope matches no non-empty owner, so the reported P0
  stays closed: authenticated A cannot read, resume, or `send_message` into authenticated B's owned
  session, and a refusal appends nothing to B's log. Not-yours remains byte-identical to
  never-existed (no existence oracle), and the auth-disabled single-tenant flavor is unchanged.

  Option A (`email ?? sub`) was rejected: Go's anonymous principal uses the literal sub `"anonymous"`
  for every visitor, so keying on `sub` would pool all anonymous visitors together and leak their
  chats to each other.

  `SessionStore.create_session` / `list_conversations` gain a keyword-only `enforced: bool = False`
  that distinguishes "auth disabled, unscoped" from "authenticated but emailless" — both of which
  present as a `None` owner.

## 1.36.4

### Patch Changes

- b1a0568: SECURITY (.NET server): scope the conversation WRITE path, not just the reads.

  th-966fab owner-checked `get_session` / `get_conversation_messages` / resume, but
  `send_message` still loaded any session by client-supplied `sessionId`. An
  authenticated user who knew (or guessed) another user's `sessionId` could send a
  message into that session — the turn replayed the victim's conversation history as
  context and streamed the agent's reply back to the _attacker_. A read of someone
  else's conversation dressed up as a write, defeating the read scoping entirely.
  `verify_otp` and `confirm_tool_action` were unscoped the same way (marking a
  foreign session identity-verified; approving a foreign parked write).

  The fix adopts the Go server's chokepoint pattern: a single private
  `ScopedSessionAsync` is now the only way a handler may turn a client-supplied
  `sessionId` into a session. It hides a session the connection's principal doesn't
  own by returning exactly what an unknown id returns, so every caller emits the
  identical not-found response and "not yours" stays indistinguishable from "never
  existed". All five sessionId-taking handlers route through it.

  The visibility rule is "Option B": a session that HAS an owner is owner-checked; a
  session with NO owner is reachable. A first attempt (#308) also denied ownerless
  sessions and emailless principals outright, and was reverted (#309) — an
  authenticated principal whose token carries no `email` claim, and an anonymous
  connection to an auth-enabled server, both stamp `ownerEmail = null` at
  `create_conversation_session` and were then refused by their own session on the next
  `send_message`. That is not "denied someone else's history", it is "cannot use the
  product": it killed anonymous/public-agent chat, and hung the .NET integration suite,
  whose ACL test converses over exactly that path. Ownerless sessions remain absent from
  `list_conversations` and non-resumable by `conversationId`, so reaching one requires
  already holding its `sessionId`. No behavior change when auth is disabled
  (single-tenant local/dev stays unscoped).

## 1.36.3

### Patch Changes

- d17fa66: SECURITY: TS server — owner-check only conversations that HAVE an owner (Option B)

  The per-user scoping rule shipped in #297 scoped an authenticated principal with no
  `email` claim (and an anonymous connection to an auth-enabled server) to an unownable
  sentinel, and required `ownerEmail === scope` on every read. That denied such callers
  EVERYTHING — empty list, resume refused, `send_message` refused — locking them out of
  the session they had just created, i.e. no anonymous or emailless chat at all on an
  auth-enabled server. The identical rule in .NET hung CI on a WebSocket ACL test and was
  reverted in #309; TS had no equivalent test, so it went unnoticed here.

  `mayRead` now allows a conversation with NO owner and owner-checks one that has an
  owner. The reported P0 stays closed: authenticated A still cannot read or write
  authenticated B's owned session (`SESSION_NOT_FOUND`, byte-identical to a never-existed
  id, nothing appended to B's log), and an emailless scope still matches no real owner —
  the list stays empty for emailless principals rather than pooling every anonymous
  visitor's chats into one readable bucket.

  Owner comparison is now case-insensitive (read path and list selection), matching .NET
  and Python — OIDC providers vary on the casing they emit for the same identity.

## 1.36.2

### Patch Changes

- bd7fb5b: SECURITY (Rust server): owner-check every sessionId-taking WebSocket action.

  The per-user scoping added for the read paths left the write paths loading a
  session by raw client-supplied id. `send_message` was the worst case: an
  authenticated user who knew or guessed another user's `sessionId` could send a
  message into that session — the turn replayed the victim's conversation history
  as context and streamed the agent's reply back to the _sender_, so the write
  hole was also a read of the victim's conversation. `get_session`, `verify_otp`,
  `confirm_tool_action`, `submit_interaction` and `rename_conversation` had the
  same gap.

  All of them now route through a single `scoped_session` chokepoint (mirroring
  the Go dispatcher's `scopedSession`): it loads the session and hides it unless
  the connection's authenticated principal owns its conversation, returning
  exactly what an unknown id returns — so "not yours" is byte-identical to "never
  existed" and cannot be used as an existence oracle. A storage error is a denial.
  Unauthenticated single-user deployments (the `th` daemon / `LocalServer`
  embedding) stay unscoped, and org scoping remains as defense in depth.

## 1.36.1

### Patch Changes

- 770edec: SECURITY: fix cross-user conversation-history leak in the Python server's `get_conversation_messages`.

  The per-user scoping fix added a `_visible_session` ownership chokepoint and routed
  `get_session`, `send_message` and `verify_otp` through it, but `get_conversation_messages`
  still called the store directly — so any authenticated user could read any other user's
  full conversation history by sessionId. It now routes through the same chokepoint and
  reports a session it does not own with the byte-identical `SESSION_NOT_FOUND` payload it
  uses for an id that never existed (no existence oracle). A structural test now fails if any
  future handler bypasses the chokepoint again.

## 1.36.0

### Minor Changes

- ef9a697: **SECURITY (Rust): per-user conversation scoping — fixes a cross-user data leak.**
  `list_conversations` was scoped by organization only, and the resume-by-`conversationId`
  path plus `get_conversation_messages` were not owner-checked at all, so any authenticated
  user in an org could enumerate and open every other user's conversations in that org.

  Conversation reads are now scoped to the connection's **authenticated principal** (the
  JWT `email` claim, surfaced as `Principal::email`), on top of — never instead of — the
  existing org scope. The scope is derived only from the verified token: a create frame's
  client-supplied `userEmail` no longer decides the session's identity when the connection
  is authenticated (that was the spoofing vector), and the same fix is applied to the
  Lambda transport's create path. `StorageAdapter` gains
  `list_conversations_by_org_and_user`, which filters in the query rather than after a
  limit; Postgres pushes it down to one `EXISTS` query, other adapters use the trait's
  participant-filtering default, so a new adapter is scoped by construction.

  Fail-closed rules: auth enabled + principal email ⇒ scoped; auth enabled but no principal
  or no `email` claim ⇒ empty list and every read denied (never a silent fall back to the
  whole org); auth **disabled** (`AUTH_MODE=none`, unconfigured, or the single-user
  `local-token` daemon / `LocalServer`) ⇒ unscoped, behavior unchanged. Denials are
  indistinguishable from genuine misses — another user's session returns the byte-identical
  `SESSION_NOT_FOUND` an unknown id returns, and resuming another user's conversation mints
  a fresh one exactly as an unknown id does, so there is no existence oracle to enumerate
  conversation ids with.

## 1.35.0

### Minor Changes

- 16c5d4e: **SECURITY (.NET server) — cross-user conversation data leak.** `list_conversations` returned EVERY
  user's conversations, and `create_conversation_session` resume, `get_conversation_messages`, and
  `get_session` performed no ownership check. Any authenticated user could enumerate and open anyone
  else's chats. The conversation surface is now scoped to the connection's **authenticated principal**.

  What changed:

  - `Principal` carries `Email` (init-only, lifted from the validated token's `email` claim), and
    `AccessContext` carries `AuthEnabled` — which distinguishes "no auth configured" from "auth on but
    this token is anonymous". The second case now fails closed instead of inheriting unscoped behavior.
  - `create_conversation_session` stamps the **principal's** email as the session owner. The frame's
    client-supplied `userEmail` is honoured only when no auth is configured at all — supplying someone
    else's email no longer buys you their scope.
  - Resume, `get_conversation_messages`, and `get_session` are owner-checked. A conversation/session you
    do not own returns `SESSION_NOT_FOUND` with a payload **byte-identical** to one that never existed,
    so the error cannot be used as an existence oracle to enumerate other users' ids. (This includes
    resume of an unknown id, which previously minted a fresh conversation — under auth it now returns
    the same `SESSION_NOT_FOUND`.)
  - Conversations with no recorded owner (rows written before scoping existed) belong to nobody and are
    invisible to every authenticated user.
  - Auth-disabled single-tenant local/dev servers are **unchanged**: unscoped, no ownership checks.

  **BREAKING for anyone implementing `ISessionStore`** (this compile break is deliberate — an optional
  parameter defaulting to "no filter" would be fail-open and would leave downstream stores silently
  vulnerable):

  - `ListConversationsAsync(CancellationToken)` → `ListConversationsAsync(ConversationScope scope, CancellationToken)`.
    Apply the scope **inside your query** (`WHERE user_email = …`), never as a post-hoc filter in
    C# — the dispatcher applies its `LIMIT` to what you return, so filtering afterwards yields short or
    empty pages. `ConversationScope.Unscoped` returns every user's conversations and is legitimate ONLY
    on a server with no auth configured; `ConversationScope.None` returns nothing.
  - New required member `ConversationBelongsToUserAsync(string conversationId, string userEmail, CancellationToken)`.
    It MUST return `false` — indistinguishably — for a conversation that does not exist, one owned by
    another user, and one with no recorded owner.

  Hosts that pass identity to the server must ensure the token carries an `email` claim; a principal
  without one now sees no conversations rather than everyone's.

## 1.34.0

### Minor Changes

- b79184f: **SECURITY (cross-user data leak) — Go server: scope conversations to the authenticated user.**

  `SessionStore.ListConversations` took no user filter, so `list_conversations` returned **every user's conversations** to any authenticated caller. The resume path and `get_conversation_messages` were not owner-checked either, so a caller could also open and read another user's conversation by id. Any authenticated user could enumerate and read anyone else's chats.

  Conversations are now owned by the **authenticated principal** and every read is filtered by it:

  - `Principal` carries `Email` (the JWT `email` claim); `AccessContext.ConversationScope()` derives the connection's visibility. Ownership comes from the connection's principal only — the client-supplied `userName` / `userEmail` frame fields were the spoofing vector and no longer influence who may read what (`userEmail` still serves as the OTP delivery contact).
  - `ListConversations` filters during selection, before the handler's limit — filtering after a limit silently returns short/empty pages.
  - `get_session`, `get_conversation_messages`, `send_message` and `verify_otp` all route session lookups through one owner-checked chokepoint.
  - **Not-yours is indistinguishable from never-existed.** A denied session read returns the identical `SESSION_NOT_FOUND` payload an unknown id returns, and resuming another user's conversation mints a fresh conversation exactly as an unknown id does — so neither path can be used as an oracle to enumerate other users' session or conversation ids.
  - Fails **closed**: auth enabled with a principal that has no email (including a rejected/expired token) sees nothing, rather than falling back to unscoped.
  - Auth **disabled** (no verifier configured — local/dev single-tenant) stays unscoped and is unchanged. That is the only unscoped path.

  **BREAKING for `SessionStore` implementers (deliberate).** `ListConversations`, `CreateSession` and `ResumeSession` now take a required `ConversationScope`. The parameter is required rather than optional-defaulting-to-unscoped precisely so that every downstream implementation gets a **compile error** and must confront who may see what; a default-to-unscoped parameter would be fail-open and would leave downstream stores silently vulnerable.

  Migration: thread the scope from `AccessContext.ConversationScope()` into your store, persist the owning email on conversation creation, and filter reads by `scope.Allows(ownerEmail)`. `ConversationScope`'s zero value denies everything, so a partially-migrated store leaks nothing.

- 011db17: **SECURITY** — fix a cross-user conversation data leak in the TypeScript server, and scope every conversation read to the connection's authenticated principal (th-8fe998).

  **The vulnerability.** `SessionStore.listConversations()` took no user filter, so the `list_conversations` action returned EVERY user's conversations to any caller. The resume path (`create_conversation_session` with a `conversationId`) and `get_conversation_messages` performed no owner check either, so any authenticated user could enumerate other users' conversation ids from the list and then open, read, and post into those conversations. `get_session`, `send_message`, and `verify_otp` were exposed through the same missing check.

  **The fix.** A session now records an owner — the authenticated principal's email, taken from the connection's `email` claim. Every conversation read is checked against it:

  - `list_conversations` is scoped to the principal, with the filter applied inside the store selection (ahead of any limit, so a scoped page is never silently short or empty).
  - `get_session`, `get_conversation_messages`, `send_message`, and `verify_otp` return `SESSION_NOT_FOUND` for a session the caller doesn't own — byte-identical to the response for an id that never existed, so the pair can't be used as an existence oracle to enumerate other users' session ids.
  - Resuming another user's conversation is treated exactly like resuming an unknown id: the id is dropped and a fresh conversation is minted. Erroring on a real-but-not-yours id while silently minting for an unknown one would itself confirm which ids exist.
  - The client-supplied `userName` / `userEmail` frame fields no longer determine identity. They were the spoofing vector: a caller could claim any email and receive that user's scope. The principal always wins; on an auth-enabled server the frame values are ignored for ownership (`userEmail` still serves as the OTP delivery contact).

  **Fail-closed rules.** Auth enabled and the principal has an email → scoped to it. Auth enabled and the principal is missing or emailless (including a missing, expired, or forged token) → empty list and denied reads, never a silent fall back to unscoped. Auth disabled (no verifier configured — local/dev single-tenant) → unscoped, unchanged; this is the only unscoped path.

  **BREAKING for custom `SessionStore` implementations** — deliberately, and the break is the point:

  - `listConversations()` gains a **required** `userEmail: string | undefined` parameter. It is required, not optional-defaulting-to-unscoped, because an optional parameter is fail-OPEN: existing implementations would keep compiling and keep leaking every user's conversations. The compile error forces each implementation to make an explicit scoping decision.
  - `getConversation()` now returns `{ conversationId, userEmail }`, with `userEmail` required so a store that doesn't track ownership fails to compile rather than silently reporting every conversation as ownerless.
  - `StoredSession` gains an optional `userEmail` (the owner). Implementations must persist it at create time and must NOT let a resume rewrite it, or a second caller could take ownership of a conversation by resuming it.

  Migration: filter conversations by the passed `userEmail` in the query itself (`WHERE user_email = ?`), never after applying a limit; return `undefined` for `userEmail` only when the row genuinely has no owner. `AccessContext` also gains a required `authEnabled` flag, set by the verifier, which distinguishes "auth is off" (unscoped) from "auth is on but this connection didn't authenticate" (fail closed) — custom `AuthVerifier` implementations must set it.

## 1.33.0

### Minor Changes

- b38cb4b: **SECURITY — Python server: per-user conversation scoping (th-8fe998).** Fixes a cross-user data leak: `list_conversations` took no user filter and returned **every** user's conversations, and neither the resume path nor the sessionId-bearing actions were owner-checked, so any authenticated user could enumerate and open anyone else's chats.

  Conversations are now owned by the **authenticated principal's** email (the JWT `email` claim, plumbed onto `Principal`) — never the client-supplied `userName` / `userEmail` frame fields, which were the spoofing vector. With auth enabled the principal's email also replaces `userEmail` as the OTP contact, so a verification code can't be delivered to a client-chosen address.

  - `list_conversations` is scoped to the caller, with the filter applied **in the store's selection** — not after the dispatcher's limit, which would silently return short or empty pages.
  - `create_conversation_session` (resume), `get_session`, `send_message`, and `verify_otp` are owner-checked. Someone else's id is reported **byte-identically** to an id that never existed — the resume path mints a fresh conversation, the rest return the same `SESSION_NOT_FOUND` payload — so none of them can be used as an existence oracle to enumerate other users' ids.
  - Fail-closed: auth enabled + a principal with no email lists nothing and can resume nothing; it never falls back to unscoped. A session stored with no owner is invisible to everyone. Auth **disabled** (the local single-tenant flavor) is the only unscoped path and is unchanged.

  **BREAKING (`SessionStore` implementers).** `list_conversations` now takes a **required** `user_email` parameter — deliberately not an optional defaulting to `None`/unscoped, which would be fail-open and would let a downstream store ship cross-user-leaking without ever confronting the question. `create_session` gains a keyword-only `owner_email`, and `StoredSession` gains `owner_email`.

  Migration: pass the authenticated principal's email through both and filter your selection by it. Pass `None` **only** for a single-tenant, auth-disabled deployment, where it means "unscoped". If you implement this protocol in your own store, treat a not-owned row exactly as a missing one.

## 1.32.1

### Patch Changes

- 3acca21: .NET server: implement turn cancellation (the "Stop button") — the `cancel` action, ported from the Rust reference. `FrameDispatcher` now tracks the connection's single in-flight `send_message` turn with a per-turn `CancellationTokenSource`: a `{"action":"cancel","requestId":"<turn>"}` frame cancels it (dropping the turn at its next await, abandoning the in-flight LLM/tool call) and emits the terminal `cancelled` event (`status: 499`, echoing the turn's `requestId`) in place of `eventual_response`. The partial assistant message is discarded — the user's message, persisted before the agent loop, stays. A cancel with no active turn is a silent no-op; a second `send_message` while a turn is in flight is rejected with `TURN_IN_PROGRESS` rather than run concurrently. A client disconnect aborts the in-flight turn as well, while graceful shutdown still drains it. No engine change.

## 1.32.0

### Minor Changes

- d17ede9: Emit `stream_preamble` from the **.NET server**. It already had the generated protocol type but never produced the event and never read `SMOOTH_AGENT_PREAMBLE_MODEL`, so a host running on the C# server could not turn the feature on at all — this closes that gap and brings the .NET lane to parity with the Rust reference.

  When `SMOOTH_AGENT_PREAMBLE_MODEL` is set (e.g. `groq-gpt-oss-20b`), `TurnRunner` fires a small fast model IN PARALLEL with the agent loop — same gateway and key as the turn, with only the model id and a 64-token output cap overridden — and emits ONE short present-tense "what I'm about to do" sentence as an ephemeral `stream_preamble` event, covering the reasoning model's time-to-first-token. The system prompt is byte-identical to the other servers'.

  It is deliberately defined by what it must never do: the turn never awaits it (it can't delay or gate the answer), an atomic first-answer-token guard drops it the moment real answer tokens start streaming, any failure (timeout, gateway error, bad model id) is logged at debug and swallowed with no error event reaching the client, and the text is never persisted nor folded into `eventual_response`. Unset, empty, or whitespace ⇒ the feature is off, no extra LLM call is made, and behavior is byte-for-byte unchanged.

- bfaf1a8: Go server: emit `stream_preamble`. When `SMOOTH_AGENT_PREAMBLE_MODEL` is set, a small fast model runs in parallel with the turn and streams one ephemeral "what I'm about to do" sentence, covering the reasoning model's time-to-first-token — matching the Rust reference server's prompt, 64-token cap, and first-answer-token race guard. Unset/empty/whitespace leaves behavior and the model-call count unchanged. The preamble is best-effort (failures swallowed) and ephemeral (never persisted, never folded into `eventual_response`).
- ff2e4d9: Python server: emit `stream_preamble`. When `SMOOTH_AGENT_PREAMBLE_MODEL` is set, a small fast model runs concurrently with each streaming turn and emits one ephemeral "what I'm about to do" sentence, covering the reasoning model's time-to-first-token — matching the Rust reference server (same system prompt, same 64-token cap, same gateway/key with only the model id overridden).

  The preamble never delays or gates the real turn, is dropped the instant the first real answer token is emitted, is never persisted or folded into `eventual_response`, and any failure is swallowed at debug. Unset, empty, or whitespace ⇒ off (the default): no extra LLM call, behavior byte-for-byte unchanged.

## 1.31.0

### Minor Changes

- 2fc8486: TypeScript server: emit `stream_preamble` (pearl th-8e0a52).

  The TS server now honours `SMOOTH_AGENT_PREAMBLE_MODEL`, matching the Rust reference. When set, a small fast model runs in parallel with each turn on the same gateway/key (model id + a 64-token cap are the only overrides) and emits ONE ephemeral "what I'm about to do" sentence to cover the reasoning model's time-to-first-token.

  Off by default: unset, empty, or whitespace means no extra LLM call, no extra event, behaviour byte-for-byte unchanged. The preamble is suppressed once the real answer starts streaming, is never persisted or folded into `eventual_response`, and any failure is swallowed at debug so it can never fail or delay a turn.

## 1.30.0

### Minor Changes

- a15fd43: .NET server: `get_conversation_messages` pages by an opaque `cursor` (a message id) instead of the `before` ISO-timestamp cursor, and returns `nextCursor` alongside `hasMore`.

  A timestamp cursor is broken by design — two messages can share a timestamp at any precision the wire keeps, so a `created_at < cursor` filter drops or repeats the collisions. An id cursor names exactly one message. The paging path no longer compares timestamps at all, and the 500-message `before` rescan window (and its paging ceiling) is gone. The .NET client SDK's `GetMessagesAction.Before` becomes `Cursor`, and `GetMessagesResult` gains `NextCursor`. Breaking wire change for clients still sending `before`.

- 5c0fb98: Python server: `get_conversation_messages` pages by opaque `cursor`, not the `before` timestamp.

  The handler now reads `cursor` (a message id today), locates that message in the conversation log, and returns the page immediately older than it. Responses carry `nextCursor` — the id of the oldest message in the page, non-null exactly when `hasMore` is true. An unknown or stale cursor is a `VALIDATION_ERROR` rather than a silently empty page. `createdAt` stays on every message for display, with microsecond precision intact; it is simply no longer the cursor.

  This removes code. The old `_BEFORE_SCAN_WINDOW = 500` bounded rescan existed only because a timestamp cannot locate a position in the log — it capped `before` paging to the newest 500 messages. An id cursor locates the position exactly, so the window, the ISO parsing (`_parse_before`), and the `created_at <` comparison are all gone. There is no timestamp comparison left on the paging path.

  Matches the spec change in #279 and the Rust reference. Tests cover round-trip paging to exhaustion, the identical-`created_at` collision case a timestamp cursor provably cannot survive (the bug the Go server shipped), and the unknown-cursor error.

### Patch Changes

- 075d6e4: Go: commit the type-generation command as `scripts/generate-go.sh` and regenerate `go/protocol/types_gen.go`.

  The command that produced `go/protocol/types_gen.go` was never committed — `go/README.md` deferred to "the original spec" — so Go was the one language whose wire types could not be regenerated. It is now a runnable script, verified to reproduce the previously committed file byte-for-byte from the spec at the commit that last generated it.

  Regenerating picked up everything Go had missed since: `get_messages` now takes an opaque `Cursor *string` (replacing `Before *time.Time`) and returns `NextCursor`, plus the `stream_reasoning` / `stream_preamble` / `cancel` events and the rich-interaction types.

## 1.29.0

### Minor Changes

- 441d198: Go server: page `get_conversation_messages` by opaque id cursor instead of an ISO timestamp.

  The request field `before` (ISO 8601) is replaced by `cursor` (opaque, a message id today), and the response now carries `nextCursor` — the id of the oldest message in the page, non-null exactly when `hasMore` is true. Breaking wire change.

  This removes the failure mode rather than renaming it: a timestamp cursor cannot separate two messages that share a timestamp, so `created_at <` paging silently dropped every message colliding on the cursor's instant. An id names exactly one row. The paging path now contains no timestamp comparison, and the bounded 500-message rescan the timestamp cursor required is gone — an id cursor locates its position in the log directly, so paging has no depth ceiling. `createdAt` is still returned (RFC3339Nano) for display.

  An unknown or stale cursor now returns a `VALIDATION_ERROR` instead of a silent empty page.

- bd836c3: TypeScript server: `get_conversation_messages` now pages on an opaque `cursor` (a message id) instead of the `before` ISO-timestamp cursor, and returns `nextCursor` alongside `hasMore`.

  Breaking wire change. A timestamp cursor cannot page a log correctly — two messages can share a `createdAt` at any precision the wire keeps, so a `createdAt < cursor` filter drops or repeats the messages that collide. The cursor now names exactly one message: the page starts immediately after it, on the older side. `nextCursor` is the oldest message in the page, non-null exactly when `hasMore` is true; an unknown cursor is a `VALIDATION_ERROR` rather than a silent empty page.

  This also removes the 500-message bounded rescan the timestamp cursor required, so paging is no longer capped at the newest 500 messages. `createdAt` stays on every message for display.

## 1.28.0

### Minor Changes

- b135852: Protocol: `get_conversation_messages` pagination moves from a timestamp cursor to an opaque cursor.

  `before` (ISO 8601 timestamp) is replaced by `cursor` (opaque, storage-defined — a message id today), and the response gains `nextCursor`, non-null exactly when `hasMore` is true. Page by feeding `nextCursor` back as the next request's `cursor`.

  A timestamp is the wrong cursor: two messages can share a timestamp at any precision the wire format preserves, so a `created_at < cursor` filter either drops or repeats the messages that collide. This is not hypothetical — the Go server shipped whole-second `RFC3339` and silently dropped every message sharing a second from page two. An id names exactly one message and cannot collide. The Rust server already paginated this way; this makes the spec match the design that was already correct.

  Breaking on paper, inert in practice: a survey of every consumer (smooai, smooth, heypage) found no caller that pages — all are single-fetch, none passes `before`, none reads `hasMore` or `nextCursor`.

  Also regenerates all client type sets from `spec/`, which had drifted badly. The regen pulls in schema changes that landed without regeneration (`cancel`, `submit_interaction`, `interaction_required`/`interaction_invalid`) and surfaces a latent bug: `organizationId` became required on `Session` in spec PR #97, but Python's model was never regenerated, so the Python client has been accepting sessions a conformant server would reject ever since.

## 1.27.7

### Patch Changes

- 95524bc: Python server: regression test pinning sub-second precision on `get_conversation_messages`' `createdAt`. The handler already emits full microsecond precision (`datetime.isoformat()` on a tz-aware UTC value), but nothing guarded it — clients page by handing the oldest `createdAt` back as `before`, and a second-truncated cursor makes the strict `<` filter drop every message sharing that second. Matches the Go (#264) and TypeScript (#273) fixes.
- d730dac: TypeScript server: user-initiated turn cancellation (the "Stop button"), mirroring the Rust reference (PR #259). A client stops the in-flight turn with `{"action":"cancel","requestId":"<the send_message requestId>"}`; the server aborts that turn and emits a terminal `cancelled` event (`status: 499`, requestId echoed at the envelope level and inside `data`) **in place of** the `eventual_response` — so a turn always emits exactly one terminal event. A cancel with no active turn is a silent no-op. Only ONE turn runs per connection: a second `send_message` while one is in flight is rejected with error code `TURN_IN_PROGRESS` rather than run concurrently (`confirm_tool_action` / `verify_otp` are turn _resumes_, so they're unaffected). A cancelled turn's partial assistant reply is DISCARDED (never persisted); the user's message, persisted at the start of the turn, stays. A client disconnect mid-turn now also aborts the turn, while the graceful SIGTERM drain still lets an in-flight turn finish.

  Implementation is connection-local, matching the Rust approach: the turn is already spawned as a background task (so the reader stays free to receive `confirm_tool_action` while a turn is parked), so the dispatcher tracks it as the connection's single active turn along with a per-turn `AbortController`, and fires it on cancel/disconnect. Cancellation is cooperative — JS can't drop an in-flight `await` the way tokio drops a future — so a turn parked inside a long tool call stops at the next stream event; the observable protocol contract is identical either way.

## 1.27.6

### Patch Changes

- 91078ac: TypeScript server: regression test pinning sub-second `createdAt` precision on `get_conversation_messages`. A server that formats `createdAt` at whole-second precision breaks the documented paging loop — clients feed page one's oldest `createdAt` back as `before`, and a strictly-less-than filter against a truncated cursor silently drops every message sharing that second. The TS server was already correct (`Date#toISOString`, millisecond precision, passed through unreformatted); the test locks it in.

## 1.27.5

### Patch Changes

- ac1da05: Go server: implement turn cancellation — the `cancel` action (the "Stop button"), porting the Rust reference. A connection now runs at most ONE agent turn at a time: `send_message` registers its turn with a cancellable context, a `cancel` frame cancels it and emits the terminal `cancelled` event (`status: 499`, echoing the cancelled turn's `requestId` at the envelope level and inside `data`), and a second `send_message` while a turn is in flight is rejected with `TURN_IN_PROGRESS` rather than run concurrently. A cancel with no active turn is a silent no-op. A cancelled turn discards its partial assistant message (never persisted) and emits no `eventual_response`; the user's message stays persisted. A client disconnect mid-turn aborts the turn the same way, while the SIGTERM graceful-drain path still lets an in-flight turn finish.

## 1.27.4

### Patch Changes

- b910a11: Python server: implement the `get_conversation_messages` action. It previously fell through to `UNSUPPORTED_ACTION`, so a web client resuming a conversation against the Python server rendered no history. The handler mirrors the merged Go/Rust reference and the `spec/actions/get-messages.schema.json` contract: newest-first `messages` (id, direction, content.text, createdAt) plus `hasMore`, with `limit` (1..100, default 50) and an optional ISO 8601 `before` cursor. `StoredMessage` gains a defaulted `created_at` timestamp to back the `createdAt` field and the cursor.
- c64b97b: TypeScript server: implement the `get_conversation_messages` action. Its dispatcher switch stopped at `verify_otp`, so the action fell through to `UNSUPPORTED_ACTION` and a web client resuming a conversation against the TS server rendered no history. The handler mirrors the merged Go/Rust references and the `spec/actions/get-messages.schema.json` contract: newest-first `messages` (id, direction, content.text, createdAt) plus `hasMore`, with `limit` (1..100, default 50) and an optional ISO 8601 `before` cursor. `StoredMessage` gains an optional `createdAt` timestamp (set by `InMemorySessionStore.appendMessage`) to back the `createdAt` field and the cursor.

## 1.27.3

### Patch Changes

- 0e65c59: Go server: emit `createdAt` with sub-second precision (`RFC3339Nano`) from `get_conversation_messages`. Clients page by handing the oldest `createdAt` back as `before`, which is filtered strictly-less-than against the store's full-precision timestamp — whole-second `RFC3339` truncation put the cursor _before_ the message it named, so every message sharing that second silently vanished from page two. Also aligns the Go wire format with the .NET server, which already round-trips full precision.

## 1.27.2

### Patch Changes

- e5bb69c: .NET server: implement the `get_conversation_messages` action

  The .NET `FrameDispatcher` answered `UNSUPPORTED_ACTION` for
  `get_conversation_messages`, so a C#-hosted server couldn't page conversation
  history the way the Rust/Go/TS servers can — a client resuming a conversation
  had no way to load prior messages. It now returns `{messages, hasMore}`
  newest-first per `spec/actions/get-messages.schema.json`, with `limit` (1–100,
  default 50) and an optional ISO 8601 `before` cursor.

  `StoredMessage` gained a `CreatedAt` init-only property (not a positional
  parameter, so downstream `ISessionStore` implementations keep compiling) that
  the Postgres store now reads from — and returns on append via — the existing
  `conversation_messages.created_at` column.

## 1.27.1

### Patch Changes

- d6c63d7: Go server: implement the `get_conversation_messages` action. It previously fell through to `UNSUPPORTED_ACTION`, so a web client resuming a conversation against the Go server rendered no history. The handler mirrors the Rust reference and the `spec/actions/get-messages.schema.json` contract: newest-first `messages` (id, direction, content.text, createdAt) plus `hasMore`, with `limit` (1..100, default 50) and an optional ISO 8601 `before` cursor. `StoredMessage` gains a `CreatedAt` timestamp to back the `createdAt` field and the cursor.

## 1.27.0

### Minor Changes

- 1765f6e: Add a built-in ACL-scoped `knowledge_search` tool to the .NET server. Registering an `IAccessKnowledge` already grounds turns via RAG auto-context; this exposes the same store as a model-callable tool a host enables by name (`knowledge_search`) — no hand-wrapped `AIFunction` required. It's built per-turn over the connection's `IAccessKnowledge.ForAccess(access)` handle, so every search is document-level access-controlled (a doc outside the caller's ACL is never a candidate), and matches the Rust server's tool for parity: same name, args (`query` required + `limit` clamped 1..10, default 3), and text result shape.
- 508de9d: dotnet: add a Notion `IConnector` (`NotionConnector`) to the server. Recurses `blocks/{id}/children` (paginated, `Notion-Version: 2022-06-28`, integration-token auth), flattens `paragraph`/`heading_1-3`/`bulleted_list_item`/`numbered_list_item`/`quote`/`code`/`toggle` rich_text (plus nested toggle/list-item bodies) into document text, and emits a `child_page` block as its own recursed document rather than inlining it. The document id is the canonical Notion page id and the source is the page URL, so citations link back and re-ingesting overwrites in place. Each configured `NotionRoot` carries a `DocumentAcl`, stamped onto every document under that root (`SourceDocument` gains an optional `Acl`).

### Patch Changes

- c6f202b: dotnet server: TurnRunner degrades gracefully when knowledge retrieval fails. When the embedding gateway / vector store is down, `QueryAsync` used to propagate out of the turn and the dispatcher surfaced `INTERNAL_ERROR`, killing the whole turn. Now the retrieval failure is caught: the turn proceeds with empty grounding (no citations, and the failing store isn't handed to the engine's own RAG query), and a warning is logged. Only the retrieval is wrapped — the rest of the turn is unchanged.

## 1.26.0

### Minor Changes

- 798f447: Per-agent write-confirmation (HITL) patterns. `AgentConfig` gains a
  `ConfirmToolPatterns` field so a multi-agent host can gate tools behind a
  `confirm_tool_action` round-trip per agent instead of sharing the single global
  `ConfirmTools` DI singleton. The dispatcher uses the per-agent patterns when the
  agent specifies them (an explicit empty list disables gating for that agent) and
  falls back to the global `ConfirmTools` when it doesn't — fully backward
  compatible.

### Patch Changes

- 8a0eae9: .NET ingestion parity: paragraph-aware chunker + content-hash IngestLedger

  Bring the .NET `Chunker` to parity with the Rust ingestion chunker — ~500-char
  paragraph-aware chunks (blank-line units, oversized paragraphs hard-split on word
  boundaries, greedy packing) with 64-char whole-word trailing overlap and stable
  `{documentId}#{index}` chunk ids (replacing the old whitespace-break 1200/150
  sliding-window splitter). Add a new `IngestLedger` with FNV-1a content-hash
  idempotency (byte-identical to Rust's `content_hash`) so re-ingesting identical
  content is a no-op while changed content is reprocessed; wire it through
  `IngestPipeline` (skips unchanged documents, dedupes identical chunks).

## 1.25.0

### Minor Changes

- a69d091: Add a .NET Slack `IConnector` (`SlackConnector`) for knowledge ingestion. Resolves author names
  via `users.list`, lists channels via `conversations.list`, and pages messages via
  `conversations.history`. Emits one document per channel per day with a stable id
  `slack:{channel}:{date}` (today re-hashes as messages land, past days dedupe on the pipeline's
  (id, hash) key), `source` = the day's first-message permalink (`chat.getPermalink`), incremental
  pulls via an `oldest` cursor, and a per-channel ACL label. `SourceDocument` gains an optional
  `Acl` field to carry per-document access labels (mirrors the Rust `RawDocument.acl`). Threaded
  replies (`conversations.replies`) are deferred to a follow-up.

### Patch Changes

- aa72bb0: Make the two .NET Server add-on packages publishable to NuGet and bump the Core pin. `SmooAI.SmoothOperator.Server.AspNetCore` (the ASP.NET Core WebSocket host) and `SmooAI.SmoothOperator.Server.Postgres` (the durable Postgres session store) now carry NuGet packaging metadata, get their `<Version>` stamped in lockstep by `sync-versions.mjs`, and are packed + pushed by `ci-publish.mjs` alongside the base `SmooAI.SmoothOperator.Server` package — so downstream hosts can `PackageReference` them instead of vendoring the extension source. The Server package's `SmooAI.SmoothOperator.Core` pin is also bumped from 1.5.0 to the latest published 1.7.0.

## 1.24.0

### Minor Changes

- 14070ec: Add a host-callable seam to start an agent turn server-side (`IServerInitiatedTurns`, registered by `AddSmoothOperatorServer`). A host — e.g. `POST /webhooks/datadog` saying "investigate this alert" — can now create a conversation and run a turn without a client `send_message` frame. It reuses the same `TurnRunner` + `ISessionStore` path as the client flow, so the inbound message and streamed reply persist identically: a client that later lists or resumes that conversation sees it the same as a client-initiated one. Interactive per-connection concerns (write-confirmation HITL, OTP gating) are intentionally omitted. Live push to already-connected sockets is deferred — the durable message log is the surface clients read.

## 1.23.4

### Patch Changes

- 607f81d: docs: refresh the .NET server docs to match the shipped 1.23.x surface. `dotnet/server/README.md`'s "What's shipped/Next" list and `docs/Architecture/Polyglot Cores.md`'s service-layer intro both lagged the published dll — knowledge grounding, ACL-filtered retrieval, citations, the reranker, GitHub ingestion + connectors, HITL write-confirmation, the `/admin/*` API, and the deployable host all ship in C# now. Corrected the stale "not yet built in C#" framing and marked the genuinely-open items (Notion/Slack connectors in-flight, checkpoint-adapter resume wiring).

## 1.23.3

### Patch Changes

- 7a53f95: Docs: add branded, NuGet-page READMEs for `SmooAI.SmoothOperator.Server.AspNetCore`
  and `SmooAI.SmoothOperator.Server.Postgres`. Each explains what the package is,
  how to install and use it (real API surface — `AddSmoothOperatorServer` /
  `MapSmoothOperatorWebSocket` / `ConfirmTools`; `PostgresSessionStore` /
  `PostgresAclKnowledgeStore`), and cross-references the rest of the .NET family
  (Core, Server, AspNetCore, Postgres, client). Wired each via `PackageReadmeFile`
  so it renders on nuget.org once the packages are published.

## 1.23.2

### Patch Changes

- 4b2b5d7: Conversation-workflow adherence (th-d57a1d): the rendered `<ConversationWorkflow>` step section now instructs the agent to ask the current step's question directly and never re-ask for permission / re-confirm readiness / repeat an answered question (gpt-oss-class models over-indexed on the old "you don't have to force the step to close" line and looped on re-confirmation). The workflow judge now counts brief/terse answers that address the step ("a four", "sure") as satisfying it instead of holding out for elaboration. Same wording change applied across all five language servers (TS, Rust, Python, Go, .NET).

## 1.23.1

### Patch Changes

- b60234e: Wire Changesets to drive lockstep publishing for every polyglot server artifact — npm + NuGet + PyPI + crates.io — closing the npm-only gap.

  - `scripts/sync-versions.mjs` now also stamps the .NET server package (`SmooAI.SmoothOperator.Server.csproj` `<Version>`) and the PyPI server package (`python/server/pyproject.toml`), and fails loudly if any manifest anchor is missing (never publishes an out-of-lockstep set).
  - New `scripts/ci-publish.mjs`: a single idempotent orchestrator that runs sync-versions first, then publishes npm → NuGet → PyPI (client + server) → crates.io, each existence-checked + skip-if-already-published, with a `DRY_RUN=1` path that packs/validates but uploads nothing. One registry's failure no longer skips the others; any hard failure exits non-zero. `ci:publish` now points at it.
  - `release.yml` folds the previously-inline crates.io/PyPI steps into `ci:publish` and adds the NuGet publish token, so the whole polyglot release goes through one orchestrator.

- b60234e: Docs: elevate the server + registry-landing READMEs into a narrative story. Root
  README gets a sharper problem→vision hook, a "safe by construction" section
  (ToolHook auth-gate + per-agent allow-list + document ACLs + SEP allowlist), and
  a clean language→client→server→registry table. Each per-language server README
  (Rust crates.io crate, TypeScript, Python, Go, .NET) now leads with a hook, a
  "spin up a real agent server in N lines" snippet, an honest "extending via
  tools + guardrails" example in that language's real API, badges, and the polyglot
  table. No code changes; accuracy verified against the shipped surface.

## 1.23.0

### Minor Changes

- d3d3abe: Two additive SEP-protocol enhancements on the streaming path (directive nav + business-card images), both optional and back-compatible.

  **Directive-over-SEP.** `eventual_response` gains an optional `directive` field — an opaque client-side directive (e.g. a Navigate / ApplyView instruction) a host tool emitted this turn. The runner threads a `directive_sink` into the `ToolProviderContext` (new `with_directive_sink` builder), drains it after the turn (last-write-wins, mirroring the citation sink), and carries the value onto `TurnResult::directive`. The protocol layer never interprets the shape — the host client owns it, exactly like `response`. Absent when no host tool wrote one, so the event is byte-for-byte unchanged for existing clients. Added to `spec/events/eventual-response.schema.json` and `spec/actions/send-message.schema.json` `$defs/Response`, and to the TypeScript SDK.

  **Image-through-SEP.** `send_message` gains an optional `images` array (`{ url, detail? }`) for multimodal turns. A new facade `UserImage` type flows from the inbound request into `TurnRequest::images` and the `ToolProviderContext` (new `with_images` builder); when non-empty the runner maps each onto a core `ImageContent` and attaches them to the engine's user message via `AgentConfig::with_user_images` (requires core `0.16.2`). Parsing is fail-soft (a malformed `images` entry is dropped, never rejects the turn). Empty/absent ⇒ a text-only turn, unchanged. Added to `spec/actions/send-message.schema.json` `$defs/Request` and the TypeScript SDK.

## 1.22.17

### Patch Changes

- 57c7a02: Add an optional fast-model **preamble** to streaming turns to cover the reasoning model's time-to-first-token. When the server is configured with `SMOOTH_AGENT_PREAMBLE_MODEL` (e.g. `groq-gpt-oss-20b`), a small fast model runs IN PARALLEL with the main turn and streams ONE short present-tense "what I'm about to do" sentence over a new `stream_preamble` wire event — an ephemeral status line the real answer replaces. It's best-effort (any error/slowness is swallowed on its own task) and guarded: it's dropped if the real answer has already begun streaming, so it can never block or corrupt a turn. Unset ⇒ no extra call and byte-for-byte unchanged behavior. Adds `stream-preamble.schema.json` to the SEP spec and `StreamPreamble` to the TypeScript SDK union.

## 1.22.16

### Patch Changes

- 33a92bd: Persist conversation-workflow step state to shared storage (th-c12df5). The step pointer (`currentStepId`) and per-step attempt counter were held in the per-pod in-memory session map, so on a widget reconnect or a pod hop they reset to step 0 — the workflow froze on its first step, the judge/attempt-cap could never advance it, and any per-step rich elements (quick-reply chips today, richer message elements later) were pinned to that first step. They now live on the conversation's `metadata_json` (shared storage, keyed by the stable `conversation_id`) and load per turn, so a workflow resumes on the right step across reconnects and replicas. Element-agnostic — the fix moves the step pointer, not the emitted content.

## 1.22.15

### Patch Changes

- fa6d913: Deterministic workflow chips (th-d57a1d). `ConversationWorkflowStep` gains an optional `suggestedReplies: string[]`; when the agent is on a step that declares it, the server emits those canonical answers as the response's `suggestedNextActions`, overriding any model-invented chips. This makes quick-reply chips fire on every such step (reliable, not model-dependent) and — because a tapped chip is clean, canonical input — fixes the assessment stalling where the judge would not advance on terse free-text answers. Free-form steps declare none, leaving model behavior unchanged.

## 1.22.14

### Patch Changes

- 2476916: Add a per-step attempt cap to the conversation-workflow judge so a guided assessment can't stall forever on one step. The judge only advances on `yes`; when a step's criteria demand evidence the judge never accepts, the step re-asks indefinitely and a multi-step flow (e.g. the public Transformation Posture agent) never reaches its scoring / lead-capture step (th-d57a1d). The step pointer already persists and advances correctly — this adds the missing escape hatch: `apply_step_cap` force-advances to the next step after `WORKFLOW_STEP_ATTEMPT_CAP` (3) consecutive non-advancing turns, resetting the counter on any advance. The counter persists in session metadata (`stepAttempts`) alongside the existing `currentStepId` pointer. With tuned criteria the cap rarely fires — it's the safety net for a pathological non-answering visitor.

## 1.22.13

### Patch Changes

- 98e7c06: Server: deterministic backstop against a degenerate LLM repetition loop spamming
  the chat widget. `general_agent_response` now collapses runaway near-identical
  filler in the finalized reply — splits on paragraph breaks, drops paragraphs
  near-identical to one already kept, and caps the count — before it reaches the
  widget. A healthy reply is returned byte-for-byte unchanged.

## 1.22.12

### Patch Changes

- Harden chat streaming + fix gpt-oss suggested-reply chips.

  - `chat_stream` now retries retryable HTTP statuses (429/5xx) before reading any
    stream bytes, mirroring the non-streaming `chat()` path. A transient gateway
    5xx (groq/LiteLLM 502/503) previously propagated as an `AGENT_ERROR` and the
    chat widget rendered an empty reply. Bumps the core dep to 0.16.2 (where the
    retry lives).
  - `extract_suggested_replies` now also parses a trailing markdown
    `Suggested replies:` list, so models that ignore the `<suggested_replies>`
    marker (gpt-oss-120b) still populate chips.

## 1.22.11

### Patch Changes

- fix(rust): require core `^0.16.1` so `with_model_ceiling` resolves from crates.io

  Server 1.22.10 calls `AgentConfig::with_model_ceiling` / `LlmClient::with_model_ceiling`,
  but its published manifest required core `^0.16` — and crates.io topped out at core
  **0.16.0**, which predates those methods. So any external `cargo build` against the
  published server resolved the broken 0.16.0 and failed to compile (the chips/empty-reply
  reasoning-channel fix was un-buildable off crates.io). Core **0.16.1** is now published
  with the API; pin the floor at `0.16.1` and drop the stopgap `git`/`rev` pin so the
  published server resolves the fixed engine.

## 1.22.10

### Patch Changes

- 22b193e: Fix `eventual_response` still shipping an empty reply (blank `responseParts` + empty `suggestedNextActions`) on gpt-oss-120b via the LiteLLM/groq gateway, which 1.22.1 did not cover.

  Confirmed empirically against the real SSE parser: this gateway/model emits the WHOLE answer on the reasoning channel (`delta.reasoning_content`) with `delta.content` never populated. The engine accumulates reasoning into a separate buffer and drops it from `response.content`, so BOTH `last_assistant_content()` and the 1.22.1 `streamed_reply` (content tokens) come back empty — even though the answer streams to the client as `stream_reasoning` and persists. The "streamed tokens" observed in prod were `stream_reasoning` frames (protocol-identical to `stream_token`), not content.

  `rust/smooth-operator-server/src/runner.rs`: accumulate the turn's reasoning stream and use it as a LAST-RESORT fallback for the final reply — after `last_assistant_content()` and `streamed_reply`, only when no answer content exists anywhere. A normal reasoning model always populates `content`, so it never surfaces its thinking as the answer; this rung fires solely for the degenerate answer-in-reasoning case where the alternative is an empty response. The suggested-replies trailer is preserved through the fallback so suggestions are recovered.

  Adds `tests/gateway_wire_empty_reply.rs`, a regression that drives the real `LlmClient` against a local mock speaking the gateway SSE wire format (answer-in-content and answer-in-reasoning shapes) — it fails if the reply goes empty again.

## 1.22.9

### Patch Changes

- 01c434e: Fix auto-title producing empty titles. The auto-title model (`groq-gpt-oss-20b`) is a reasoning model whose reasoning tokens count against `max_tokens`, so the original 32-token cap was fully consumed by reasoning and left the completion content empty — the titler then silently kept the default `Session <uuid>` name. Raise the auto-title budget to 512 (the title itself is still capped to `TITLE_MAX` chars by `sanitize_title`), extract `title_request_body` so the budget is unit-tested, and add tracing at each auto-title bail point (debug for the expected "already named" skip, warn for real failures).

## 1.22.8

### Patch Changes

- 6e994ad: SDK: `SmoothAgentClient.listConversations()` + `conversationId` resume typing — the client surface for a conversation sidebar (pearl th-2f028f).

  - New `listConversations({ limit? })` method wrapping the server's `list_conversations` action; resolves to `{ conversations: [{ conversationId, title, updatedAt, messageCount }] }` (most-recent-first). Exports `ConversationSummary` / `ListConversationsResponse`.
  - `createConversationSession` now accepts an optional `conversationId` (already honored by the server) to RESUME an existing conversation; pair it with `getMessages` to load the transcript.
  - Additive and back-compat.

  Also adds `examples/web-chat` — a private, runnable Vite + React reference chat client built on this SDK (token streaming, inline tool-call/result blocks, HITL approvals, conversation sidebar, oldest-first history). Not published.

## 1.22.7

### Patch Changes

- 487d10b: Rust server: conversation auto-title (small model) + `rename_conversation` (pearl th-d5b446).

  - **Auto-title** — after the first assistant turn on a conversation still carrying its default `Session <uuid>` name, a best-effort, detached, non-blocking task asks the fast/cheap `groq-gpt-oss-20b` model for a short 3-6 word title over the first exchange and stores it as the conversation `name`. Fail-safe: any error (no gateway key, gateway failure, empty output, storage error) simply leaves the default name — a turn is never slowed or broken. The default-name guard (re-checked right before the write) means a manual rename is never clobbered, and a titled conversation won't re-fire.
  - **`rename_conversation`** — new WS action `{action, requestId, conversationId, title}`: sanitizes/trims the title (rejects empty), 404s an unknown conversation, persists `name` via the storage adapter's existing `update_conversation`, and replies `immediate_response` (200) with `{ conversationId, title }`.
  - `list_conversations` now surfaces a **meaningful** conversation `name` (auto-title or manual rename — anything not the default `Session <uuid>`) as the sidebar title, falling back to the first-inbound message preview for un-titled conversations. Back-compat: every pre-titling conversation carried the default name, so the message-preview behavior is unchanged for them.

  Additive + back-compat. New tests cover title sanitization (quotes/markdown/whitespace/length), the default-name-only auto-title guard (mock gateway, never clobbers a manual name, no-key fail-safe), rename success + list surfacing, empty-title rejection, and unknown-id 404.

## 1.22.6

### Patch Changes

- 9b842d7: .NET server: conversation-history / resume substrate for the WS protocol (pearl th-d5b446) — C# parity with the merged Rust reference (and the Go/TS mirrors) so every client (daemon PWA, `th code` TUI, chat-widget) can build a conversation sidebar + resume against the .NET server too.

  - New WS action `list_conversations` (`{action, requestId, limit?}`, default limit 50): replies via `immediate_response` (200, message "Conversations") with `{ conversations: [ { conversationId, title, updatedAt, messageCount } ] }`, most-recent-first, filtered to conversations with `messageCount > 0` (drops the empty conversations every page-load mints). `title` = a ~60-char preview of the first inbound message with leading markdown/control chars stripped, falling back to a generic name; `updatedAt` = ISO-8601.
  - `create_conversation_session` gains an optional `conversationId`: when it names a known conversation, the new session RESUMES — reuses that conversation's id and keeps its message log, so `send_message` appends to it and the runner replays its history. Absent/unknown id ⇒ a fresh conversation is minted (byte-for-byte unchanged behavior).
  - Additive and back-compat: no `conversationId` / no `list_conversations` call = unchanged behavior. `ISessionStore` grows `ResumeSessionAsync` + `ListConversationsAsync` (+ a `ConversationSummary` record), implemented by both `InMemorySessionStore` (tracks per-conversation last-activity) and `PostgresSessionStore`; the shared `SessionStoreContractTests` cover both.

## 1.22.5

### Patch Changes

- b367240: Python server: `list_conversations` + resume-by-`conversationId` (pearl th-d5b446) — Python parity with the merged Rust/Go/TS reference so every client (daemon PWA, `th code` TUI, chat-widget) can build a conversation sidebar + resume against the Python server too.

  - New WS action `list_conversations` (`{action, requestId, limit?}`, default limit 50): replies via `immediate_response` (200, "Conversations") with `{ conversations: [ { conversationId, title, updatedAt, messageCount } ] }`, most-recent-first, filtered to conversations with `messageCount > 0` (drops the empty conversations every page-load mints). `title` = a ~60-char preview of the first inbound message with leading markdown/control chars stripped, falling back to a generic name; `updatedAt` = ISO-8601.
  - `create_conversation_session` gains an optional `conversationId`: when it names a known conversation, the new session RESUMES — reuses that conversation's id and keeps its message log, so `send_message` appends to it and the runner replays its history. Absent/unknown id ⇒ a fresh conversation is minted (unchanged behavior).
  - Additive and back-compat: no `conversationId` / no `list_conversations` call = unchanged behavior. `SessionStore` gains `list_conversations()` + an optional `conversation_id` arg on `create_session`; the in-memory store tracks per-conversation last-activity for the sort key.

## 1.22.4

### Patch Changes

- 9ba82d1: Go server: conversation-history / resume substrate for the WS protocol (pearl th-d5b446) — Go parity with the merged Rust reference so every client (daemon PWA, `th code` TUI, chat-widget) can build a conversation sidebar + resume against the Go server too.

  - New WS action `list_conversations` (`{action, requestId, limit?}`, default limit 50): replies via `immediate_response` (200, message "Conversations") with `{ conversations: [ { conversationId, title, updatedAt, messageCount } ] }`, most-recent-first, filtered to conversations with `messageCount > 0` (drops the empty conversations every page-load mints). `title` = a ~60-char preview of the first inbound message with leading markdown/control chars stripped, falling back to a generic name; `updatedAt` = ISO-8601 (RFC 3339).
  - `create_conversation_session` gains an optional `conversationId`: when it names a known conversation, the new session RESUMES — reuses that conversation's id and keeps its message log, so `send_message` appends to it and the runner replays its history. Absent/unknown id ⇒ a fresh conversation is minted (byte-for-byte unchanged behavior).
  - Additive and back-compat: no `conversationId` / no `list_conversations` call = unchanged behavior. `go/server/{dispatcher,session_store}.go` only. In-memory store tracks per-conversation last-activity for the sort key.

## 1.22.3

### Patch Changes

- 1644852: Rust server: conversation-history / resume substrate for the WS protocol (pearl th-d5b446) — the contract every client (daemon PWA, `th code` TUI, chat-widget) builds a conversation sidebar + resume against.

  - New WS action `list_conversations` (`{action, requestId, limit?}`, default limit 50): replies via `immediate_response` (200) with `{ conversations: [ { conversationId, title, updatedAt, messageCount } ] }`, most-recent-first, filtered to conversations with `messageCount > 0` (drops the empty conversations every page-load mints). `title` = a ~60-char preview of the first inbound message, falling back to the conversation `name`; `updatedAt` = ISO-8601.
  - `create_conversation_session` gains an optional `conversationId`: when it names an existing conversation, the new session RESUMES — reuses that conversation's id + org and skips `create_conversation`, so `send_message` appends to it and the runner replays its history via `thread_id`. Absent/unknown id ⇒ a fresh conversation is minted (byte-for-byte unchanged behavior).
  - Additive and back-compat: no `conversationId` / no `list_conversations` call = unchanged behavior. `handler.rs` only.

## 1.22.2

### Patch Changes

- 6306e36: TypeScript server: model-output ceiling clamp + raised starvation-prone defaults (EPIC th-1cc9fa), matching the Rust/Python server reference.

  - `typescript/server/src/modelCeiling.ts`: best-effort per-model output ceiling from the gateway's `/model/info` (`extractModelCeilings` + `createGatewayModelCeilingResolver`), cached once per process, `undefined` on any error ⇒ engine leaves `max_tokens` unclamped.
  - `turnRunner.ts`: raise `DEFAULT_MAX_TOKENS` 512→8192 and `DEFAULT_MAX_ITERATIONS` 6→20 (chat-widget sizing starved reasoning models), thread the per-turn ceiling into the engine via `AgentOptions.modelMaxOutput`, and set an explicit `DEFAULT_MODEL` shared by the request and the ceiling lookup.
  - Thread `model` + `modelCeiling` through `FrameDispatcher`, `ServerOptions`, `serveLocal`; `main.ts` builds the resolver from `SMOOAI_GATEWAY_URL`/`KEY` (undefined on the keyless local path ⇒ unclamped, behaviour unchanged).
  - Bump `@smooai/smooth-operator-core` pin to `^0.20.4` (the published release introducing `modelMaxOutput` / `effectiveMaxTokens`).

## 1.22.1

### Patch Changes

- 17e1ad9: Fix intermittently empty `eventual_response` on the streaming turn (blank `responseParts` + dropped `suggestedNextActions`) even though the full reply streamed and persisted.

  The runner sourced the final reply from `Conversation::last_assistant_content()`. On reasoning models (e.g. `groq-gpt-oss-120b`) a turn can end on a tool-call or reasoning-only assistant entry whose `content` is empty, so that returned `""` — shipping an empty `eventual_response` and losing the parsed suggestions.

  `rust/smooth-operator-server/src/runner.rs`: accumulate THIS turn's raw streamed answer tokens (pre-suppressor, reasoning excluded — identical to the engine's assistant `content`) and fall back to it when `last_assistant_content()` is empty. The suggested-replies trailer is preserved in the fallback so `extract_suggested_replies` strips it and recovers the suggestions exactly as on the normal path. The non-empty path is byte-for-byte unchanged.

## 1.22.0

### Minor Changes

- 998e270: SMOODEV-2172 — per-agent `model` and `max_iterations` overrides. `AgentBehaviorConfig`
  now carries `model: Option<String>` (per-agent gateway model id) and
  `max_iterations: Option<u32>` (per-agent agent-loop cap), parsed from optional
  `agents.model` (text) and `agents.max_iterations` (integer) row values. Blank models
  are ignored; `max_iterations` is clamped to `1..=64` with a `warn` on clamp.

  At turn time the operator server threads both through: the model resolves highest-wins
  as per-turn `send_message.model` (Smooth Modes) → per-agent `agents.model` →
  `SMOOTH_AGENT_MODEL`; the loop cap resolves per-agent `agents.max_iterations` →
  `SMOOTH_AGENT_MAX_ITERATIONS`. `None` at every layer falls back to the global env
  default exactly as before, so a standalone deploy is byte-for-byte unchanged. The
  reference Postgres adapter reads both columns tolerantly — a DB predating them degrades
  to the global default (no migration-ordering dependency).

## 1.21.4

### Patch Changes

- 2d2ab24: Consume `smooai-smooth-operator-core` from crates.io (0.16) instead of the sibling
  path dep, and collapse the image build to a single-repo Docker context.

  - `rust/Cargo.toml`: `smooai-smooth-operator-core` path dep → `"0.16"` (published crate).
  - `Dockerfile`: drop the sibling `smooth-operator-core` COPY; context is this repo alone (cargo fetches the engine crate from crates.io).
  - `deploy/scripts/kind-smoke.sh`: build from the repo root, drop `PARENT_DIR`/`SIBLING_DIR`.
  - `.github/workflows/pr-kind-deploy-smoke.yml`: drop the sibling checkout + `ref:` pin + `PARENT_DIR` env.

  `Cargo.lock` regen + `cargo build --locked` verification happen AFTER 0.16.0 is
  published to crates.io.

## 1.21.3

### Patch Changes

- 0da6007: SMOODEV-2328 — OpenTelemetry GenAI agent spans on the production streaming path.

  The reference server drives every real turn through `runner::run_streaming_turn`,
  which previously emitted **no** `gen_ai.*` spans (only the secondary
  `KnowledgeChatRuntime::run_turn` was instrumented). Both paths now emit the
  identical span shape so agent turns flow via OTLP to the observability studio:

  - Per-turn `gen_ai.chat` span now also carries `gen_ai.agent.name` and — on the
    streaming path — `smooai.org_id` (matching the monorepo TS chat handler's
    attribute exactly, so the studio groups Rust + TS turns by org), alongside the
    existing system / model / conversation.id and aggregated token usage.
  - Per-tool `gen_ai.tool` child span now carries the tool's `gen_ai.tool.call.arguments`
    (redacted via `telemetry::redact_tool_arguments`, which scrubs secret-named JSON
    keys and caps length) plus an `otel.status_code`=`ERROR` + message on failure,
    in addition to the existing tool name / latency / is_error.

  OTLP export was already wired end-to-end (`init_telemetry()` in both server and
  lambda `main.rs`, gated on `OTEL_EXPORTER_OTLP_ENDPOINT`). No per-LLM-call
  inference span yet — that needs `smooth-operator-core` to emit per-call usage +
  finish-reason, tracked separately.

## 1.21.2

### Patch Changes

- 25adb5c: th-6784a6 — sync to core@main + pin the CI core checkout so a moving core can't
  silently break every PR.

  `pr-kind-deploy-smoke.yml` checked out `SmooAI/smooth-operator-core` with no
  `ref`, so when core@main advanced (multimodal `Message.images` field), this
  repo's `main` stopped compiling against it and `cargo build --locked` failed the
  lock check — turning every open PR red for reasons unrelated to its own diff.

  - Add `images: vec![]` to the two `EngineMessage` constructions (replayed
    text-only history) in `runtime.rs` and `runner.rs`.
  - Fix stale test literals missing new struct fields: `suggested_replies.rs`
    (`identity_intake` → `interactions`, removed in #176) and `serve_smoke.rs`
    (`ServerConfig` + `TurnRequest` new fields).
  - Regenerate `Cargo.lock` against core@main so `--locked` passes.
  - Pin the CI core checkout to a known-good SHA
    (`3c7b21dbde4f31519b2eab3d5343f154119fe655`), documented as interim until
    core publishes to crates.io. Bump it deliberately alongside
    `cargo update -p smooai-smooth-operator-core`.

## 1.21.1

### Patch Changes

- 909443a: SMOODEV-2259 — per-agent SEP extension enablement: `AgentBehaviorConfig` now carries
  `enabled_extensions` (parsed from the `agents.extension_config` jsonb, camelCase
  `enabledExtensions[{extensionId, enabled, config}]`), and the operator server's extension
  host intersects the server allowlist (`SMOOTH_EXTENSIONS_ALLOW`) with the per-agent enabled
  extension ids.

  Fail-closed for resolved agents: any agent that resolves to a config (exists in the agents
  DB) but enables no extensions loads ZERO extensions, even when the server allowlist is
  non-empty — extensions can intercept & mutate tool calls, so a public agent must never
  silently inherit one. Backward-compatible when no per-agent config resolves at all
  (bare/standalone operator): the server allowlist alone decides, unchanged. The Postgres
  resolver now keys "no per-agent config" off row existence (not `is_empty()`), so a
  found-but-blank agent is distinguishable from an unknown one; the `extension_config` column
  read degrades to `None` on a standalone deploy whose table predates the column (no migration
  ordering dependency).

## 1.21.0

### Minor Changes

- 85e5643: Rich Interactions — generalize the just-shipped identity-intake seam into an extensible structured-interaction framework (`docs/Architecture/Rich Interactions.md`). One generic wire surface serves every interaction kind: `interaction_required` / `interaction_invalid` events + the single `submit_interaction` resume verb (with `interactionId` echo so stale submits can't resolve newer parks); per-kind precision lives in `spec/interactions/<kind>.schema.json` and the per-kind raise tools. Adding a kind (date picker, choice chips, file upload, …) = one `InteractionKind` impl (server-side validator + conversational-fallback directive + raise-tool schema) + a spec entry + a widget card — no new events, no client-library release. `identity_intake` (capability `identity_form`) ships as the first kind through the framework. Supersedes 1.19.0's typed `identity_intake_*` events (removed — zero external consumers). TypeScript client: regenerated types and the generic `submitInteraction()` verb (replaces `submitIdentityIntake()`).

## 1.20.0

### Minor Changes

- af9ac05: Suggested quick replies: the Rust server's `eventual_response` now carries live `suggestedNextActions` instead of a hardcoded empty array. The runner appends a machine-parsed trailer contract (`<suggested_replies>["…"]</suggested_replies>`) to every turn's system prompt, suppresses the trailer from the live token stream, strips it from the persisted/final reply, and surfaces the parsed suggestions (capped at 4) on `TurnResult.suggested_next_actions` and the `eventual_response` payload. `runner::general_agent_response` now takes the suggestions slice. Rust server only; other language servers still emit an empty array (parity follow-up).

## 1.19.0

### Minor Changes

- 3a9d29e: Identity intake — a channel-normalized lead/identity capture primitive (`docs/Architecture/Identity Intake.md`). New protocol surface: `supports` client-capability declaration on `create_conversation_session`, `identity_intake_required` / `identity_intake_invalid` events, and the `submit_identity_intake` resume action (with server-side validation: required fields, email shape, E.164 phone normalization). Rust reference implementation: `request_identity_intake` / `submit_identity_intake` agent tools in `smooai-smooth-operator` (park-and-resume on form-capable sessions; validated conversational turn-by-turn fallback on text-only channels — both resume with the same structured payload), server wiring (pending-intake registry, session identity attach onto the OTP contact keys) in `smooai-smooth-operator-server`. TypeScript client: regenerated spec types, `supports` on `createConversationSession`, and the `submitIdentityIntake()` resume verb. Parity for the TS/Python/Go/.NET servers is tracked as follow-ups; the spec + conformance fixtures are the complete contract.

## 1.18.0

### Minor Changes

- 21016e5: SEP Phase 8 (spec + SDK + demo) — long-tail pi parity.

  **Spec.** `initialize.schema.json` registrations gain `hooks` (declared intercept
  hooks, so the host can skip the per-turn `context` hook) and `message_renderers`
  (declarative `tag` → render-block templates). New `RenderBlock` `$def` — the
  render-block DSL (`markdown`/`keyvalue`/`table`/`diff`/`progress`/`stack` + the
  interactive `widget` kind with keybindings, each with a `text` fallback) — plus
  `MessageRendererRegistration`. `ui/request` `set_widget` documents its widget as a
  render block (kept permissive since SEP carries no cross-file `$ref`s). New
  conformance fixtures: `event_bus_fanout` (`bus/event`), `event_widget_key`
  (`widget/key`), `registrations_phase8` (hooks + message renderer), and
  `render_block_widget`.

  **SDK.** `render.*` builders for the render-block DSL; `smooth.events`
  (`publish`/`on`) for the inter-extension bus; `smooth.registerMessageRenderer(tag,
template)`; `ctx.ui.setWidget` now takes a typed `RenderBlock`; the `context` +
  `before_agent_start` hooks and `widget/key` events are exercised end-to-end.
  `buildRegistrations` emits `hooks` + `message_renderers`. `createTestHost` records
  `bus/publish` (`busPublishes`) and services it. New `eventName` constants
  (`BUS_EVENT`, `WIDGET_KEY`) and `method.BUS_PUBLISH`.

  **Demo.** `snake` — pi's game ported to the render-block v2 widget DSL: `play`
  pushes an interactive `widget` block; each `widget/key` advances a pure game core
  and re-renders. Full-fidelity on web, reduced-fidelity (ASCII grid + score) on the
  TUI, identical keybinding DSL.

  **Docs + scaffold.** `PORTING.md` — the pi → SEP parity checklist (every pi
  `ExtensionAPI` member → equivalent, port delta, or documented N/A). New `provider`
  scaffold template in `create-smooth-extension` (registers a provider; builds and
  tests green with a canned response, marked where the real call goes).

## 1.17.0

### Minor Changes

- f370ae9: SEP — the .NET operator server (`dotnet/server`) now hosts extensions (ui/confirm producer).

  The C# server wires the engine `ExtensionHost` (from `SmooAI.SmoothOperator.Core` 1.4.0)
  into each `send_message` turn. With `SMOOTH_EXTENSIONS_ALLOW` set (a default-deny allowlist —
  the server has no interactive trust prompt), `ExtensionServerHost.BuildAsync` discovers
  `extension.toml` extensions, spawns them as JSON-RPC/ndjson subprocesses, and exposes their
  tools. Those tools join the turn's tool set so they flow through the SAME per-agent
  `enabled_tools` filtering + auth gate as native tools (dotted `<ext>.<tool>` names match
  `toolId`), and the host is torn down (subprocesses killed) at turn end.

  An extension's `ui/confirm` bridges onto the operator protocol's
  `write_confirmation_required`/`confirm_tool_action` frames via `ConfirmUiProvider` — parking
  on the same session-keyed `ConfirmationRegistry` the native write-tool HITL uses. Every other
  `ui/*` degrades headless. Only the `confirm` capability is advertised at handshake.

  Additive: with the allowlist empty (the default) no host is ever built, so behavior is
  byte-for-byte unchanged. Verified by an integration test that runs the spec's Node echo peer
  through a real server turn and asserts `enabled_tools` filtering drops an extension tool
  exactly like a native one.

## 1.16.0

### Minor Changes

- 49bd798: SEP — the TypeScript operator server now hosts extensions (`ui/*` producer),
  mirroring the Rust reference (`rust/smooth-operator-server/src/extensions.rs`).

  `typescript/server` wires the engine `ExtensionHost`
  (`@smooai/smooth-operator-core/extension`) into each turn: with
  `SMOOTH_EXTENSIONS_ALLOW` set (a default-deny, comma-separated trust allow-list)
  it discovers `extension.toml` extensions, spawns them as JSON-RPC/ndjson
  subprocesses, and registers their `<ext>.<tool>` tools into the turn's tool set
  BEFORE the per-agent `enabled_tools` filter — so an allow-list drops them exactly
  like a built-in (SMOODEV-590 parity). A `ConfirmUiProvider` bridges an
  extension's `ui/confirm` onto the existing `write_confirmation_required` /
  `confirm_tool_action` frames via the session-keyed `ConfirmationRegistry`; every
  other `ui/*` degrades headless (render-only → `{}`, select/input → `{cancelled}`).
  The host and its subprocesses are torn down at turn end. Unset
  `SMOOTH_EXTENSIONS_ALLOW` (the default) builds no host — behavior is unchanged.

## 1.15.1

### Patch Changes

- 35806b2: Go server: host SEP extensions in a turn + ui/confirm bridge (th-829d9f).

  Wires the engine's SEP `ExtensionHost` (new in smooth-operator-core) into the Go
  operator server's send_message turn:

  - **Default-deny discovery** — `SMOOTH_EXTENSIONS_ALLOW` (comma-separated names)
    is the trust decision; empty (the default) builds no host, so behavior is
    byte-for-byte unchanged. Allowlisted `extension.toml` extensions are discovered
    (`SMOOTH_EXTENSIONS_DIR` or the engine default) and spawned per turn.
  - **Tool composition** — an extension's tools (`<ext>.<tool>`) are folded into the
    turn's tool set before the SMOODEV-590 `enabled_tools` / authLevel filter, so
    they gate exactly like a built-in tool.
  - **ui/confirm bridge** — `confirmUIProvider` projects an extension's `ui/confirm`
    onto the existing `write_confirmation_required` / `confirm_tool_action` frames via
    the per-connection confirmation registry; other `ui/*` degrade headless.

  Covered by an end-to-end test that drives a scripted model calling an
  extension-registered tool through the real WS/dispatcher turn (echo peer via a
  self-re-exec of the test binary), asserting execution and `enabled_tools` filtering
  parity, plus default-deny. Race-clean.

## 1.15.0

### Minor Changes

- b88d39c: Python server: host SEP extensions in a turn (ui/\* producer) — pearl th-66251a.

  Wires the engine's `ExtensionHost` (ported to the Python core in smooth-operator-core#33) into the Python operator server, the Python sibling of the Rust reference server wiring (#159). A turn can now host `extension.toml` extensions: their tools reach the agent and their `ui/confirm` bridges onto the chat-native confirmation frame.

  - **Trust — default deny.** `SMOOTH_EXTENSIONS_ALLOW` (comma-separated names) IS the trust decision; empty/unset (the default) means no extension is ever spawned and the host is never built, so behavior is byte-for-byte unchanged. `SMOOTH_EXTENSIONS_DIR` overrides the discovery dir.
  - **Tools + `enabled_tools` parity.** An allowlisted extension's eager tools are added to the turn's tool set and flow through the SAME per-agent `enabled_tools` filter (`filter_tools`, by tool name) the built-ins get — so an allow-list drops an extension tool (`echo.say`) exactly like a built-in.
  - **`ui/confirm` → the confirmation frame.** `ConfirmUiProvider` (a `HostDelegate`) projects an extension's `ui/confirm` onto the existing `write_confirmation_required` / `confirm_tool_action` frames via the same session-keyed `ConfirmationRegistry` the native write HITL uses; every other `ui/*` degrades headless (interactive → `{cancelled}`, render-only → `{}`). Only the `confirm` capability is advertised at handshake.
  - **Teardown.** The per-turn host is shut down (subprocesses stopped, parked confirmation cleared) at turn end.

  New module `smooth_operator_server.extensions` (`build_extension_host`, `ConfirmUiProvider`, `parse_allowlist`), wired into `turn_runner.py`. Integration tests drive a real echo-peer extension through a live `send_message` turn (tool runs + result streams back) and assert `enabled_tools` filtering parity, plus the `ui/confirm` bridge unit tests.

## 1.14.0

### Minor Changes

- be6b62f: SEP — the Rust operator server now hosts extensions (`ui/*` producer).

  The reference operator server (`smooth-operator-server`) wires the engine
  `ExtensionHost` into each turn: with `SMOOTH_EXTENSIONS_ALLOW` set (a default-deny
  allowlist — the server has no interactive trust prompt), it discovers
  `extension.toml` extensions, spawns them as JSON-RPC/ndjson subprocesses, and
  attaches the host to the agent. An extension's tools land in the turn's
  `ToolRegistry` and flow through the same per-agent `enabled_tools` filtering +
  authLevel gating as built-ins (SMOODEV-590), and its hooks/events run in the
  agent loop.

  `ui/confirm` is projected onto the existing `write_confirmation_required` /
  `confirm_tool_action` HITL frames — the same out-of-band bridge the native
  write-tool `ConfirmationHook` uses, so a hosted extension's confirm prompt pauses
  and resumes the turn end-to-end. Every other `ui/*` degrades headless (only the
  `confirm` capability is advertised at handshake). Unconfigured (empty allowlist),
  no host is built and behavior is byte-for-byte unchanged.

  This is the first operator server to host extensions. The other four polyglot
  servers (TypeScript, Python, Go, .NET) have the agent-loop + HITL landing pad
  wired but their engine cores have no SEP `ExtensionHost` yet — porting it to each
  engine is tracked as follow-up work.

## 1.13.0

### Minor Changes

- 70bd271: SEP Phase 7 (spec + SDK + demo) — registerProvider: declarative providers, OAuth,
  proxied streaming, and set_model.

  **Spec.** New `provider.schema.json` covering `provider/complete` (params +
  result), `provider/delta`, and `provider/oauth_login`/`oauth_refresh` (params +
  credentials). `initialize`/`registry-update` registrations gain `providers`
  (`ProviderRegistration` + `ProviderModel`); `session/set_model` params gain
  optional `provider` + `thinking`; `capabilities_enabled` gains `providers`. New
  conformance fixtures for every provider shape (valid + `$invalid`), replayed by
  both the TypeScript schema conformance test and the Rust host's vendored copy.

  **SDK.** `smooth.registerProvider(defineProvider({ name, models, complete,
oauthLogin?, oauthRefresh? }))` — the extension owns the request/stream, emitting
  `ctx.delta(event)` chunks while streaming. `session.setModel(model, { provider,
thinking })` completes the Phase 4 session surface. `createTestHost` gains
  `complete()` (with `onDelta`), `oauthLogin()`, `oauthRefresh()`, and routes
  `provider/delta` by `request_id` — the in-process mirror of the engine's
  `ProviderStreams`.

  **Demo.** `corporate-proxy` registers a provider that proxies an OpenAI-compatible
  endpoint: it streams the upstream SSE back as `provider/delta` chunks, maps
  tool-call responses, and mediates OAuth (login prompt over `ui/input`, token
  exchange). Exercised end-to-end in `provider-path.test.ts` against a real mock
  upstream serving scripted SSE.

## 1.12.0

### Minor Changes

- 7a05f00: SEP Phase 6 (chat-widget) — render agent confirmation prompts as chat-native
  buttons.

  The embeddable chat widget now renders a `write_confirmation_required` HITL
  event as an inline Yes/No button prompt inside the assistant bubble instead of
  silently ignoring it. Clicking a button sends the `confirm_tool_action` resume
  frame and un-pauses the turn; the chosen answer sticks in the transcript. This
  is the chat-native projection of SEP `ui/confirm` (a hosted extension's confirm
  prompt maps onto the existing `write_confirmation_required` frame).

  `ConversationController` gains `answerPrompt(requestId, value)` and an optional
  client-options constructor arg (a transport seam for tests). `ChatMessage` gains
  an optional `prompt` field (`ChatPrompt`) carrying the buttons; the multi-option
  shape also backs a future `ui/select` chat frame.

## 1.11.4

### Patch Changes

- 0953584: SEP Phase 4 (spec + SDK) — commands, flags, shortcuts, and session actions.

  **Spec.** New `command-complete.schema.json` (argument autocomplete). `session.schema.json` now carries the dispatch `context` on every params object (the wire form of the command-tier + epoch guard the host enforces) and adds `send_user_message` (`deliver_as` steer/follow_up/next_turn). `initialize.schema.json` gains a `flags` delivery map on the params and a `shortcuts` list (+ `ShortcutRegistration`) on the registrations. New conformance fixtures for command/complete, session send_user_message/append_entry, shortcuts, and flag delivery; new `$invalid` cases proving `context` is required on a session action and `value` on a completion. The reference `echo.mjs` registers a command + shortcut and answers command/execute + command/complete.

  **SDK.** `smooth.registerCommand` (with an optional `complete` completer), `registerFlag` (+ `smooth.getFlag`), and `registerShortcut`. Command handlers receive a `CommandContext` bound to their command-tier context, exposing `session.sendMessage` / `sendUserMessage` / `appendEntry`, `ui`, `hasUI`, and `args`. `createTestHost` gains `runCommand`, `completeCommand`, and a `session/*` service that enforces the same command-tier guard the engine does (event-tier → -32003), recording every session call for assertions. `runConformance` now replays command/execute + command/complete.

  **Demo.** `plan-mode` — the flagship extension that exercises phases 2–4 together: a `--plan` flag and a `/plan` command toggle plan mode; a `tool_call` intercept blocks write/edit/apply_patch/bash while it is on; each toggle pushes a `set_widget` render block and persists an LLM-invisible `appendEntry`, so the state survives a hot reload (the flag re-seeds it, the transcript keeps the history).

## 1.11.3

### Patch Changes

- a36ee69: SEP Phase 3 (SDK + spec) — the `ui/request` surface.

  The extension SDK now exposes the capability-negotiated UI surface. An extension
  reads the host's declared `ui_capabilities` from the `initialize` handshake and
  gates on `smooth.hasUI(kind)` / `ctx.hasUI(kind)`; `ctx.ui` (and `smooth.ui`)
  speak `ui/request` back to the host: `select`/`confirm`/`input` return the user's
  answer (or `{ cancelled: true }`), and `notify`/`setStatus`/`setWidget`/`setTitle`
  push to the frontend. A headless or uncapable host rejects with `RpcError` code
  -32001 (NoUI). `createTestHost(ext, { onUiRequest })` scripts the host side; its
  default mimics a headless frontend.

  Ships the `todo` demo extension (pi's todo, ported): stateful list whose tools
  push a `keyvalue` `set_widget` render block and whose `clear` asks for `confirm`
  first — both `hasUI`-gated, so it degrades cleanly headless.

  Extends `spec/extension/conformance/fixtures.json` with the remaining `ui/request`
  kinds (input/notify/set_status/set_widget/set_title), select/input/cancelled
  results, and invalid cases (unknown kind, missing `options`/`message`, extra
  property).

## 1.11.2

### Patch Changes

- 1c8f26f: SEP Phase 2 (SDK + spec) — hooks + the observe event bus.

  `@smooai/smooth-extension-sdk` gains **hook handlers**: `smooth.on(name, handler)`
  now covers both observe events (return ignored) and intercept hooks (return a
  `HookResult` — `{ block, reason? }` to veto or `{ patch }` to rewrite the input).
  The extension answers the `hook` request by folding its own handlers in
  registration order (first `block` short-circuits; `patch`es shallow-merge and
  thread to the next), and the host chains the outcome across extensions. Hook
  names are kept out of the reported event `subscriptions`. `createTestHost` gains
  `callHook(hook, input)`; new `permission-gate` demo extension blocks dangerous
  `bash` commands via a fail-closed `tool_call` hook.

  `spec/extension`: the event schema gains an optional `seq` (per-connection
  monotonic sequence; absent on the out-of-band `events_lost` marker) with a
  `model_select → AgentEvent::ModelResolved` parity note, and fixtures add a
  seq-numbered event, the `events_lost` marker (drop-N → count), a
  `tool_execution_start` event, and the `tool_result` hook input + a result-shaped
  `modify` outcome. Rust and TypeScript conformance replays stay green.

## 1.11.1

### Patch Changes

- 940560b: Add the SEP TypeScript extension SDK — Phase 1 (the tool path).

  New published package `@smooai/smooth-extension-sdk`: build Smooth Extension Protocol
  extensions in TypeScript. `defineExtension`/`defineTool` (zod v4 via `z.toJSONSchema`, with
  raw JSON-Schema / TypeBox pass-through), a symmetric JSON-RPC 2.0 `Peer`, an ndjson stdio
  transport (plus an in-memory `linkedPair`), `createTestHost` for driving an extension
  in-process, and `runConformance` to replay the shared fixtures against a real extension
  subprocess. Ships the `hello` demo extension (`hello.greet` — zod schema, streamed
  `tool/update` progress, `$/cancel` cancellation). Wired into the TypeScript CI lane.

  Extends `spec/extension/conformance/fixtures.json` for the tool path: `is_error` and
  `details` tool results, a message-only `tool/update`, and invalid fixtures (missing
  `content`, out-of-range `progress`).

## 1.11.0

### Minor Changes

- ec80d14: Add the SEP (Smooth Extension Protocol) spec — Phase 0.

  New `spec/extension/` tree: `envelope.md` (JSON-RPC 2.0 over ndjson framing, method
  catalog, error codes, context tiers, deferred WS binding), `methods/*.schema.json` (draft
  2020-12, snake*case: initialize, shutdown, ping, event, hook, tool/execute, tool/update,
  $/cancel, command/execute, registry/update, tools/set_active, session/*, exec/run,
  ui/request, kv/\_, bus/publish, log, plus the JSON-RPC frame envelope), and
  `conformance/fixtures.json` (43 valid + 6 invalid instances) with the dependency-free
  `echo.mjs` demo extension. A new `extension-conformance.test.ts` validates every fixture
  against its schema, mirroring the existing operator-protocol conformance harness. SEP is a
  sibling of the operator WebSocket protocol — it reuses the spec machinery, not the
  envelope.

## 1.10.4

### Patch Changes

- 00b2623: C# server: OTP / session-identity seam parity with the Rust reference (SMOODEV pearl th-8078dd).

  Brings the .NET reference server (`SmooAI.SmoothOperator.Server`) to behavioral parity with the Rust server's OTP / session-identity seam (PR #132), so a public agent's `end_user`-gated tools can offer a one-time-code identity flow while the server stays credential-free.

  - New host seam `IOtpService` (`SendOtpAsync(sessionId, contact) -> OtpDelivery`; `VerifyOtpAsync(sessionId, code) -> OtpVerifyOutcome.Verified | Invalid`) with the `OtpChannel` / `OtpContact` / `OtpDelivery` / `OtpError` value types. Registered via DI; absent ⇒ unchanged (the `end_user` gate fail-closed-refuses and no OTP is offered).
  - When a turn's auth gate refuses an `end_user` tool on an unverified session, an `IOtpService` is installed, and the session has a contact, the server emits `otp_verification_required`, calls `SendOtpAsync`, and emits `otp_sent` — before the terminal response. Admin refusals are never offered OTP.
  - New `verify_otp` action: a `Verified` outcome marks the session identity-verified (`otp_verified`); an `Invalid` outcome emits `otp_invalid` with the host's remaining-attempt count. Validation order mirrors Rust (requestId → sessionId → code → session-exists → service); no service installed ⇒ fail closed (`otp_invalid` / `NOT_FOUND`).
  - Per-conversation verified state is persisted in the session store and threaded into the auth gate via a store-backed `ISessionAuthenticator` default (replacing the hardcoded deny-all), so a verified caller's `end_user` tools run. The caller's email contact is captured at create-session time. Both are backed in the in-memory and Postgres stores with a shared contract test.

  The reference server does not park/auto-resume the original turn; the client re-sends its message after `otp_verified`. Event shapes validate against the same `spec/events/otp-*.schema.json`.

## 1.10.3

### Patch Changes

- f3ace72: Go server: OTP / session-identity seam parity for end-user tool auth (th-8078dd).

  Brings the Go reference server to parity with the Rust server's OTP / session-identity seam (PR #132). A public agent's `end_user`-gated tools can now offer a one-time-code identity flow, while the Go server stays credential-free — it never generates, delivers, or validates a code.

  - New `OtpService` seam (`SendOtp` / `VerifyOtp`) plus the `OtpContact`, `OtpDelivery`, `OtpChannel`, `OtpErrorCode`, and `OtpVerifyOutcome` value types, mirroring the existing resolver seams. Installed via `server.WithOtpService`; absent ⇒ unchanged fail-closed behavior (the gate refuses, no OTP offered).
  - The session's OTP-verified bit (`StoredSession.OtpVerified`, set by a successful `verify_otp`) is threaded into the auth gate so a verified caller's `end_user` tools run.
  - On an `end_user` refusal, with a service installed and a session contact captured at create-session time, the server emits `otp_verification_required`, calls `SendOtp`, and emits `otp_sent` (before the terminal `eventual_response`, matching the Rust ordering). `admin` refusals are never offered OTP.
  - New `verify_otp` action: validation order `requestId → sessionId → code → session-exists → no-service`; a correct code emits `otp_verified` and marks the session authenticated, a rejected code emits `otp_invalid` with the host's remaining attempts, and no installed service fails closed (`otp_invalid` / `NOT_FOUND`).

  Semantics match the Rust reference exactly. Exhaustive tests (seam types, verify_otp happy/invalid/no-service/unknown-session/missing-fields, offer-flow event order, admin-not-offered, verified-session-runs-tool); server events validate against the shared `spec/events/*` schemas.

## 1.10.2

### Patch Changes

- 8535264: Python server: OTP / session-identity seam parity with the Rust reference (SMOODEV pearl th-8078dd).

  Brings the Python operator server to behavioral parity with the Rust server's end-user OTP identity-verification seam (landed for Rust in #132). Like the reference, the Python server never generates, delivers, or validates a code — a new host seam, `OtpService` (`smooth_operator_server.otp`, with `OtpContact` / `OtpDelivery` / `OtpChannel` / `OtpError` / `OtpVerified` / `OtpInvalid`), owns generation, delivery, expiry, and attempt counting. Install one via `ServerState.otp_service` (or `FrameDispatcher(..., otp_service=...)`); absent (the default), behavior is unchanged — the `end_user` auth gate fail-closed-refuses and no OTP is offered.

  - When a turn's gate refuses an `end_user` tool on an unverified session and an `OtpService` is installed and the session has a contact (the caller's email, captured at create-session time), the server emits `otp_verification_required`, calls `send_otp`, and emits `otp_sent` — in that order, before the terminal `eventual_response`. An `admin` refusal is never OTP-remediable, so it is not offered.
  - A new `verify_otp` action validates a submitted code via `OtpService.verify_otp`: an `OtpVerified` outcome marks the session identity-verified (persisted on the session store) and emits `otp_verified`; an `OtpInvalid` outcome emits `otp_invalid` with the host's remaining-attempt count and optional machine-readable reason. Validation order mirrors Rust (requestId, sessionId, code required; unknown session → `SESSION_NOT_FOUND`; no service → fail closed `otp_invalid` / `NOT_FOUND`).
  - Per-session verified state is tracked on the session store and threaded into the tool auth gate as the resolved `session_authenticated` bit (the session's OTP-verified state OR'd with the existing `SessionAuthenticator` seam), so a verified caller's `end_user` tools run.

  The reference server does not park/auto-resume the original turn; the client re-sends after `otp_verified`. The four OTP event builders reproduce the shared conformance fixtures byte-for-byte; exhaustive tests cover verify happy/invalid/no-service/unknown-session/missing-field, the offer flow's emission order, admin-not-offered, no-contact/no-service/send-failure edges, and a verified session running the gated tool.

## 1.10.1

### Patch Changes

- 9352c87: TS server: OTP / session-identity seam parity with the Rust reference (pearl th-8078dd).

  Brings `typescript/server` to parity with the Rust server's end-user OTP / session-identity seam (#132). The native TS server can now offer a one-time-code identity-verification flow behind a public agent's `end_user` tool auth gate, without holding any credentials itself.

  - New host seam `OtpService` (`typescript/server/src/otp.ts`) with `sendOtp` / `verifyOtp`, mirroring the shape of the server's other pluggable seams (`AgentConfigResolver`, `SessionAuthenticator`). Installed via the `otpService` server option; absent → unchanged fail-closed behavior (the `end_user` gate refuses and no OTP is offered). The server never generates, delivers, or validates a code — the host owns generation, delivery, expiry, and attempt counting.
  - When a turn's auth gate refuses an `end_user` tool on an unverified session and an `OtpService` is installed and the session has a contact, the server emits `otp_verification_required`, calls `sendOtp`, and emits `otp_sent` — in that order, before the terminal `eventual_response`.
  - New `verify_otp` action validates a submitted code: a `verified` outcome marks the session identity-verified and emits `otp_verified`; a non-verified outcome emits `otp_invalid` with the host's remaining-attempt count. No service installed → fail closed (`otp_invalid` / `NOT_FOUND`).
  - The session's OTP-verified bit is tracked on the session store (`contactEmail` captured at create-session time, `otpVerified` set by `verify_otp`) and threaded into the `end_user` auth gate, so a verified caller's gated tools run on the re-sent message. Admin refusals are never offered OTP.

  The server does not park/auto-resume the original turn; the client re-sends its message after `otp_verified`. Four protocol event builders + the shared `spec/conformance/fixtures.json` OTP fixtures + exhaustive tests (verify_otp happy/invalid/no-service/unknown-session/missing-fields, offer-flow event order, admin-not-offered, verified-session tool execution) added.

## 1.10.0

### Minor Changes

- 86d9e4f: Server-side OTP / session-identity seam so hosts can wire end-user tool auth (SMOODEV pearl th-8e8a89).

  The Rust reference server can now offer a one-time-code identity-verification flow behind a public agent's `end_user` tool auth gate, without holding any credentials itself. A new host seam, `OtpService` (`smooth_operator::otp`), owns code generation, delivery, expiry, and attempt counting; the reference server only orchestrates the wire flow around it. Install one via `AppState::with_otp_service`; absent, behavior is unchanged (the `end_user` gate fail-closed-refuses and no OTP is offered).

  - When a turn's auth gate refuses an `end_user` tool on an unverified session and an `OtpService` is installed and the session has a contact, the server emits `otp_verification_required`, calls `send_otp`, and emits `otp_sent`.
  - A new `verify_otp` action validates a submitted code via `OtpService::verify_otp`: a `Verified` outcome marks the session identity-verified and emits `otp_verified`; an `Invalid` outcome emits `otp_invalid` with the host's remaining-attempt count. With no service installed, verification fails closed (`otp_invalid` / `NOT_FOUND`).
  - Per-session verified state is tracked in session metadata and threaded into the auth gate as the real `session_authenticated` bit (previously hardcoded `false`), so a verified caller's `end_user` tools run.

  The reference server does not park/auto-resume the original turn; the client re-sends its message after `otp_verified`. Rust-only for now (mirrors how per-agent config landed as separate per-language PRs); parity in the Python/TS/Go/.NET servers is follow-up work.

## 1.9.0

### Minor Changes

- 0e29a9b: Per-agent behavior config: honor `instructions` + run `conversation_workflow` (SMOODEV-590).

  The reference server resolved a turn's system prompt from **per-org** settings, so every agent in an org spoke with the same voice and `conversation_workflow` was never applied — a public chat agent ignored its own persona and behaved as the generic customer-support bot.

  Config-delivery seam (matches the sibling Python/TS/C#/Go lanes): `AgentConfigResolver::resolve(agent_id)` — the ws protocol's `create_conversation_session` carries only an agent UUID, so config is resolved **server-side by id**. Default `StaticAgentConfigResolver` (empty ⇒ no-op, behavior unchanged); a `PgAgentConfigResolver` reads the monorepo `agents` table on the adapter's existing pool. The runner now:

  - uses the agent's `instructions` (+ `personality.persona`) as the system prompt, overriding the org default;
  - injects the agent's `greeting` into the prompt only on the first turn of a conversation;
  - restricts the turn's tools to `tool_config.enabledTools` (`enabled == true` entries by snake_case `toolId`; empty/absent ⇒ full set; unknown ids ignored), and delivers each entry's `config` to the tool via `ToolProviderContext`;
  - enforces per-tool `authLevel` at execution against the agent's `visibility` (a `ToolHook` gate: admin blocked on public agents; internal auto-satisfies; end_user on public requires an identity-verified session, fail-closed — the OTP flow is a host seam);
  - when a `conversation_workflow` is set, injects the current step's intent/criteria and, after each turn, runs a cheap failure-tolerant judge on the configurable `judge_model` (haiku-tier default) to advance the step; the step id is tracked per session.

  Per-agent isolation, malformed-jsonb tolerance (degrade to org default, never crash the turn), judge-failure tolerance (stay on the current step), and the authLevel branches (admin/end_user/internal, authed vs not) are covered by unit + integration tests.

- 9db9007: C# server: honor per-agent config + implement conversation workflows. An agent's `instructions.prompt` now drives its system prompt (overriding the org/default persona), so agents in the same org behave as themselves rather than a generic customer-support persona. `conversation_workflow` (goal + intent/criteria steps) is now implemented as a stepped, judge-advanced guided-agency flow: the current step's intent + criteria are rendered into the system prompt, and a cheap post-turn judge decides whether the step's criteria were met to advance (explicit `next` or sequential), with the current step id persisted per conversation. Per-agent `greeting` is woven into the agent's first reply only (first-turn prompt seed), and `tool_config.enabledTools` restricts the server's tool set to the agent's enabled snake_case toolIds per turn (empty/absent ⇒ the full set, unchanged). At tool-execution time each entry's `authLevel` is enforced (admin blocked on public agents; `end_user` needs a verified session via the new `ISessionAuthenticator` seam, default fail-closed; internal agents auto-satisfied; only tools declaring `supportsAuthRequirement` are gated) and its per-tool `config` is delivered to the executing tool. The workflow judge model is the uniform `judgeModel` option. Per-agent config reaches the server through a new `IAgentConfigResolver` DI seam (`ResolveAsync(agentId)`, default dict-backed `StaticAgentConfigResolver`) — `create_conversation_session` carries only an agent UUID, so config is resolved server-side per turn from the session's agent (mirroring the TS / Python lanes' `AgentConfigResolver`). jsonb parsing is tolerant (malformed config degrades to the default persona, never crashes a session) and the judge is failure-tolerant (any error keeps the conversation on the current step). Mirrors the Rust server change and the monorepo SMOODEV-590 behavior.
- a69a799: C# server local flavor: serve a prebuilt SPA same-origin from `SMOOTH_WEB_DIR` with the local token injected into `index.html` as `window.__SMOOTH_TOKEN__`, a `SMOOTH_LOCAL_TOKEN` → `LocalTokenVerifier` for same-origin `/ws` auth, and `SMOOTH_PERSONA` to set the agent's system prompt. Lets the .NET server be a drop-in "Big Smooth" backend behind the shared smooth-web Presence UI (validated end-to-end: SPA + WS + streamed persona reply).
- a6fab4a: Go server: honor per-agent config + implement conversation workflows (SMOODEV-590).

  Agents served by the Go operator now respect their own per-agent config instead of all sharing one generic org persona. A new `AgentConfigResolver` seam resolves a session's `agentId` into its `AgentConfig` (instructions, `Workflow`, greeting, personality, tool allow-list); resolution is server-side because the `create_conversation_session` payload carries only an `agentId`. An un-configured agent (no resolver, or resolver returns nil) falls back to the server/org default prompt + full tool set, so existing behavior is unchanged. Wire one in via `server.WithAgentConfigResolver`.

  `conversationWorkflow` is implemented as a stepped, judge-advanced guided-agency flow: the current step's intent + criteria are rendered into the system prompt (`<ConversationWorkflow>` block), and after each turn a cheap failure-tolerant judge LLM call decides whether the criteria were met and advances the pointer (following `next` or array order), tracked as `CurrentStepID` on the session. Malformed config degrades to the default flow and never crashes a session. Mirrors the TS/Python server siblings and the Rust reference's `agent-config-instructions-workflow` design.

- ebd2ad2: Python server: honor per-agent config + implement conversation workflows (SMOODEV-590).

  Agents served by the Python server previously ignored their per-agent config and always used the generic server-wide "customer support agent" persona. Now:

  - **Per-agent `instructions`** drive the system prompt for that agent's conversations, overriding the server-wide default (falling back to it when unset). Per-agent `personality` and first-turn `greeting` are plumbed into the prompt; `tool_config.enabledTools` (`[{ toolId, enabled, authLevel, config }]`, the monorepo `AgentToolConfig` shape) is a tool allow-list restricting the agent's turns to the `enabled=true` tools by `toolId` (empty/absent → full set; unknown toolIds ignored), matching the Go/TS lanes. Per-tool `authLevel` is enforced at execution against the agent's `visibility` and a `SessionAuthenticator` seam (admin blocked on public agents; internal auto-satisfies; end_user on public requires identity verification, fail-closed), and each entry's `config` is delivered to the tool at execution. The post-turn judge model is a `judge_model` server option (haiku-tier default).
  - **`conversation_workflow`** is implemented as a stepped, judge-advanced guided flow: the current step's intent + criteria are rendered into the system prompt, and a cheap post-turn judge call decides whether the criteria were met and advances to the next step (explicit `next` → sequential → terminal). The current step id is tracked per conversation.

  Config parsing is tolerant — a malformed workflow or config degrades to the server default and never crashes a session. The judge is failure-tolerant — any judge error leaves the conversation on the current step. Delivery seam: `ServerState.agent_config_resolver` (`AgentConfigResolver.resolve(agentId)`, default dict-backed `StaticAgentConfigResolver`) is resolved per turn from the session's agent — the ws protocol carries only an agent UUID, so config is looked up server-side. Empty resolver → behavior unchanged. Mirrors the Rust reference PR.

## 1.8.0

### Minor Changes

- 023c531: feat(auth): JWKS-based JWT verification (ES256 + any algorithm, with rotation) for `smoo`/`jwt` modes

  The auth verifier could only validate tokens against a **static RS256 PEM**
  (`AUTH_JWT_RS256_PUBLIC_KEY`). SmooAI's `auth.smoo.ai` (the `smoo` issuer) signs
  dashboard tokens with **ES256** (`/.well-known/jwks.json` → `alg: ES256, kty: EC`),
  so every real SmooAI token was rejected — blocking `AUTH_MODE=smoo` for the SmooAI
  K8s flavor.

  This adds a JWKS-backed verification path (additive, behavior-preserving):

  - New optional `AUTH_JWT_JWKS_URL`, and auto-derivation of
    `{AUTH_JWT_ISSUER}/.well-known/jwks.json` when an issuer is set and no static
    key is given.
  - Keys are fetched, **cached** (TTL) and **rotation-aware** (refresh-on-unknown-`kid`),
    selected per-token by `kid`, and validated with the key's algorithm via
    `DecodingKey::from_jwk` — so **any** advertised JWS algorithm works
    (ES256/ES384/RS256/PS256/EdDSA/…), not just RS256.
  - Wired into both `SmooIdentityVerifier` (the `smoo` path) and `JwtVerifier`
    (BYO), so any OIDC issuer works. `AuthVerifier::verify` stays **synchronous**
    (the keyset is read from cache; the network fetch is off the hot path).

  Key-source precedence (`jwt`/`smoo`): static `AUTH_JWT_RS256_PUBLIC_KEY` →
  static `AUTH_JWT_HS256_SECRET` → JWKS (`AUTH_JWT_JWKS_URL`, else issuer-derived).
  The static-RS256/HS256 paths are unchanged. With this, `AUTH_MODE=smoo` needs
  only `AUTH_JWT_ISSUER` (+ optional audience) — no static public key.

## 1.7.1

### Patch Changes

- 86dd6f8: local flavor: serve the canonical `@smooai/chat-widget` (Aurora Glass) bundle

  The local-flavor server now vendors and serves the published **`@smooai/chat-widget`**
  (Aurora Glass) standalone bundle instead of a parallel copy of the widget. One canonical
  public widget, consumed — not two. Same `<smooth-agent-chat>` element + `endpoint`/`agent-id`
  attributes, so it's a drop-in for the host page.

## 1.7.0

### Minor Changes

- 1d9c60e: feat: thread `organization_id` into `AccessContext` for per-turn knowledge scoping

  `StorageAdapter::knowledge_for_access(&self, access)` carried only `user_id` +
  `groups` — no org — so a multi-tenant relational backend (SmooAI) could not scope
  RAG to the turn's organization and was forced to a single static org. This was the
  last multi-tenant gap on the knowledge path.

  `AccessContext` now carries an additive `organization_id: Option<String>`
  (default `None`, set via the new `with_organization_id(...)` builder). The
  authenticated-principal path (`Principal::access_context`) stamps the principal's
  org automatically; the reference server / lambda send-message paths fall back to
  the turn's **session** org (every session carries `organization_id` since the
  create-session path derives it) when the requester has no org of its own. The org
  is then **available** to a host adapter's `knowledge_for_access` so it can scope
  retrieval to the right tenant.

  The operator's built-in single-tenant ACL ignores the org (org isolation already
  happens upstream), so this is behavior-preserving for the reference/local flavor.
  The Postgres knowledge adapter additionally uses the context's org — when present
  — to **override** its construction-time org as a cheap SQL pre-filter
  (`organization_id = $1`), so one adapter instance can serve per-turn tenants
  instead of being pinned to a single static org; an org-less context leaves the
  construction-time org unchanged.

## 1.6.0

### Minor Changes

- bdbf868: feat(server): derive org + agent from auth in `create_conversation_session`

  `handle_create_session` no longer hard-codes the seed org. It now derives the
  session's `organization_id` from the authenticated request, in priority order:

  1. the agent's widget-auth policy `organization_id` (widget visitors authenticate
     via origin + `authContext`, not a JWT, so their org rides on the agent policy —
     new optional `AgentWidgetAuth.organization_id` field),
  2. the connection's authenticated JWT principal org (dashboard / authed clients —
     the principal's `org_id` is now threaded from the `/ws` handshake through to the
     handler instead of being dropped at `AccessContext` reduction),
  3. the server's seed org as a behavior-preserving fallback for the no-auth/local
     flavor.

  The agent id continues to come from the inbound `agentId` payload. The same
  JWT-org-then-configured-org derivation is applied to the lambda dispatch
  create-session path. All existing in-memory/seed flows are unchanged.

## 1.5.0

### Minor Changes

- f2ecef9: Add `organizationId` to the `Session` domain type so org-scoping is uniform across every core domain type (`Conversation`, `Participant`, and `Message` already carry it). Storage backends can now write the session's org directly instead of re-deriving it from the conversation. The built-in Postgres adapter gains an `organization_id` column (additive, `DEFAULT ''`) on `conversation_sessions` plus an org index; the in-memory and DynamoDB adapters thread the new field through automatically; server/runner create-session paths populate it from the conversation/turn org already in scope.

## 1.4.0

### Minor Changes

- 45fd77e: Thread the turn's `conversation_id` and resolved per-org `gateway_key` into `ToolProviderContext`.

  A host's injected `ToolProvider` now receives the conversation the turn runs in and the LLM-gateway key that turn was billed/scoped to — alongside the existing `org_id` + `access`. This lets SmooAI's conversation-persisting tools correlate to the right conversation (instead of degrading to a no-op on an empty conversation id) and lets agent-brain's `knowledge_search` obtain the gateway key.

  Purely additive and behavior-preserving: both new fields are `Option`, default to `None` via `ToolProviderContext::new`, and existing `ToolProvider` impls that ignore them are unaffected. New builders `with_conversation_id` / `with_gateway_key` set them; the runner populates both from the turn it already has in hand.

## 1.3.0

### Minor Changes

- 12d348a: Add two host provider-injection seams to the chat runner so a deployment flavor can run a turn with its OWN tools and persona without forking the runner:

  - **Custom tool injection** — a new `ToolProvider` trait (`tools_for(&ToolProviderContext) -> Vec<Arc<dyn Tool>>`) plus `AppState::with_tools(provider)`. When installed, the runner merges the provider's per-turn tools into the turn's `ToolRegistry` alongside the built-ins; the `ToolProviderContext` carries the turn's `org_id` + `AccessContext` so a host can return per-org tools. No provider ⇒ the registry is exactly today's built-ins.
  - **Per-org agent persona** — an optional `AgentSettings.persona: Option<String>`; the runner uses the resolved persona as the turn's system prompt when present, else falls back to the existing const `KNOWLEDGE_CHAT_SYSTEM_PROMPT`. No persona ⇒ identical prompt to today.

  Both seams are behavior-preserving by default — the local/default flavor is unaffected.

- ab1aa9d: feat(server): `confirm_tool_action` — write-confirmation human-in-the-loop pause/resume

  The reference WebSocket server can now gate write tools behind human approval.
  When an agent turn calls a tool whose name matches `SMOOTH_AGENT_CONFIRM_TOOLS`
  (comma-separated substrings), the turn **parks** and emits a
  `write_confirmation_required` event (matching
  `spec/events/write-confirmation-required.schema.json`) carrying
  `{ toolId, actionDescription }`. The client resumes it by sending
  `confirm_tool_action` (`{ sessionId, requestId, approved }`, per
  `spec/actions/confirm-tool-action.schema.json`): on `approved: true` the parked
  tool executes; on `false` it is skipped with a rejection result the model sees,
  and the turn still completes.

  Built entirely on the existing smooth-operator-core human-gate primitive
  (`ConfirmationHook` + `human_channel()` + `AgentConfig::with_human_channel`) —
  **no core change required**. The server wires the hook's `HumanRequest` stream to
  a WS event and bridges an inbound `confirm_tool_action` back to the hook's
  `HumanResponse`, keyed by session. The `send_message` turn now runs in a spawned
  task so the socket reader stays free to receive the confirmation on the same
  connection (the turn would otherwise deadlock awaiting a frame it is blocking).

  With `SMOOTH_AGENT_CONFIRM_TOOLS` unset (the default), no `ConfirmationHook` is
  installed, no tool ever parks, and behavior is byte-for-byte unchanged. The
  local/default flavor is unaffected.

- feec0b5: Add a per-org LLM gateway-key resolution seam so a multi-tenant flavor can
  bill/scope each org's turns to its own gateway key (e.g. a per-tenant LiteLLM
  virtual key), while the local/default flavor keeps using the single environment
  key.

  - New `GatewayKeyResolver` trait (`smooth_operator::gateway_key`) — the public,
    contributable hook: `async fn resolve(&self, org_id: &str) -> Option<String>`.
  - Default `EnvGatewayKeyResolver` returns the single `SMOOAI_GATEWAY_KEY` for
    every org, so behavior is unchanged unless a host injects a per-org resolver.
  - `resolve_gateway_key(resolver, org_id, env_key)` helper centralizes the
    resolve-then-fall-back-to-env contract used by the per-turn LLM-config build.
  - The server's `AppState` holds an `Arc<dyn GatewayKeyResolver>` (default =
    `EnvGatewayKeyResolver`) with a `with_gateway_key_resolver(...)` builder for
    injection. `send_message` resolves the turn's `org_id` from its conversation,
    resolves the key, and falls back to the env key when the resolver returns
    `None`.

  Behavior-preserving by default: with no resolver injected, every turn uses the
  env key exactly as before. No SmooAI/DB specifics live in the shared code — only
  the trait and the env default; a host injects its own per-org key store.

- 45be211: Add a `get_conversation_messages` WebSocket action to `smooth-operator-server`. Returns paginated message history for a session's conversation (`{ conversationId, messages, nextCursor, hasMore }`), wrapping the existing `StorageAdapter::list_messages_by_conversation` (the same call the admin API + turn runner use). Optional `limit` (default 50) + opaque `cursor`, newest-first. Completes wire-compat for chat clients that page history over the socket (previously only `/admin` exposed it).
- cf6fab4: feat(server): graceful SIGTERM/ctrl_c drain of WebSocket connections.

  The reference WebSocket server (`smooth-operator-server`) now drains in-flight
  turns on shutdown instead of being killed mid-flight. Previously `run()` did a
  plain `axum::serve(listener, app).await` with no `with_graceful_shutdown`, so on
  a Kubernetes pod termination (scale-down / rollout) the process was killed while
  turns were in progress — in-flight WebSocket turns dropped and connections never
  `detach`ed from the `Backplane`, leaving stale registry entries in Valkey/NATS.

  A single shared `tokio_util::sync::CancellationToken` is now threaded through
  `AppState` (`shutdown`, defaulted to a fresh never-cancelled token in
  `AppState::new`, plus a `with_shutdown` builder). Each per-connection reader loop
  `select!`s on that token (`biased`, shutdown wins ties) with the inbound-frame
  read — and keeps `handle_frame(...).await` inside the frame arm so a turn already
  in flight finishes before the next shutdown check. After the loop the existing
  `backplane.detach(...)` runs, so the connection always leaves the registry clean.
  The serve loop (`run`) wires `axum::serve(...).with_graceful_shutdown(...)` to
  SIGTERM (k8s) or ctrl_c (interactive), cancelling the token to fan the drain out
  to every connection within the chart's `terminationGracePeriodSeconds` window.

### Patch Changes

- 7545ea8: Add an unauthenticated `GET /health` HTTP route to `smooth-operator-server`. A WebSocket `/ws` upgrade can't answer a plain GET healthcheck, so HTTP load balancers (AWS ALB, nginx ingress) had nothing to probe; `GET /health` now returns `200 OK`, dependency-free (no storage/LLM touch). Enables HTTP health checks for the K8s deployment flavor.

## 1.2.0

### Minor Changes

- 5971864: Phase 4: streaming turn execution across the Python, TypeScript, and Go cores (C#
  already streams via MEAI's `RunStreamingAsync`). A new streaming run method alongside
  the existing `run()` — TS `runStream` (`AsyncGenerator<StreamEvent>`), Python
  `run_stream` (`AsyncIterator[StreamEvent]`), Go `RunStream` (returns a `*Stream` whose
  `Events()` channel carries `StreamEvent`s and whose `Err()` reports a mid-turn model
  error) — drives the SAME agentic loop (system/knowledge/memory build, compaction, cost
  tracking, budget early-stop, deferred tools, clearance + human-gate, checkpoint/thread
  persistence) but calls the model in STREAMING mode and yields incremental events: a
  `text` event per content delta, a `tool_call` event per requested call (before
  dispatch), a `tool_result` event per finished tool (in original call order even under
  `parallelToolCalls`), and exactly one terminal `done` event carrying the same
  `AgentRunResponse` `run()` would return. The provider seam gains an OpenAI-style
  streaming call (`createStream` / `create(..., stream=True)` / `ChatStream`) that
  accumulates content + `tool_calls` deltas by index into a full assistant message, so
  the rest of the loop is unchanged; usage is read from the final chunk for cost/budget.
  The reusable mock LLM providers replay their FIFO script as chunked deltas (text split
  into pieces, tool-call arguments split across two chunks). Retry-with-backoff is
  intentionally not applied to streaming (re-running would re-emit chunks), mirroring C#.

## 1.1.0

### Minor Changes

- a89045d: Phase 4: concurrent (parallel) tool-call execution across the Python, TypeScript, Go,
  and C# cores. A new opt-in `parallelToolCalls` option (Python `parallel_tool_calls`,
  Go/C# `ParallelToolCalls`), default false, dispatches an assistant turn's tool calls
  concurrently (`asyncio.gather` / `Promise.all` / goroutines + `sync.WaitGroup` /
  `Task.WhenAll`) when there are two or more. The tool-result messages are still appended
  in the original tool-call order, so the transcript stays deterministic regardless of
  completion order; a failing or human-denied tool keeps its error result in its correct
  position. With the flag off (the default) — or for single-tool-call turns — dispatch is
  unchanged from today's sequential behavior. Per-tool semantics (clearance, human-gate
  approval, tool_search promotion, JSON-arg parsing) are untouched.

## 1.0.0

### Major Changes

- 6f6f622: Unified 1.0.0 polyglot publish — all five language implementations now ship from one changeset at one shared version via the existing lockstep release.

  - **Rust** reclaims the crate name `smooai-smooth-operator` (the predecessor standalone engine 0.13.x is superseded by `smooai-smooth-operator-core`) and publishes the full set: the reference lib plus 7 library crates (`-ingestion`, the `-adapter-*` storage/backplane adapters, and `-server`) to crates.io.
  - **Python** distributions are renamed to `smooai-smooth-operator` and `smooai-smooth-operator-core` (PyPI), keeping the `smooth_operator` / `smooth_operator_core` import packages unchanged.
  - **Go** is published by tag `go/v1.0.0` (subdir module `github.com/SmooAI/smooth-operator/go`).
  - **npm** (`@smooai/smooth-operator`) and **NuGet** (`SmooAI.SmoothOperator.Core`) continue as before.

  One changeset → one shared version → npm + NuGet + crates.io + PyPI + Go tag, all stamped by `scripts/sync-versions.mjs`.

## 0.9.0

### Minor Changes

- 08f1780: Phase 2: human-in-the-loop approval (HumanGate) across the Python, TypeScript, and
  Go cores, at parity with the C# reference. The agent consults an optional approval
  gate before running any tool flagged by a `requires_approval` predicate; a denial is
  fed back to the model as the tool result (the tool never runs) and an approval lets
  it execute normally. With no gate configured, behavior is unchanged.

## 0.8.0

### Minor Changes

- a8bfb62: HTTP-backed widget auth (SMOODEV-1890): `HttpWidgetAuth`, a generic `WidgetAuthProvider` that resolves each agent's embed policy (`allowed_origins` + `public_key`) by GETting `{base_url}/{agentId}` from a host policy service, with TTL caching. Response handling fails safe: 2xx caches the policy, 404 caches a no-policy result (denied under `WIDGET_AUTH_STRICT`), and 5xx/network/malformed responses return `None` without caching so the next connect retries. The server now installs it from env — set `WIDGET_AUTH_URL` (plus optional `WIDGET_AUTH_BEARER` / `WIDGET_AUTH_TTL_SECS`) to enforce embeddable-widget auth against a host's policy service with no custom binary; unset leaves the permissive default. This is the reusable mechanism a host backs with its own agent store (SmooAI points it at an api-prime route).
- bc901d7: Persistent + semantic agent memory (SMOODEV-1470, parity gap Phase 3): `PgMemory`, a pgvector-backed implementation of the core `Memory` trait in the `adapters/postgres` crate. Before this the only `Memory` backend was the core `InMemoryMemory` (a `Vec` behind a `Mutex`, keyword recall, lost on restart). `PgMemory` gives the general agent cross-thread user memory that survives restarts and recalls by semantic similarity — the Rust equivalent of the TS `store`/`store_vectors` namespaced by `['memories', orgId, userId]`.

  Each `PgMemory` instance is bound to one `(organization_id, user_id)` namespace at construction (built via `PostgresAdapter::memory(org, user)`; `user_id = None` for org-wide memory), mirroring how `PgKnowledgeBase` binds an org — the core `Memory::recall(query, limit)` signature carries no scoping, so scoping is threaded through the constructor. `store` embeds the entry content and upserts a row in a new `memories` table (`embedding vector(N)` matching the active `Embedder` dim, HNSW cosine index, namespaced by `(organization_id, user_id)`); `recall` embeds the query and returns the namespace's top-K by pgvector cosine distance with `relevance` set to the cosine similarity; `forget` deletes within the namespace. Embedding goes through the shared `Embedder` seam (DeterministicEmbedder offline, GatewayEmbedder live), so memory and knowledge vectors share column width and hashing. Covered by a testcontainers integration test (semantic recall, org/user namespace isolation, namespace-scoped forget, empty recall) that skips cleanly when Docker is unavailable. No change to the core `Memory` trait was required.

## 0.7.0

### Minor Changes

- ed12900: Realtime publish endpoint (SMOODEV-1893): `POST /admin/publish` lets non-AI publishers — job status, ingestion progress, notifications, billing — push an event to a backplane target over the WebSocket fleet without going through an agent turn. Body is `{ target: { type: session|user|org|agent|connection, id }, event }`; it calls `Backplane::publish`, so with a distributed backplane the event fans out across pods. Admin-gated (RBAC role 2); the response reports local deliveries on the serving pod (cross-pod deliveries happen but aren't counted). Targets are opaque ids matched against the connection registry — tenant id-namespacing is a host concern, documented on the handler.

## 0.6.0

### Minor Changes

- e9fa854: Distributed Backplane backends (SMOODEV-1892): `RedisBackplane` and `NatsBackplane` — the horizontal scale-out seam. Both implement the `Backplane` trait by wrapping a per-pod `InMemoryBackplane` for local registry + delivery and adding a pub/sub bus (Redis/Valkey channel or NATS subject) for cross-pod fan-out: `publish(Target, event)` delivers to local sockets immediately, then broadcasts a `BackplaneEnvelope` so every other pod re-resolves the target against its own registry and delivers to its sockets (the origin pod skips its own echo). This makes the same `publish` call reach a socket on any replica — required to run the WS service with >1 pod, and the cross-pod path for non-AI publishers. Selected at runtime via `SMOOTH_AGENT_BACKPLANE` (`memory` | `redis`/`valkey` | `nats`) + `SMOOTH_AGENT_BACKPLANE_URL`; default stays single-process in-memory. `Target` is now `Serialize`/`Deserialize` and a shared `BackplaneEnvelope` is exposed so a host's own transport adapter can speak the same wire format. New crates: `adapters/backplane-redis`, `adapters/backplane-nats` (cross-pod fan-out proven end-to-end over real Redis + NATS via testcontainers).

## 0.5.0

### Minor Changes

- e6d9dbe: Connection backplane (SMOODEV-1891): a pluggable `Backplane` trait + default `InMemoryBackplane` in the OSS server — the scale-out + event-delivery seam. Each connection's outbound sink is attached on connect and associated with its session/agent; `publish(Target, event)` delivers to every connection for a target. This is the foundation for running >1 replica (a Redis/NATS impl makes delivery cross-pod) and the plug point for non-AI realtime: any service can `publish(Target::Session(...), event)` and reach the connected client over WebSocket. Wired into `AppState` (`with_backplane`) + the connection lifecycle. Runtime-agnostic (the sink is a closure, no tokio dep added to the lib).

## 0.4.0

### Minor Changes

- 715f79c: Embeddable-widget auth (SMOODEV-1878): a pluggable `WidgetAuthProvider` hook in the Rust server that enforces a per-agent **origin allowlist** + public-key **`authContext`** (HMAC-SHA256, replay-protected) for `<smooth-agent-chat>` connections. The `Origin` header is captured at the WebSocket handshake and validated at `create_conversation_session`; hosts plug in a concrete provider (backed by their agent store) while the bundled `PermissiveWidgetAuth` leaves a standalone OSS server unaffected. `WIDGET_AUTH_STRICT=1` fails closed on unknown agents.

## 0.3.0

### Minor Changes

- 0933942: C# server (`SmooAI.SmoothOperator.Server`) + engine hardening, at Rust parity.

  Server (new):

  - Durable Postgres adapters: ACL knowledge store (ACL filtered in SQL via `acl_groups && groups`, leak contract on both in-memory and Postgres backends), session store, and checkpoint store — agent state, sessions, and ACL-scoped knowledge all survive a restart.
  - `GatewayEmbedder` for real semantic retrieval (deterministic fallback when no gateway key).
  - Reranker: opt-in post-retrieval reorder (`SMOOTH_AGENT_RERANK=gateway|lexical|off`) — engine `IReranker`/`NoopReranker`/`LexicalReranker` + server `GatewayReranker` + `RerankSelection`, wired through the turn; fails soft if the reranker errors.
  - Auth-gated `/admin` API: `/admin/health`, `/admin/me`, `/admin/connectors`, and `POST /admin/reindex` (re-ingest without a restart); fail-closed Bearer auth.
  - Tool `stream_chunk`s: tool call/result surfaced over the WebSocket protocol.
  - Deployable host (`SmooAI.SmoothOperator.Server.Host`) + Dockerfile: wires gateway model, storage, JWT/trusted/none auth, and startup GitHub ingestion.

  Engine (`SmooAI.SmoothOperator.Core`):

  - `IReranker` + `NoopReranker` + `LexicalReranker` + `Rerankers.ApplyOptionalAsync`.
  - `RunStreamingAsync` now yields the tool-result update so tool results surface in the stream.

  Robustness fixes:

  - Chunker no longer infinite-loops on long non-whitespace runs (minified code / base64 / long URLs).
  - The dispatcher emits a clean error and keeps the connection alive on any handler exception (was dropping the socket silently).
  - Postgres checkpoint store preserves tool-call/result content (was serializing text only).
  - GitHub connector fails loud on a truncated tree instead of silently indexing a partial repo.
