# `show-spans.sh` — regenerating the telemetry demo

Runs **one real agent turn** against a local server and prints the
OpenTelemetry spans it emits. This is the generator for
[`.github/demo-telemetry.gif`](../.github/demo-telemetry.gif).

```bash
(cd rust && cargo build -p smooai-smooth-operator-server)

SMOOAI_GATEWAY_KEY=sk-… SMOOTH_AGENT_MODEL=gpt-5.6-luna scripts/show-spans.sh
```

Everything is in-memory: no Postgres, no persistent state. The only external
pieces are your LLM gateway and a throwaway OTel collector container.

| env | default | |
| --- | --- | --- |
| `SMOOAI_GATEWAY_KEY` | **required** | the turn has to be real for the spans to be real |
| `SMOOTH_AGENT_MODEL` | `claude-haiku-4-5` | appears verbatim as `gen_ai.request.model` |
| `SMOOAI_GATEWAY_URL` | `https://llm.smoo.ai/v1` | any OpenAI-compatible `/v1` |
| `PROMPT` | `How much does shipping cost?` | hits the seeded shipping doc |
| `SERVER_BIN` | `rust/target/debug/…` | set if you build to a custom `CARGO_TARGET_DIR` |
| `KEEP` | `0` | `1` leaves the collector + server log for debugging |

## Why it is shaped this way

**It needs a collector — that is not incidental.** The server's `fmt` layer
prints tracing *events*; spans are only materialised on export. So a span tree
can only be read off an OTLP consumer, which is why the demo shows
`otelcol:4317`. `scripts/otel/collector.yaml` is a throwaway collector whose
only job is to print what it receives.

**Three things will hand you an empty capture,** each looking like broken
telemetry rather than a harness mistake:

1. **`RUST_LOG` below `info` for the operator crates.** `gen_ai.chat` and
   `gen_ai.tool` are info-level *spans* — a `warn` filter doesn't hide them, it
   stops them being created. The script's default allows them through; override
   `RUST_LOG` and you own this.
2. **Starting the server before the collector.** The OTLP exporter connects at
   boot, so it exports into a closed port and never retries into your capture.
3. **Killing the server the moment the turn resolves.** The exporter batches;
   the script waits for the span to land, then stops the server gracefully.

Two protocol details the schemas don't tell you, both in `drive-one-turn.mjs`:
`sessionId` comes back **nested under `data`**, and `requestId` is listed as
optional on `send_message` but the server rejects the frame without one.

## Recording the GIF

Run it in a terminal sized to the asset (the committed one is 1180×560), then
capture and encode. Keep it **real time** — stretching playback misrepresents
how long a turn takes.
