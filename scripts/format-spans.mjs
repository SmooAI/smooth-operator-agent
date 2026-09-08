/**
 * Render the gen_ai.* spans from an OTel collector `debug` dump as a tree.
 *
 * Input is the collector's stdout (see scripts/otel/collector.yaml), NOT the
 * server log: the server's fmt layer prints tracing EVENTS, and spans are only
 * materialised on export — so a span tree can only be read from a collector.
 * That is why show-spans.sh runs one.
 *
 * Nothing here is invented. Every attribute printed is read out of the dump; a
 * span or attribute the turn didn't produce simply doesn't appear.
 */
import { readFileSync } from 'node:fs';

const dump = readFileSync(process.argv[2] ?? '/dev/stdin', 'utf8');

/** Split the dump into spans: `Span #n` … up to the next one. */
const spans = [];
const lines = dump.split('\n');
let cur = null;
for (const line of lines) {
    if (/^Span #\d+/.test(line.trim())) {
        cur = { name: null, attrs: new Map() };
        spans.push(cur);
        continue;
    }
    if (!cur) continue;
    const nm = line.match(/^\s*Name\s*:\s*(\S+)/);
    if (nm) { cur.name = nm[1]; continue; }
    const at = line.match(/^\s*->\s*([\w.]+):\s*\w+\((.*)\)\s*$/);
    if (at) cur.attrs.set(at[1], at[2]);
}

const find = (n) => spans.find((s) => s.name === n);
const chat = find('gen_ai.chat');
if (!chat) {
    console.error('no gen_ai.chat span in the dump — did the turn run, and did the collector receive it?');
    process.exit(1);
}
const tool = find('gen_ai.tool');

/** Display order. Attributes absent from the span are skipped, never faked. */
const CHAT_KEYS = [
    'gen_ai.system',
    'gen_ai.request.model',
    'gen_ai.agent.name',
    'gen_ai.conversation.id',
    'smooai.org_id',
    'gen_ai.usage.input_tokens',
    'gen_ai.usage.output_tokens',
    'gen_ai.response.id',
];
const TOOL_KEYS = ['gen_ai.tool.name', 'gen_ai.tool.call.arguments'];

const C = { dim: '\x1b[2m', cyan: '\x1b[36m', gold: '\x1b[33m', green: '\x1b[32m', reset: '\x1b[0m' };

const tree = (span, keys, indent, moreFollows = false) => {
    const present = keys.filter((k) => span.attrs.has(k));
    return present
        .map((k, i) => {
            // `moreFollows` when a caller appends its own row underneath, so the
            // last attribute stays a ├─ and only the real last line gets └─.
            const branch = i === present.length - 1 && !moreFollows ? '└─' : '├─';
            const v = span.attrs.get(k);
            const val = /^\d+$/.test(v) ? `${C.gold}${v}${C.reset}` : v;
            return `${indent}${C.dim}${branch}${C.reset} ${k.padEnd(28)} ${val}`;
        })
        .join('\n');
};

console.log(`  ${C.cyan}◆ gen_ai.chat${C.reset}${' '.repeat(22)}${C.dim}span${C.reset}`);
console.log(tree(chat, CHAT_KEYS, '     '));

if (tool) {
    // Only claim the park if the turn actually parked — SMOOTH_AGENT_CONFIRM_TOOLS
    // gates it, and running without that env produces a tool span that never waited.
    const parked = process.env.PARKED === '1';
    console.log('');
    console.log(`       ${C.cyan}◆ gen_ai.tool${C.reset}${parked ? `  ${C.dim}(child span — parked for approval, then approved)${C.reset}` : ''}`);
    console.log(tree(tool, TOOL_KEYS, '          ', tool.attrs.has('gen_ai.tool.call.arguments')));
    if (tool.attrs.has('gen_ai.tool.call.arguments')) {
        console.log(`          ${C.dim}└─${C.reset} ${C.gold}↳ secrets redacted${C.reset}`);
    }
}
console.log('');
console.log(`  ${C.green}✓${C.reset} gen_ai.* OpenTelemetry — emitted by all five servers`);
