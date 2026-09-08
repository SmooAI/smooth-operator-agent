/**
 * Drive exactly one agent turn against a local smooth-operator server, so
 * `show-spans.sh` has a real turn to emit spans for.
 *
 * Raw frames rather than the TS client on purpose: this runs from a bare repo
 * checkout with no pnpm install, and the frames it needs are stable protocol
 * (`spec/actions/{create-conversation-session,send-message,confirm-tool-action}`).
 *
 * Two things the schemas do NOT tell you, both of which cost a debugging round:
 *   - `sessionId` comes back NESTED under `data`, not at the top level.
 *   - `requestId` is listed as optional on send_message but the server rejects
 *     the frame without one (`VALIDATION_ERROR`).
 */
import { randomUUID } from 'node:crypto';

const port = process.env.PORT ?? '8787';
const prompt = process.env.PROMPT ?? 'How much does shipping cost?';
const ws = new WebSocket(`ws://127.0.0.1:${port}/ws`);
const send = (o) => ws.send(JSON.stringify(o));

const turnId = randomUUID();
let sessionId = null;
let sent = false;
let tokens = 0;

const done = new Promise((resolve, reject) => {
    // The turn is the whole point; fail loudly rather than hang a capture.
    const timer = setTimeout(() => reject(new Error('turn did not complete in 120s')), 120_000);
    ws.addEventListener('open', () => send({ action: 'create_conversation_session', requestId: randomUUID(), agentId: 'demo-agent', userName: 'Demo' }));
    ws.addEventListener('error', (e) => { clearTimeout(timer); reject(new Error(`ws error: ${e.message ?? e}`)); });
    ws.addEventListener('message', (ev) => {
        const msg = JSON.parse(ev.data);
        const type = msg.type ?? msg.action ?? '?';
        if (type === 'stream_token') { tokens++; return; }
        process.stderr.write(`  ← ${type}\n`);

        sessionId ??= msg.data?.sessionId ?? msg.sessionId ?? null;
        if (sessionId && !sent) {
            sent = true;
            send({ action: 'send_message', requestId: turnId, sessionId, message: prompt, stream: true });
            return;
        }
        // SMOOTH_AGENT_CONFIRM_TOOLS gates knowledge_search, so the turn parks
        // here. Approving is what produces the child tool span at all.
        if (type === 'write_confirmation_required') {
            send({ action: 'confirm_tool_action', requestId: msg.requestId ?? msg.data?.requestId ?? turnId, sessionId, approved: true });
            return;
        }
        if (type === 'eventual_response' || type === 'error') { clearTimeout(timer); resolve(msg); }
    });
});

const final = await done;
ws.close();
process.stderr.write(`  ${tokens} tokens streamed\n`);
if ((final.type ?? '') === 'error') {
    process.stderr.write(`turn failed: ${JSON.stringify(final.error ?? final)}\n`);
    process.exit(1);
}
