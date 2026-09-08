#!/usr/bin/env bash
# show-spans.sh — run ONE real agent turn and print the OpenTelemetry spans it emits.
#
# This is the generator for `.github/demo-telemetry.gif`. It exists because the
# original was written ad-hoc, never committed, and was gone by the time the GIF
# needed regenerating for a model change — leaving a "not a mockup" asset in the
# README that nobody could reproduce.
#
#   scripts/show-spans.sh                        # defaults below
#   SMOOTH_AGENT_MODEL=gpt-5.6-luna scripts/show-spans.sh
#
# Requires SMOOAI_GATEWAY_KEY (any OpenAI-compatible gateway). Everything the
# turn touches is in-memory — no Postgres, no collector, nothing to clean up.
#
# With OTEL_EXPORTER_OTLP_ENDPOINT unset the server still emits every span, to
# stdout as tracing-fmt lines; we parse those rather than standing up a
# collector just to read our own output.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
port="${PORT:-8787}"
model="${SMOOTH_AGENT_MODEL:-claude-haiku-4-5}"
gateway_url="${SMOOAI_GATEWAY_URL:-https://llm.smoo.ai/v1}"
prompt="${PROMPT:-How much does shipping cost?}"

: "${SMOOAI_GATEWAY_KEY:?SMOOAI_GATEWAY_KEY is required — the turn has to be real for the spans to be real}"

dump="$(mktemp -t showspans)"
bin="${SERVER_BIN:-$repo/rust/target/debug/smooth-operator-server}"
[[ -x "$bin" ]] || { echo "build first: (cd rust && cargo build -p smooai-smooth-operator-server)" >&2; exit 1; }

# A collector is not optional. The server's fmt layer prints tracing EVENTS;
# spans are only materialised on export, so a span tree can ONLY be read off an
# OTLP consumer. That is what `otelcol:4317` in the demo is.
cid="showspans-otelcol"
docker rm -f "$cid" >/dev/null 2>&1 || true
docker run -d --name "$cid" -p 4317:4317 \
  -v "$repo/scripts/otel/collector.yaml:/etc/otelcol/config.yaml" \
  otel/opentelemetry-collector:latest --config /etc/otelcol/config.yaml >/dev/null

echo "> bash show-spans.sh"
echo
echo "  \$ export OTEL_EXPORTER_OTLP_ENDPOINT=http://otelcol:4317"
echo "  \$ cargo run -p smooai-smooth-operator-server   # one live turn"

for _ in $(seq 1 60); do docker logs "$cid" 2>&1 | grep -q 'Everything is ready' && break; sleep 0.5; done

# Server AFTER the collector: the OTLP exporter connects at boot, so a server
# started first exports into a closed port and the dump comes back empty.
#
# RUST_LOG must let the operator crates through at INFO. `gen_ai.chat` and
# `gen_ai.tool` are info-LEVEL SPANS, so a plain `warn` filter doesn't just hide
# them — the EnvFilter stops them being created at all, and the capture comes
# back empty in a way that looks like a broken exporter.
OTEL_EXPORTER_OTLP_ENDPOINT="http://127.0.0.1:4317" \
SMOOAI_GATEWAY_URL="$gateway_url" \
SMOOTH_AGENT_MODEL="$model" \
SMOOTH_AGENT_SEED_KB=1 \
SMOOTH_AGENT_CONFIRM_TOOLS=knowledge_search \
RUST_LOG="${RUST_LOG:-warn,smooth_operator=info,smooth_operator_server=info}" \
  "$bin" >"$dump.server" 2>&1 &
server=$!
# KEEP=1 leaves the collector container and the server log behind for debugging
# a capture that came back empty.
if [[ "${KEEP:-0}" == "1" ]]; then
  echo "  (KEEP=1 — collector container '$cid' and $dump.server left behind)" >&2
  trap 'kill $server 2>/dev/null || true' EXIT
else
  trap 'kill $server 2>/dev/null || true; docker rm -f "$cid" >/dev/null 2>&1 || true' EXIT
fi

for _ in $(seq 1 100); do
  grep -q 'listening on' "$dump.server" 2>/dev/null && break
  kill -0 $server 2>/dev/null || { echo "server exited:" >&2; tail -20 "$dump.server" >&2; exit 1; }
  sleep 0.2
done
echo "  otel-collector · traces received"
echo

PORT="$port" PROMPT="$prompt" node "$repo/scripts/drive-one-turn.mjs" >/dev/null

# The OTLP exporter BATCHES. Killing the server the moment the turn resolves
# loses the batch and the dump comes back empty — which reads as "telemetry is
# broken" rather than "we didn't wait". Give it the batch window, stop the
# server gracefully so it flushes, then read.
for _ in $(seq 1 30); do
  docker logs "$cid" 2>&1 | grep -q 'gen_ai.chat' && break
  sleep 1
done
kill -TERM $server 2>/dev/null || true
sleep 2
docker logs "$cid" >"$dump" 2>&1

# PARKED=1: SMOOTH_AGENT_CONFIRM_TOOLS is set above, so the turn genuinely
# parked. Drop it and the formatter stops claiming it did.
PARKED=1 node "$repo/scripts/format-spans.mjs" "$dump"
