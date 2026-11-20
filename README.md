# Kratos

An engine for coding agents. It runs the Codex and Claude Code CLIs you already have installed and signed in, and
keeps every conversation in one place that you own instead of in each provider's app.

Kratos is early and under active development. It has no app or interface yet. You reach the engine through its JSON
and WebSocket API. The Rust engine runs Codex and Claude Code threads, queues and streams their turns, and sends
their approvals and questions to authenticated clients.

## What it is for

- **Your threads, in one place.** Conversations live in Kratos's own database, grouped by project. Each message
  shows which provider and model wrote it.
- **Providers do the work.** Each thread runs on one provider through that provider's own CLI, so you keep its tools
  and your existing subscription. You can change the model within a provider.
- **Queue and steer.** A message sent while the agent is busy waits in a queue, or steers the running turn.
- **Agents that work together.** A Codex agent can create threads on either provider, message them, and wait for
  their turns.
- **Planned next.** Continuing a thread on another provider, opening threads from another machine, one set of skills
  for every provider, and a sandbox for every provider on every system.

## Status

| Area                                                                  | State   |
| --------------------------------------------------------------------- | ------- |
| Workspaces, threads, and an append-only message history               | Working |
| Live updates to every client, with catch-up after a reconnect         | Working |
| Codex threads: queue, steer, Stop, streaming replies, approvals       | Working |
| Claude Code threads: queue, steer, Stop, streaming replies, approvals | Working |
| Usage limits that pause a thread and resume it at the reset           | Working |
| Agent tools for creating and messaging other threads                  | Working |
| Context window tracking and per-thread model and window settings      | Working |
| Diff viewer, file tree, and terminal                                  | Planned |
| Handing a thread to another provider                                  | Planned |
| Opening threads from another machine                                  | Planned |

## Requirements

- Windows, macOS, or Linux. CI runs the full test suite on all three.
- Rust. `rust-toolchain.toml` pins 1.99.1, and `rustup` installs it on first use.
- Git.
- The Codex CLI, installed and signed in: `npm install -g @openai/codex`, then `codex login`.
- Claude Code 2.1.303 or newer, installed and signed in. An older release pauses its thread and names both releases.
- Node.js 24, only for the TypeScript engine and its tools in `packages/`.

Kratos finds `codex` and `claude` on your PATH. To use a different install, set `CODEX_EXE` or `CLAUDE_EXE` to its
full path.

## Run the engine

```sh
cargo run --release -p kratos-engine   # runs the engine at http://127.0.0.1:4200
```

The engine prints `Kratos engine listening on http://127.0.0.1:<port>` when it is ready, and answers
`GET /api/health` with `{"ok":true}`. `KRATOS_PORT` (default 4200) and `KRATOS_HOME` (default `~/.kratos`, which
holds `kratos.db`) configure it. `KRATOS_PORT=0` picks a free port.

Every route except `/api/health` requires the per-launch token in `<KRATOS_HOME>/client-token`.

### Codex on Windows

Use the carried Codex 0.159.1 sandbox fix. Stock 0.159.1 and 0.166.0 can start commands before workspace read
permissions are ready. Build the fixed helper in a separate install and select it for this terminal:

```powershell
$codexInstall = Join-Path $env:LOCALAPPDATA 'Kratos/codex-0.159.1'
npm install --prefix $codexInstall --ignore-scripts --no-audit --no-fund '@openai/codex@0.159.1'
$nativePath = 'node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc'
$env:CODEX_EXE = Join-Path $codexInstall "$nativePath/bin/codex.exe"
./scripts/build-codex-sandbox.ps1 -CodexExe $env:CODEX_EXE
```

The script builds only the native sandbox helper from a pinned source revision. It keeps external dependencies
locked, preserves Codex's license and notice, and removes its temporary source and build files. CI uses the same
script.

Before it opens a Codex thread on Windows, the engine checks that the elevated sandbox is ready and requests setup
when it is not. If setup fails, queued work stays unsent and the thread says why.

### The data folder

One engine can use a data folder at a time. The engine locks `kratos.lock` in the folder, and a second engine on the
same folder exits with an error. The TypeScript engine takes no lock, so do not run both on one folder.

When the engine starts on a database with an older schema, it first copies the database to
`backups/kratos-v<version>-<id>.db` in the data folder and prints where. `kratos-engine restore <backup-id>` makes a
backup the live database again. It saves the database it replaces as another backup, and it refuses to run while an
engine uses the folder. If `kratos.db` is missing but `kratos.db-wal` or `kratos.db-shm` remains, restore refuses and
keeps those files. Confirm that no SQLite program uses the folder before you move the files aside and retry.

### Drive it from the command line

`kratos-client` prints each reply and event as one JSON line:

```sh
cargo run -p kratos-client -- health
cargo run -p kratos-client -- create-workspace repo C:\work\repo
cargo run -p kratos-client -- send <thread-id> "Explain this repository"
cargo run -p kratos-client -- approve <approval-id> accept
cargo run -p kratos-client -- subscribe <thread-id> --after 0 --count 1
cargo run -p kratos-client -- usage
```

Run `kratos-client` with no arguments to see the other commands. It reads `<KRATOS_HOME>/client-token` for each
request, including after an engine restart, so set `KRATOS_HOME` on the client to the engine's data folder.

## Permissions

Each thread has one of three permissions. Kratos decides every command, file change, and tool request itself.

| Permission     | Codex                                                            | Claude Code                                                      |
| -------------- | ---------------------------------------------------------------- | ---------------------------------------------------------------- |
| Ask me         | Asks before every command and file change                        | Asks before every command and file change                        |
| Approve for me | Asks before a dangerous command or a write outside the workspace | Asks before a dangerous command or a write outside the workspace |
| Full access    | Runs without a sandbox and asks before a dangerous command       | Asks before a dangerous command                                  |

On Ask me and Approve for me, Codex commands run in Codex's sandbox and can write only the thread's workspace and
temporary folders. Codex's sandbox lets them write to the temporary folder without asking.

Claude Code threads use Claude's own shell tools, `Bash` and `PowerShell`. A Claude command runs as the user, with no
sandbox, on every system. A pre-tool hook sends every tool request to Kratos, including the ones Claude Code would
otherwise run without asking. Kratos's guards are its own decision on each request and the dangerous-command check.
The check misses many destructive commands, so an unflagged command can change or delete anything the user can, and
can read Kratos's client token.

Kratos lets Claude load skills and tool definitions without asking, and asks about every tool it does not know. In
plan mode, ordinary reads run without asking, protected reads are denied, and web and shell tools ask.

Under every permission, a write into a protected folder, such as Kratos's own data folder, asks.

Only a Full access thread can create, message, stop, or answer threads in another workspace. Any thread can read
across workspaces.

Kratos never updates a CLI on its own. `POST /api/agents/claude/cli/update` updates Claude Code only when you ask,
and refuses while a Claude thread works or runs a background command.

## Agent tools

Every Codex thread gets a `kratos` MCP server. Codex starts `kratos-engine mcp` for the thread with its id, and the
server calls the engine's API with the client token. An agent can list workspaces and threads, read a thread, create
a thread, send another thread a message labelled with its own id, wait for a thread's turn, stop a turn, answer a
question, and decide an approval.

The server names the acting thread on each HTTP request. For `wait_for_turn`, it subscribes to the target thread on
the authenticated event socket. The engine answers 403 on every route the tools do not call, such as a settings
change, a steer, or a schedule. A new route stays closed to agents until it is added to `agent_routes` in
`crates/kratos-engine/src/server/mod.rs`. On the open routes, the engine checks each request against the agent's
thread:

- A thread an agent creates gets at most the agent's permission.
- Only a Full access thread may decide another thread's approval.
- No request acts on the agent's own thread.
- A question answer is refused if the sender has less access than the receiving turn.

The server runs outside the agent's sandbox, so sandboxed commands still cannot read the token. Tools use the
current turn's permission, even when the user has saved different settings for the next turn. MCP connections keep
their original client token and must restart with the engine.

A message from another thread runs in a turn of its own. When it starts, the receiving thread's settings drop to the
narrower of the two threads' permissions, and to plan mode if either thread plans. The settings stay there after the
turn, because the agent remembers the request, until the user raises them. Codex sees the message after a label that
names the sender and its permission. Such a message cannot be steered into a running turn. Before a narrower turn
starts, Kratos forks the native Codex session with the same history. The fork drops command and file approvals
granted to the previous session, and the Kratos thread keeps its id.

## Develop

```sh
cargo test --workspace --locked
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
lizard -l rust -C 11 -w -x "crates/kratos-command-safety/*" crates tools
```

The tests drive small fakes of Codex and Claude Code from `tools/fake-agents`, so you do not need either CLI to run
them. The `real_codex` tests need the pinned Codex 0.159.1 and a provisioned Windows sandbox home, so only CI runs
them.

## The TypeScript engine

`packages/` holds the earlier TypeScript engine. It stays until the Rust migration deletes it. It serves the storage
API and has a Codex provider that the chat API never connected.

```sh
npm install
npm run build    # bundles the engine into packages/engine/dist
npm run engine   # runs the engine at http://127.0.0.1:4200
npm run check    # format, lint, typecheck, and tests of packages/**
```

## Packages

| Package                        | What it is                                                                                      |
| ------------------------------ | ----------------------------------------------------------------------------------------------- |
| `crates/kratos-engine`         | The engine: the JSON and WebSocket API, the Codex and Claude Code providers, and the scheduler. |
| `crates/kratos-protocol`       | The wire types: the JSON that clients and the engine exchange.                                  |
| `crates/kratos-store`          | The SQLite store, with migrations, backups, and the data-folder lock.                           |
| `crates/kratos-codex-client`   | A typed client for `codex app-server`, the JSON-RPC server inside the Codex CLI.                |
| `crates/kratos-claude-client`  | A typed client for the Claude Code CLI's stream-json protocol, with no Agent SDK.               |
| `crates/kratos-process`        | Child processes whose whole process tree ends with their owner.                                 |
| `crates/kratos-command-safety` | Codex's dangerous-command check, copied from openai/codex.                                      |
| `crates/kratos-teams`          | The Teams rules, as pure functions over a recorded log of facts.                                |
| `crates/kratos-api-client`     | A client for the engine's API, shared by the MCP server, `kratos-client`, and tests.            |
| `tools/kratos-client`          | A command-line client that drives the engine and prints JSON lines.                             |
| `tools/fake-agents`            | Fakes of the Codex and Claude Code CLIs that the tests drive.                                   |
| `packages/engine`              | The earlier TypeScript engine.                                                                  |
| `packages/protocol`            | Types and schemas for the TypeScript engine's API.                                              |
| `packages/codex`               | A typed TypeScript client for `codex app-server`.                                               |
