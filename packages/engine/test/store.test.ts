import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Store } from '../src/store';

let home: string;
const open: Store[] = [];

function openStore(): Store {
  const store = new Store(home);
  open.push(store);
  return store;
}

function seedThread(store: Store, title: string) {
  const workspace = store.createWorkspace({ name: 'repo', path: '/tmp/repo' });
  return store.createThread({ workspaceId: workspace.id, title, provider: 'codex' });
}

beforeEach(() => {
  home = mkdtempSync(join(tmpdir(), 'kratos-store-'));
});

afterEach(() => {
  for (const store of open.splice(0)) {
    try {
      store.close();
    } catch {
      // already closed by the test
    }
  }
  rmSync(home, { recursive: true, force: true });
  vi.useRealTimers();
});

describe('Store', () => {
  it('numbers events per thread independently, starting at 1', () => {
    const store = openStore();
    const first = seedThread(store, 'first');
    const second = seedThread(store, 'second');

    const seqs = [
      store.appendEvent(first.id, { kind: 'user.message', text: 'a' }).seq,
      store.appendEvent(first.id, { kind: 'user.message', text: 'b' }).seq,
      store.appendEvent(second.id, { kind: 'user.message', text: 'c' }).seq,
    ];

    expect(seqs).toEqual([1, 2, 1]);
    expect(store.eventsAfter(first.id, 0).map((e) => e.event)).toEqual([
      { kind: 'user.message', text: 'a' },
      { kind: 'user.message', text: 'b' },
    ]);
    expect(store.eventsAfter(first.id, 1).map((e) => e.seq)).toEqual([2]);
    expect(store.eventsAfter(first.id, 2)).toEqual([]);
  });

  it('keeps events across closing and reopening the database', () => {
    const store = openStore();
    const thread = seedThread(store, 'persisted');
    store.appendEvent(thread.id, { kind: 'user.message', text: 'before restart' });
    store.close();

    const reopened = openStore();
    expect(reopened.getThread(thread.id)).toEqual(expect.objectContaining({ id: thread.id, title: 'persisted' }));
    expect(reopened.eventsAfter(thread.id, 0).map((e) => e.event)).toEqual([
      { kind: 'user.message', text: 'before restart' },
    ]);
    expect(reopened.appendEvent(thread.id, { kind: 'user.message', text: 'after' }).seq).toBe(2);
  });

  it('bumps the thread updatedAt when an event is appended', () => {
    vi.useFakeTimers({ toFake: ['Date'] });
    vi.setSystemTime(new Date('2030-01-01T00:00:00.000Z'));
    const store = openStore();
    const thread = seedThread(store, 'clock');

    vi.setSystemTime(new Date('2030-01-01T00:05:00.000Z'));
    store.appendEvent(thread.id, { kind: 'user.message', text: 'tick' });

    expect(store.getThread(thread.id)).toEqual(
      expect.objectContaining({ createdAt: '2030-01-01T00:00:00.000Z', updatedAt: '2030-01-01T00:05:00.000Z' }),
    );
  });

  it('rejects events for an unknown thread without writing anything', () => {
    const store = openStore();
    expect(() => store.appendEvent('missing', { kind: 'user.message', text: 'x' })).toThrow(/Unknown thread/);
    expect(store.eventsAfter('missing', 0)).toEqual([]);
  });

  it('lists threads only for their own workspace', () => {
    const store = openStore();
    const mine = store.createWorkspace({ name: 'mine', path: '/tmp/mine' });
    const other = store.createWorkspace({ name: 'other', path: '/tmp/other' });
    const thread = store.createThread({ workspaceId: mine.id, title: 'here', provider: 'claude' });
    store.createThread({ workspaceId: other.id, title: 'elsewhere', provider: 'codex' });

    expect(store.listThreads(mine.id)).toEqual([thread]);
  });
});
