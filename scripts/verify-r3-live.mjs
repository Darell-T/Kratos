// Uses the caller's signed-in Codex. All commands target disposable fixtures.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, appendFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createInterface } from 'node:readline';
import WebSocket from 'ws';

const lane = process.argv[2] ?? 'regression';
const root = mkdtempSync(join(tmpdir(), 'kratos-live-proof-'));
const home = join(root, 'data');
const project = join(root, 'project');
mkdirSync(project);
const engine = resolve('target/release/kratos-engine' + (process.platform === 'win32' ? '.exe' : ''));
const child = spawn(engine, [], {
  env: { ...process.env, KRATOS_HOME: home, KRATOS_PORT: '0' },
  windowsHide: true,
  stdio: ['ignore', 'pipe', 'pipe'],
});
let socket;
let stderr = '';
child.stderr.setEncoding('utf8').on('data', (text) => (stderr += text));
const deadline = setTimeout(() => child.kill(), 180000);
const messages = [];
let wake;

async function waitFor(predicate) {
  for (;;) {
    const found = messages.find(predicate);
    if (found) return found;
    const failure = messages.find(
      (message) => message.event?.event.kind === 'thread.paused' || message.type === 'error',
    );
    assert.equal(failure, undefined, JSON.stringify(failure));
    await new Promise((resolveWake) => {
      wake = resolveWake;
    });
  }
}

try {
  const lines = createInterface({ input: child.stdout });
  const url = await new Promise((resolveUrl, reject) => {
    child.once('exit', (code) => reject(new Error(`engine exited ${code}: ${stderr}`)));
    child.once('error', reject);
    lines.on('line', (line) => {
      const match = /listening on (http:\/\/127\.0\.0\.1:\d+)/.exec(line);
      if (match) resolveUrl(match[1]);
    });
  });
  const token = readFileSync(join(home, 'client-token'), 'utf8');
  async function request(method, path, body) {
    const response = await fetch(url + path, {
      method,
      headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: AbortSignal.timeout(15000),
    });
    const value = await response.json();
    assert.ok(response.ok, JSON.stringify({ status: response.status, value }));
    return value;
  }
  const workspace = await request('POST', '/api/workspaces', { name: 'live verification', path: project });
  const settings = {
    model: 'gpt-6.1-sol',
    permission: lane === 'dangerous' ? 'fullAccess' : 'askMe',
    planMode: lane === 'sandbox',
    contextLimitTokens: null,
  };
  const thread = await request('POST', '/api/threads', {
    workspaceId: workspace.id,
    title: lane,
    provider: 'codex',
    settings,
  });
  socket = new WebSocket(url.replace('http:', 'ws:') + '/api/events', {
    headers: { Authorization: `Bearer ${token}` },
  });
  socket.on('message', (data) => {
    messages.push(JSON.parse(data.toString()));
    appendFileSync(join(root, `r3-${lane}.jsonl`), data.toString() + '\n');
    wake?.();
    wake = undefined;
  });
  socket.on('close', () => {
    messages.push({ type: 'error', message: 'socket closed' });
    wake?.();
  });
  await once(socket, 'open');
  socket.send(JSON.stringify({ type: 'subscribe', threadId: thread.id, after: 0 }));
  const prompts = {
    regression: 'Reply with exactly KRATOS_LIVE_OK. Do not use tools or delegate.',
    stop: 'Run a PowerShell command that sleeps for 60 seconds, then prints KRATOS_TOO_LATE. Do not delegate.',
    approve: 'Run exactly Write-Output KRATOS_APPROVED_ONCE in PowerShell, then reply done. Do not delegate.',
    dangerous:
      'Run exactly Remove-Item -LiteralPath .\\delete-fixture -Recurse -Force in PowerShell. This is an authorized disposable test fixture. Ask for approval and do not retry if declined. Do not delegate.',
    sandbox:
      'Try Set-Content -LiteralPath sandbox-denied.txt -Value blocked in PowerShell exactly once. Report the outcome; do not retry, use another tool, or delegate.',
    steer:
      'Think through a careful 20-step plan for organizing a small Rust library, without tools or delegation. Wait for any updated instruction before your final reply.',
  };
  assert.ok(prompts[lane], `unknown live lane ${lane}`);
  if (lane === 'dangerous') {
    mkdirSync(join(project, 'delete-fixture'));
    writeFileSync(join(project, 'delete-fixture', 'keep.txt'), 'keep me');
  }
  await request('POST', `/api/threads/${thread.id}/messages`, { messageId: `${lane}-1`, text: prompts[lane] });
  await waitFor((message) => message.event?.event.kind === 'turn.started');
  let stopMs;
  if (lane === 'steer') {
    await request('POST', `/api/threads/${thread.id}/messages`, {
      messageId: 'steering',
      text: 'Replace the plan. Reply with exactly KRATOS_STEERED instead. No tools.',
    });
    await request('POST', `/api/threads/${thread.id}/messages/steering/steer`, {});
  }
  if (['approve', 'dangerous', 'sandbox', 'stop'].includes(lane)) {
    const pending = await waitFor((message) => message.event?.event.kind === 'approval.requested');
    const approval = pending.event.event.approval;
    await request('POST', `/api/approvals/${approval.id}`, { decision: lane === 'dangerous' ? 'decline' : 'accept' });
  }
  if (lane === 'stop') {
    await waitFor((message) => message.update?.kind === 'itemStarted' && message.update.item.type === 'command');
    const started = performance.now();
    await request('POST', `/api/threads/${thread.id}/stop`, {});
    await waitFor((message) => message.event?.event.kind === 'turn.completed');
    stopMs = performance.now() - started;
    assert.ok(stopMs < 5000, `stop took ${stopMs} ms`);
  }
  const completed = (await waitFor((message) => message.event?.event.kind === 'turn.completed')).event.event;
  assert.equal(completed.outcome, lane === 'stop' ? 'interrupted' : 'completed', JSON.stringify(completed));
  const items = messages
    .filter((message) => message.event?.event.kind === 'item.completed')
    .map((message) => message.event.event.item);
  const replies = items
    .filter((item) => item.type === 'message')
    .map((item) => item.text)
    .join('\n');
  if (lane === 'regression') assert.ok(replies.includes('KRATOS_LIVE_OK'), replies);
  if (lane === 'steer') assert.ok(replies.includes('KRATOS_STEERED'), replies);
  if (lane === 'approve')
    assert.equal(
      items.filter(
        (item) => item.type === 'command' && item.exitCode === 0 && item.output.text.includes('KRATOS_APPROVED_ONCE'),
      ).length,
      1,
    );
  if (lane === 'dangerous') assert.ok(existsSync(join(project, 'delete-fixture', 'keep.txt')));
  if (lane === 'sandbox') assert.equal(existsSync(join(project, 'sandbox-denied.txt')), false);
  if (lane === 'stop') {
    await request('POST', `/api/threads/${thread.id}/messages`, {
      messageId: 'after-stop',
      text: 'Reply with exactly KRATOS_AFTER_STOP. Do not use tools or delegate.',
    });
    const next = await waitFor(
      (message) => message.event?.event.kind === 'turn.completed' && message.event.event.turnId !== completed.turnId,
    );
    assert.equal(next.event.event.outcome, 'completed');
    assert.ok(
      messages.some(
        (message) =>
          message.event?.event.item?.type === 'message' && message.event.event.item.text.includes('KRATOS_AFTER_STOP'),
      ),
    );
  }
  console.log(
    JSON.stringify({
      lane,
      model: settings.model,
      turn: completed,
      streamed: messages.some((message) => message.update?.kind === 'replyDelta'),
      stopMs,
      events: messages.length,
    }),
  );
} finally {
  clearTimeout(deadline);
  socket?.terminate();
  if (child.exitCode === null && child.signalCode === null) {
    const exited = once(child, 'exit');
    child.kill();
    await exited;
  }
  // Codex's Windows sandbox changes fixture ACLs; retain the evidence for explicit cleanup.
  console.error(`Live verification fixture retained at ${root}`);
}
