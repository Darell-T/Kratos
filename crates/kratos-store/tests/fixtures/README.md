# TypeScript engine fixture

`typescript-v4.db` is a schema version 4 database that the TypeScript engine's own `Store` wrote at commit
`e36cffd`, with the clock and the UUIDs pinned. `typescript-v4.json` is what that engine served from it. It
holds the body of `GET /api/workspaces`, the body of `GET /api/threads?workspaceId=…` for each workspace, and the
events a socket receives after `subscribe` with `after: 0` for each thread.

`tests/store.rs` copies the database, opens the copy with the Rust `Store`, and compares everything the store
returns with the JSON file. It also compares the database's `sqlite_master` with the one the Rust migrations
build. Together they show that the two engines read each other's databases.

Everything in the fixture is synthetic: two workspaces, three threads (one has no events), and five events. The
text covers a quote, a backslash, a newline, a tab, a control character, U+2028, an emoji, and text that looks
like JSON and HTML. The database is one file because closing the last connection checkpoints the write-ahead log.

## Regenerate the fixture

This needs Node, `npm ci`, and a commit that still has `packages/engine`. The repository keeps no copy of the
script, so nothing in the Rust tree depends on `packages/`.

1. Save the script below as `packages/engine/test/write-typescript-fixture.test.ts`.
2. From the repository root, set `FIXTURE_OUT` to the absolute path of this folder and run
   `npx vitest run packages/engine/test/write-typescript-fixture.test.ts`.
3. Delete the script.

```ts
import { copyFileSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { expect, it, vi } from 'vitest';
import { WebSocket } from 'ws';
import { Hub } from '../src/hub';
import { startServer } from '../src/server';
import { Store } from '../src/store';

// The TypeScript engine's Store makes every id with randomUUID, so pin it.
const counter = vi.hoisted(() => ({ next: 0 }));
vi.mock('node:crypto', async (importOriginal) => ({
  ...(await importOriginal<typeof import('node:crypto')>()),
  randomUUID: () => `00000000-0000-4000-8000-${String(++counter.next).padStart(12, '0')}`,
}));

const outputFolder = process.env.FIXTURE_OUT ?? '';

async function get(port: number, path: string): Promise<unknown> {
  return (await fetch(`http://127.0.0.1:${port}${path}`)).json();
}

// What the engine sends when a client subscribes after 0: every stored event of the thread, in order.
function replay(port: number, threadId: string, expected: number): Promise<unknown[]> {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(`ws://127.0.0.1:${port}/api/events`);
    const events: unknown[] = [];
    const finish = () => {
      socket.terminate();
      resolve(events);
    };
    socket.once('error', reject);
    socket.on('open', () => socket.send(JSON.stringify({ type: 'subscribe', threadId, after: 0 })));
    socket.on('message', (data) => {
      events.push((JSON.parse((data as Buffer).toString('utf8')) as { event: unknown }).event);
      if (events.length === expected) finish();
    });
    if (expected === 0) setTimeout(finish, 300);
  });
}

it('writes typescript-v4.db and typescript-v4.json', async () => {
  expect(outputFolder).not.toBe('');
  const home = mkdtempSync(join(tmpdir(), 'kratos-fixture-'));
  const store = new Store(home);

  // One second between writes, from a fixed start.
  vi.useFakeTimers({ toFake: ['Date'] });
  let seconds = 0;
  const tick = () => vi.setSystemTime(new Date(Date.parse('2030-01-01T00:00:00.000Z') + ++seconds * 1000));

  tick();
  const repo = store.createWorkspace({ name: 'demo repo', path: 'C:\\work\\demo repo' });
  tick();
  const unicode = store.createWorkspace({ name: '名前 🚀', path: '/work/日本語' });
  tick();
  const codex = store.createThread({ workspaceId: repo.id, title: 'First "chat"', provider: 'codex' });
  tick();
  const claude = store.createThread({ workspaceId: repo.id, title: 'Second chat', provider: 'claude' });
  tick();
  const quiet = store.createThread({ workspaceId: unicode.id, title: 'Quiet thread', provider: 'claude' });

  const messagesByThread: [string, string[]][] = [
    [
      codex.id,
      [
        'plain text',
        'quote " backslash \\ newline \n tab \t',
        'control \u0001 separator \u2028 emoji 🚀 accent é',
      ],
    ],
    [claude.id, ['{"looks":"like json"}', '</script><b>&amp;']],
  ];
  for (const [threadId, messages] of messagesByThread) {
    for (const text of messages) {
      tick();
      store.appendEvent(threadId, { kind: 'user.message', text });
    }
  }
  vi.useRealTimers();

  const server = await startServer({ store, hub: new Hub(), port: 0 });
  const workspaces = (await get(server.port, '/api/workspaces')) as { id: string }[];
  const threads: Record<string, unknown> = {};
  for (const workspace of workspaces) {
    threads[workspace.id] = await get(server.port, `/api/threads?workspaceId=${workspace.id}`);
  }
  const eventCounts = new Map(messagesByThread.map(([id, messages]) => [id, messages.length]));
  const events: Record<string, unknown[]> = {};
  for (const thread of [codex, claude, quiet]) {
    events[thread.id] = await replay(server.port, thread.id, eventCounts.get(thread.id) ?? 0);
  }
  await server.close();
  store.close();

  // Closing the last connection checkpoints the log, so the database is one file.
  expect(readdirSync(home)).toEqual(['kratos.db']);
  copyFileSync(join(home, 'kratos.db'), join(outputFolder, 'typescript-v4.db'));
  writeFileSync(join(outputFolder, 'typescript-v4.json'), `${JSON.stringify({ workspaces, threads, events }, null, 2)}\n`);
  rmSync(home, { recursive: true, force: true });
});
```
