// Run with node --import tsx scripts/verify-r3-restore.mjs <kratos-engine executable>.
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import { mkdtempSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createInterface } from 'node:readline';
import { DatabaseSync } from 'node:sqlite';
import { Store } from '../packages/engine/src/store.ts';

const engine = resolve(process.argv[2] ?? 'target/debug/kratos-engine' + (process.platform === 'win32' ? '.exe' : ''));
const home = mkdtempSync(join(tmpdir(), 'kratos-restore-proof-'));
const children = new Set();
const env = { ...process.env, KRATOS_HOME: home, KRATOS_PORT: '0', CODEX_EXE: join(home, 'no-provider') };

async function start(program, args) {
  const child = spawn(program, args, { env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
  children.add(child);
  let errors = '';
  child.stderr.setEncoding('utf8').on('data', (text) => (errors += text));
  const lines = createInterface({ input: child.stdout });
  const ready = new Promise((resolveReady, reject) => {
    child.once('error', reject);
    child.once('exit', (code) => reject(new Error(`engine exited ${code}: ${errors}`)));
    lines.on('line', (line) => {
      const match = /Kratos engine listening on http:\/\/127\.0\.0\.1:(\d+)/.exec(line);
      if (match) resolveReady(`http://127.0.0.1:${match[1]}`);
    });
  });
  const timer = setTimeout(() => child.kill(), 15000);
  try {
    return { child, url: await ready };
  } finally {
    clearTimeout(timer);
  }
}

async function stop(child) {
  if (child.exitCode === null && child.signalCode === null) {
    const exited = once(child, 'exit');
    child.kill();
    await exited;
  }
  children.delete(child);
}

function snapshot() {
  const db = new DatabaseSync(join(home, 'kratos.db'), { readOnly: true });
  try {
    const tables = db
      .prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
      .all();
    return {
      version: db.prepare('PRAGMA user_version').get().user_version,
      tables: Object.fromEntries(
        tables.map(({ name }) => [
          name,
          db
            .prepare(`SELECT * FROM "${name.replaceAll('"', '""')}"`)
            .all()
            .sort((a, b) => JSON.stringify(a).localeCompare(JSON.stringify(b))),
        ]),
      ),
    };
  } finally {
    db.close();
  }
}

try {
  const store = new Store(home);
  const workspace = store.createWorkspace({ name: 'restore proof', path: home });
  const thread = store.createThread({ workspaceId: workspace.id, title: 'preserve this', provider: 'codex' });
  store.appendEvent(thread.id, { kind: 'user.message', text: 'before migration five' });
  const expectedThreads = store.listThreads(workspace.id);
  store.close();
  const before = snapshot();
  assert.equal(before.version, 4);

  const rust = await start(engine, []);
  assert.equal(snapshot().version, 5);
  await stop(rust.child);
  const backups = readdirSync(join(home, 'backups')).filter(
    (name) => name.startsWith('kratos-v4-') && name.endsWith('.db'),
  );
  assert.equal(backups.length, 1);
  const backupId = backups[0].slice('kratos-v4-'.length, -3);
  const restored = spawnSync(engine, ['restore', backupId], { env, windowsHide: true, encoding: 'utf8' });
  assert.equal(restored.status, 0, restored.stderr);
  assert.deepEqual(snapshot(), before, 'restore preserves all v4 tables and rows');

  const reference = await start(process.execPath, ['--import', 'tsx', 'packages/engine/src/main.ts']);
  const response = await fetch(`${reference.url}/api/threads?workspaceId=${workspace.id}`);
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), expectedThreads);
  await stop(reference.child);
  assert.deepEqual(snapshot(), before, 'the reference engine opens the restored database without changing it');
  console.log(
    JSON.stringify({ migration: '4 -> 5 -> restore 4', allTablesPreserved: true, referenceEngineHttpRead: true }),
  );
} finally {
  for (const child of children) await stop(child);
  rmSync(home, { recursive: true, force: true });
}
