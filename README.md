# Kratos

Kratos is a local Rust engine for coding agents. It runs the Codex and Claude Code CLIs you already installed and signed in, and it keeps every conversation in one local SQLite database that you own.

Kratos is under active development and has no desktop app yet. Clients talk to the engine over an authenticated local JSON and WebSocket API. The Rust engine runs Codex and Claude Code threads, isolates work in git worktrees, queues and streams turns, and forwards approvals and questions to connected clients.

## What Kratos does

- **Your threads in one place.** Conversations live in Kratos's SQLite database, grouped by workspace. Each message records which provider and model wrote it.
- **Your CLIs and subscriptions.** Each thread runs through the installed Codex or Claude Code CLI, so you keep its tools and your existing subscription. You can change the model within a provider from the next turn.
- **Worktrees and finished work.** A thread can work in the project folder, in a new git worktree on its own branch, or in an existing worktree. From the API or command line, you can read the thread's diff, commit with your own git identity, open a pull request through `gh`, and archive the thread cleanly.
- **Queue and steer.** Messages sent while a turn runs wait in a queue of up to 10 messages, or steer the running turn.
- **Agents that work together.** A Codex thread can create threads on either provider, send them labeled messages, and wait for their turns within permission bounds.
- **Planned next.** Continuing a thread on another provider, running multi-agent Teams loops in the engine, opening threads from another machine, sharing skills across providers, and sandboxing Claude Code commands.

## Status

| Area                                                                                | State   |
| ----------------------------------------------------------------------------------- | ------- |
| Workspaces, threads, and an append-only message history                             | Working |
| Live updates to every client, with catch-up after a reconnect                       | Working |
| Codex threads with queueing, steering, stop, streaming replies, and approvals       | Working |
| Claude Code threads with queueing, steering, stop, streaming replies, and approvals | Working |
| Usage limits that pause a thread and resume it at the reset                         | Working |
| Agent tools for creating and messaging other threads                                | Working |
| Context window tracking and per-thread model and window settings                    | Working |
| Git worktrees per thread, diffs, commits, pull requests, and archive cleanup        | Working |
| Pure Teams rules crate (`kratos-teams`)                                             | Working |
| Teams runner wired into the engine                                                  | Planned |
| Handing a thread to another provider                                                | Planned |
| Desktop app, file tree, and terminal                                                | Planned |
| Opening threads from another machine                                                | Planned |

## Requirements

- Windows, macOS, or Linux. CI runs the full test suite on all three.
- Rust. `rust-toolchain.toml` pins 1.99.1, and `rustup` installs it on first use.
- Git. Install the GitHub CLI (`gh`) and run `gh auth login` if you want Kratos to open pull requests.
- Codex CLI. Install it with `npm install -g @openai/codex`, then sign in with `codex login`.
- Claude Code 2.1.303 or newer, installed and signed in. An older release pauses its thread and names both releases.
- Node.js 24, needed only for the legacy TypeScript engine and its checks in `packages/`.

Kratos looks for `codex` and `claude` on your PATH. To point at another install, set `CODEX_EXE` or `CLAUDE_EXE` to its full path.

## Run the engine

Start the engine with Cargo:

```sh
cargo run --release -p kratos-engine
```

By default, the engine listens at `http://127.0.0.1:4200`, prints `Kratos engine listening on http://127.0.0.1:<port>` when ready, and answers `GET /api/health` with `{"ok":true}`. Set `KRATOS_PORT` to change the port, or set `KRATOS_PORT=0` to pick a free port. Set `KRATOS_HOME` to change the data folder from its default `~/.kratos`.

Every route except `/api/health` requires the per-launch bearer token that the engine writes to `<KRATOS_HOME>/client-token`.

### Set up Codex on Windows

Use the carried Codex 0.159.1 sandbox fix on Windows. Stock 0.159.1 and 0.166.0 can start commands before workspace read permissions are ready. Install Codex 0.159.1 into a separate folder, point `CODEX_EXE` at the native binary, and build the fixed sandbox helper:

```powershell
$codexInstall = Join-Path $env:LOCALAPPDATA 'Kratos/codex-0.159.1'
npm install --prefix $codexInstall --ignore-scripts --no-audit --no-fund '@openai/codex@0.159.1'
$nativePath = 'node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc'
$env:CODEX_EXE = Join-Path $codexInstall "$nativePath/bin/codex.exe"
./scripts/build-codex-sandbox.ps1 -CodexExe $env:CODEX_EXE
```

The script builds only the native sandbox helper from a pinned source revision. It keeps dependencies locked, preserves Codex's license and notice, and removes its temporary source and build files. CI runs the same script.

Before Kratos opens a Codex thread on Windows, the engine checks that the elevated sandbox is ready and starts setup if it is not. If setup fails, queued messages stay unsent and the thread reports why.

### Manage the data folder

Only one engine can use a data folder at a time. The Rust engine locks `kratos.lock` in `<KRATOS_HOME>`, so a second engine on the same folder exits with an error. The legacy TypeScript engine takes no lock, so do not run both engines on one folder.

Before the engine migrates an older database, it writes a consistent copy to `backups/kratos-v<version>-<id>.db` and prints the path. Run `kratos-engine restore <backup-id>` to make a backup live again. Restore saves the replaced database as another backup and refuses to run while an engine uses the folder. If `kratos.db` is missing while `kratos.db-wal` or `kratos.db-shm` remains, restore refuses and leaves those files untouched. Confirm that no SQLite process holds the folder, move those leftover files aside, and retry.

## Work with threads and worktrees

When you create a thread, you choose where it works. Without a place option, the thread works directly in the project folder. Pass `--worktree new` in `kratos-client` (or `{"kind":"newWorktree"}` over HTTP) to create a git worktree under `.kratos-worktrees/` on a new branch named after the thread title. Pass `--worktree <path>` (or `{"kind":"existingWorktree","path":"..."}`) to attach to a worktree that already exists. Kratos treats existing worktrees as borrowed and never deletes them.

Once work is ready, use these finished-work routes through the API or `kratos-client`:

- `diff` (`GET /api/threads/{id}/diff`) returns the patch against the worktree's base commit, or against `HEAD` in the project folder or a borrowed worktree. The patch includes untracked files and leaves out ignored files.
- `commit` (`POST /api/threads/{id}/commit`) stages and commits the thread's folder with your git identity and no agent trailers. Kratos disables git hooks and refuses to commit while a turn runs in that folder or while merge conflicts remain unresolved.
- `pull-request` (`POST /api/threads/{id}/pull-request`) pushes the thread's branch to `origin` and runs your installed `gh` CLI directly with an argument list. It refuses to open a pull request from the repository's default branch.
- `archive` (`POST /api/threads/{id}/archive`) detaches the thread and stops its provider process. If Kratos created the worktree and no other active thread uses it, Kratos cleans up the checkout while keeping the branch.

By default, archiving uses `keep`, which leaves the worktree in place if it holds modified, untracked, or ignored files, or if `HEAD` is detached on an unreachable commit. Clean Kratos-owned worktrees move into `.kratos-trash/` for seven days so open file handles survive until startup cleanup empties expired entries. Archiving with `--files discard` moves the checkout out of `.kratos-worktrees/` and deletes it immediately.

## Drive the engine from the command line

`kratos-client` sends authenticated requests to the engine and prints each reply and event as one JSON line:

```sh
cargo run -p kratos-client -- health
cargo run -p kratos-client -- create-workspace repo C:\work\repo
cargo run -p kratos-client -- create-thread <workspace-id> "Add tests" codex --worktree new
cargo run -p kratos-client -- send <thread-id> "Explain this repository"
cargo run -p kratos-client -- approve <approval-id> accept
cargo run -p kratos-client -- subscribe <thread-id> --after 0 --count 1
cargo run -p kratos-client -- diff <thread-id>
cargo run -p kratos-client -- commit <thread-id> "Add tests"
cargo run -p kratos-client -- pull-request <thread-id> "Add tests"
cargo run -p kratos-client -- archive <thread-id>
cargo run -p kratos-client -- usage
```

Run `kratos-client` with no arguments to list all commands. Set `KRATOS_HOME` on the client to match the engine's data folder so it reads the current `client-token`, including after an engine restart.

## Permissions and safety

Each thread runs with one of three permissions. Kratos decides every command, file edit, and tool request itself.

| Permission     | Codex                                                                  | Claude Code                                                            |
| -------------- | ---------------------------------------------------------------------- | ---------------------------------------------------------------------- |
| Ask me         | Asks before every command and file change                              | Asks before every command and file change                              |
| Approve for me | Asks before a dangerous command or a write outside the thread's folder | Asks before a dangerous command or a write outside the thread's folder |
| Full access    | Runs without a sandbox and asks before a dangerous command             | Asks before a dangerous command                                        |

Under Ask me and Approve for me, Codex runs commands inside Codex's sandbox, which allows writes only in the thread's folder and the system temp folder. Kratos points `npm_config_cache` inside the temp folder for those sandboxed Codex profiles so package installs work without exposing the user's global npm cache. Under Plan mode and Full access, Codex keeps the user's normal npm cache.

Claude Code threads run Claude's own `Bash` and `PowerShell` tools directly as the user, with no sandbox, on every system. A `PreToolUse` hook sends every tool request to Kratos, including commands Claude Code would otherwise allow without asking. Kratos parses Bash and PowerShell commands with tree-sitter, sends unparseable syntax and destructive commands for user approval, and checks PowerShell command names in the syntax tree. Before Kratos allows a plan-mode write, it resolves symlinks, junctions, and Windows UNC paths to verify the real destination. Even with these checks, shell safety checks miss many destructive patterns, so an unflagged Claude command can change or delete anything the user can change and can read `<KRATOS_HOME>/client-token`.

Kratos lets Claude load skills and tool definitions without asking, and asks about any tool it does not recognize. In plan mode, ordinary reads run without asking, Kratos denies protected reads, and web and shell tools ask.

Under every permission, a write into a protected folder asks the user. Protected folders include `<KRATOS_HOME>`, `~/.codex`, `~/.claude`, `~/.ssh`, `~/.gnupg`, `~/.aws`, and `~/.config/gh`.

Only a Full access thread can create, message, stop, or answer threads in another workspace. Any thread can read across workspaces.

Kratos never updates a CLI on its own. `POST /api/agents/claude/cli/update` updates Claude Code only when you request it, refuses while a Claude thread or background command runs, enforces bounded `curl` release downloads with restricted protocols, and verifies the binary SHA-256 digest before running the installer.

## Agent tools

Every Codex thread receives a `kratos` MCP server. Codex launches `kratos-engine mcp` with the acting thread's id, and that process calls the engine API with the client token. Through it, an agent can list workspaces and threads, read a thread, create a thread, send another thread a labeled message, wait for a turn, stop a turn, answer a question, and decide an approval.

The MCP server attaches the acting thread id to every HTTP request. For `wait_for_turn`, it subscribes to the target thread on the authenticated event WebSocket. The engine returns 403 on every route outside `agent_routes` in `crates/kratos-engine/src/server/mod.rs`, including settings updates, steering, scheduling, worktree listing, diffs, commits, pull requests, and archiving. A new route stays closed to agents until you move it into `agent_routes`. On the open routes, the engine enforces these checks against the acting thread:

- A created thread receives at most the acting thread's permission.
- Below Full access, a thread created in the same workspace uses the acting thread's own folder or worktree.
- Only a Full access thread can decide another thread's approval.
- No request can act on the acting thread itself.
- Kratos refuses a question answer if the sender has less access than the receiving turn.

The MCP server runs outside the agent's sandbox, so sandboxed commands cannot read `<KRATOS_HOME>/client-token`. Tools use the current turn's permission even if you saved different settings for the next turn. MCP connections hold the launch token they started with and must restart with the engine.

A message from another thread runs in its own turn and cannot steer a running turn. When that turn starts, the receiving thread drops to the narrower of the two threads' permissions, and into plan mode if either thread is in plan mode. Those narrowed settings persist after the turn until you raise them, because the agent remembers the incoming prompt. Both Codex and Claude Code receive a header before the message naming the sending thread and its permission cap. Before a narrower turn starts, Codex forks its native session with the same history to drop earlier approvals, and Claude reopens its session under the narrower settings. The Kratos thread keeps its id in both cases.

## Develop

Run the same checks CI runs:

```sh
cargo test --workspace --locked
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
lizard -l rust -C 11 -w -x "crates/kratos-command-safety/*" crates tools
```

The workspace tests drive fake Codex and Claude Code binaries in `tools/fake-agents`, so you do not need either real CLI installed to run them. The ignored `real_codex` integration tests require Codex 0.159.1 and a provisioned Windows sandbox home, and run in the Windows CI job.

## Legacy TypeScript engine

`packages/` holds the earlier TypeScript engine until the Rust migration replaces every piece of it. It serves the storage API and has a Codex client that the chat API never wired up.

```sh
npm install
npm run build
npm run engine
npm run check
```

`npm run build` bundles the engine into `packages/engine/dist`. `npm run engine` starts it at `http://127.0.0.1:4200`. `npm run check` runs formatting, lint, typechecking, and tests for `packages/**`.

## Packages

| Package                        | Purpose                                                                                                      |
| ------------------------------ | ------------------------------------------------------------------------------------------------------------ |
| `crates/kratos-engine`         | Local HTTP and WebSocket server, Codex and Claude Code providers, worktrees, MCP server, and usage scheduler |
| `crates/kratos-protocol`       | Wire types exchanged between clients and the engine                                                          |
| `crates/kratos-store`          | SQLite store, schema migrations, migration backups, and the data-folder lock                                 |
| `crates/kratos-codex-client`   | Typed JSON-RPC client for `codex app-server`                                                                 |
| `crates/kratos-claude-client`  | Typed client for the Claude Code stream-json protocol and explicit CLI updater                               |
| `crates/kratos-process`        | Child process management that terminates the whole process tree with its owner                               |
| `crates/kratos-command-safety` | Dangerous-command checks for Bash and PowerShell, based on `openai/codex`                                    |
| `crates/kratos-teams`          | Pure Teams workflow rules, prompt builders, permissions, and security triggers over a fact log               |
| `crates/kratos-api-client`     | HTTP and WebSocket client shared by `kratos-engine mcp`, `kratos-client`, and integration tests              |
| `tools/kratos-client`          | Command-line client that drives the engine and prints JSON lines                                             |
| `tools/fake-agents`            | Fake Codex, Claude Code, git, and `gh` binaries used by tests                                                |
| `packages/engine`              | Legacy TypeScript engine                                                                                     |
| `packages/protocol`            | Types and schemas for the legacy TypeScript API                                                              |
| `packages/codex`               | Legacy TypeScript client for `codex app-server`                                                              |
