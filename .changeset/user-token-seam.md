---
'@smooai/smooth-operator': minor
'@smooai/smooth-operator-server': minor
---

`ToolProviderContext` can now carry an opaque act-as-user credential, so a host's
tools can call the host's own APIs as the acting user (th-8400b7).

The operator verifies the connection's JWT and hands tools only `AccessContext`
— user id, groups, org — with the bearer dropped. That default is right, and it
had a cost. In smooai it meant the copilot drawer's tools could not call the
platform REST API as the user, so they grew raw-SQL CRM writes that bypassed the
routes' validation, write gate, outbound sync and activity log. The MCP surface,
whose transport IS an authenticated HTTP session, called the API and stayed
correct. Two tool stacks diverged on transport, field coverage and side effects
for one reason: one of them had no way to authenticate.

`user_token` is `None` unless a host sets it, so every existing deployment is
byte-for-byte unchanged and the reference server passes `None`. It is threaded
exactly like `gateway_key` — carried through `TurnRequest` to the provider
context, never read by the runner — and just as uninterpreted.

That opacity is the point: it keeps the POLICY with the host. smooai will put a
short-lived, narrowly scoped minted token here rather than the raw session
bearer, so a leak costs five minutes and one scope instead of a reusable user
session. A different host may forward something else. This crate does not know
which, and must not: minting, TTL and scope are decisions only the host can make.

Guarded by `user_token_is_absent_by_default`, whose negative control is the
security-relevant one — flipping the default to `Some` fails it with "no host
opted in, so no credential may be present".
