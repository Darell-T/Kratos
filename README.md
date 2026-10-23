# Kratos

An engine for coding agents. It runs the Codex and Claude Code CLIs you already have installed and signed in, and
keeps every conversation in one place that you own instead of in each provider's app.

Kratos is early and under active development. It has no app or interface yet. You reach the engine through its JSON
and WebSocket API. Thread storage and the Codex connection work today. Running agents from chat messages is in
progress.

## What it is for

- **Your threads, in one place.** Conversations live in Kratos's own database, grouped by project, with each
  message showing which provider and model wrote it.
- **Providers are the workers.** Each thread runs on one provider through that provider's own CLI, so you keep its
  tools, sandbox and your existing subscription. You can switch models within the provider, and continue a thread on
  another provider as a new linked thread.
- **Queue and steer.** Messages sent while the agent is busy wait in a queue, or steer the running turn.
- **Any machine.** The engine runs in the background and keeps agents working while no client is connected. Other
  machines can open the same threads.
- **One set of skills and plugins** shared by every provider.
- **No wasted tokens.** Helper agents run on cheaper models, nothing polls while it waits, and long threads move to a
  fresh session from a summary instead of waiting minutes for compaction.

## Status

| Area                                                                | State       |
| ------------------------------------------------------------------- | ----------- |
| Workspaces, threads and an append-only message history              | Working     |
| Live updates to every client, with catch-up after a reconnect       | Working     |
| Typed Codex connection: start, requests, events, approvals, cleanup | Working     |
| Codex skills, plugins, models and sign-in status                    | Working     |
| Sending chat messages to Codex, with queue and streaming replies    | In progress |
| Context window tracking and per-thread model and window settings    | Planned     |
| Approvals, diff viewer, file tree and terminal                      | Planned     |
| Claude Code provider                                                | Planned     |
| macOS, and opening threads from another machine                     | Planned     |

## Requirements

- Windows (macOS is planned)
- Node.js 24 and Git
- Rust, to build the Rust engine. `rust-toolchain.toml` pins 1.99.1, and `rustup` installs it on first use.
- The Codex CLI, installed and signed in: `npm install -g @openai/codex`, then `codex login`

Kratos finds `codex` on your PATH. To use a different install, set `CODEX_EXE` to its full path.

## Run it

```sh
npm install
npm run build   # bundles the engine into packages/engine/dist
npm run engine  # runs the engine at http://127.0.0.1:4200
npm run dev     # runs the engine and restarts it when its source changes
```

The engine logs to the terminal it runs in. It answers `GET /api/health` with `{"ok":true}`.
`KRATOS_PORT` (default 4200) and `KRATOS_HOME` (default `~/.kratos`, which holds `kratos.db`) configure it.

## Run the Rust engine

The Rust engine serves the same API as the TypeScript engine, on the same port and with the same data folder. The
TypeScript engine stays until the Rust engine drives Codex and Claude Code.

```sh
cargo run --release -p kratos-engine   # runs the engine at http://127.0.0.1:4200
cargo test --workspace                 # each engine test starts the real binary
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

The engine prints `Kratos engine listening on http://127.0.0.1:<port>` when it is ready. `KRATOS_PORT` and
`KRATOS_HOME` configure it as they do the TypeScript engine, and `KRATOS_PORT=0` picks a free port.

One engine can use a data folder at a time. The Rust engine locks `kratos.lock` in the folder, and a second Rust
engine on the same folder exits with an error. The TypeScript engine takes no lock, so do not run both on one folder.

When the engine starts on a database with an older schema, it first copies the database to
`backups/kratos-v<version>-<id>.db` in the data folder and prints where. `kratos-engine restore <backup-id>` makes a
backup the live database again. It saves the database it replaces as another backup, and it refuses to run while an
engine uses the folder.

`kratos-client` drives either engine and prints each reply and event as one JSON line:

```sh
cargo run -p kratos-client -- health
cargo run -p kratos-client -- create-workspace repo C:\work\repo
cargo run -p kratos-client -- subscribe <thread-id> --after 0 --count 1
cargo run -p kratos-client -- bench-post http://127.0.0.1:4200
```

Run `kratos-client` with no arguments to see the other commands.

## Develop

```sh
npm run check                         # format, lint, typecheck and tests of packages/**
npm run generate -w @kratos/codex   # refresh the Codex protocol types after a Codex update
```

The tests run against a small fake Codex, so you do not need Codex installed to run them. The Rust crates have their
own checks, listed above.

## Codex provider

`packages/engine/src/providers/codex.ts` exports `createCodexProvider`. Importing it does not start Codex or run a
demo. The engine creates one provider with its existing `Store`, and closes the provider before closing the store.
Connecting it to the chat API is still in progress.

```ts
const provider = await createCodexProvider({ store });
const threadId = await provider.startThread(projectFolder);
const message = await provider.sendMessage(
  threadId,
  [{ type: 'text', text: 'Review this project', text_elements: [] }],
  userMessageId,
);
```

Thread IDs are native Codex IDs. Pass a stable message ID to deduplicate repeated submissions. Reusing an ID with
different input throws. Messages sent during a turn stay in the database and run individually in their original
order. `steerTurn(threadId, message.id)` sends that queued message into the current turn instead. It stays queued
if the turn is still starting or cannot accept steering.

`onEvent` delivers structured replies, tool items, turn outcomes, message states, approvals and user requests.
`messages(threadId)` returns the saved submissions. Their delivery states are:

| State        | Meaning                                                         |
| ------------ | --------------------------------------------------------------- |
| `queued`     | Saved and waiting to send                                       |
| `submitting` | Waiting for Codex to acknowledge the input                      |
| `accepted`   | Codex accepted the input; the saved turn ID identifies its turn |
| `failed`     | Codex confirmed a rejection before execution                    |
| `unknown`    | Delivery could not be established                               |
| `cancelled`  | Removed from the delivery queue                                 |

A known pre-execution rejection gets one retry. An uncertain submission is never automatically replayed.
`resumeThread(threadId)` restores saved settings and checks native history for input whose reply was lost.
For a confirmed rejection, use `retryMessage` or `removeQueuedMessage`. For an uncertain submission, use
`discardMessage` after reviewing the native thread. It checks that Codex is idle and checks history again before
discarding. Discarding cannot undo work that may already have run.

A failed or timed-out turn pauses follow-ups. Call `resumeQueue` when continuing them is appropriate.
`waitUntilIdle` rejects when the queue is paused or the thread becomes unavailable. `setThreadModel` applies to
the next turn. `setThreadPermissions` requires an idle thread so the displayed policy matches the running policy.
If saving delivery or a turn outcome fails, the provider closes and settles its waiters. Saved submissions can be
reconciled with native history after storage recovers.

Questions, additional permissions and MCP elicitation have pending-request getters and reply methods:
`answerUserInput`, `decidePermissions` and `answerElicitation`. Command and file approvals use `decideCommand` and
`decideFile`. Requests disappear when resolved, when their turn ends, or when the provider closes.

Requests and initialization have a 30-second deadline. Turns have a 10-minute deadline, extended while waiting for
blocking user input or approval. Stop acknowledgement and completion each have a 5-second deadline. If a
timed-out turn cannot stop, or an acknowledged Stop never completes, the provider closes the shared Codex process
and retains pending submissions. The factory accepts `requestTimeoutMs`, `turnTimeoutMs` and `closeTimeoutMs`.

## Packages

| Package                  | What it is                                                                                   |
| ------------------------ | -------------------------------------------------------------------------------------------- |
| `packages/engine`        | The background process: SQLite store, JSON and WebSocket API on `127.0.0.1`, provider logic. |
| `packages/protocol`      | Types and schemas for the engine's API.                                                      |
| `packages/codex`         | A typed client for `codex app-server`, with types generated from the installed Codex.        |
| `crates/kratos-protocol` | The Rust engine's wire types: the JSON that clients and the engine exchange.                 |
| `crates/kratos-store`    | The Rust engine's SQLite store, with migrations, backups, and the data-folder lock.          |
| `crates/kratos-engine`   | The Rust engine: the same API as `packages/engine`.                                          |
| `tools/kratos-client`    | A command-line client that drives either engine and prints JSON lines.                       |
