# Kratos

A desktop workspace for coding agents. It runs the Codex and Claude Code CLIs you already have installed and signed
in, and keeps every conversation in one app that you own instead of in each provider's app.

Kratos is early and under active development. The app shell, thread storage and the Codex connection work today.
Running agents from the app's chat is in progress.

## What it is for

- **Your threads, in one place.** Conversations live in Kratos's own database, grouped by project, with each
  message showing which provider and model wrote it.
- **Providers are the workers.** Each thread runs on one provider through that provider's own CLI, so you keep its
  tools, sandbox and your existing subscription. You can switch models within the provider, and continue a thread on
  another provider as a new linked thread.
- **Queue and steer.** Messages sent while the agent is busy wait in a queue, or steer the running turn.
- **Any machine.** The engine runs in the background and keeps agents working after the window closes. Other
  machines can open the same threads.
- **One set of skills and plugins** shared by every provider.
- **No wasted tokens.** Helper agents run on cheaper models, nothing polls while it waits, and long threads move to a
  fresh session from a summary instead of waiting minutes for compaction.

## Status

| Area                                                                | State       |
| ------------------------------------------------------------------- | ----------- |
| Desktop app that starts the engine and keeps it running             | Working     |
| Workspaces, threads and an append-only message history              | Working     |
| Live updates to every open window, with catch-up after a reconnect  | Working     |
| Typed Codex connection: start, requests, events, approvals, cleanup | Working     |
| Codex skills, plugins, models and sign-in status                    | Working     |
| Sending chat messages to Codex, with queue and streaming replies    | In progress |
| Context window tracking and per-thread model and window settings    | Planned     |
| Approvals, diff viewer, file tree and terminal in the app           | Planned     |
| Claude Code provider                                                | Planned     |
| macOS, and opening threads from another machine                     | Planned     |

## Requirements

- Windows (macOS is planned)
- Node.js 24 and Git
- The Codex CLI, installed and signed in: `npm install -g @openai/codex`, then `codex login`

Kratos finds `codex` on your PATH. To use a different install, set `CODEX_EXE` to its full path.

## Run it

```sh
npm install
npm start       # builds everything and opens the app, which starts the engine if it is not running
npm run dev     # the engine, the UI with hot reload, and the app pointed at it
npm run engine  # the engine on its own, serving the built UI at http://localhost:4200
```

The engine keeps running after the window closes. Its log is `engine.log` in the app's logs folder.
`KRATOS_PORT` (default 4200) and `KRATOS_HOME` (default `~/.kratos`, which holds `kratos.db`) configure it.

## Develop

```sh
npm run check                         # format, lint, typecheck and tests
npm run generate -w @kratos/codex   # refresh the Codex protocol types after a Codex update
```

The tests run against a small fake Codex, so you do not need Codex installed to run them.

## Packages

| Package             | What it is                                                                                   |
| ------------------- | -------------------------------------------------------------------------------------------- |
| `packages/desktop`  | The Electron app. Starts the engine when needed and opens a window on it.                    |
| `packages/engine`   | The background process: SQLite store, JSON and WebSocket API on `127.0.0.1`, provider logic. |
| `packages/ui`       | The React interface the window shows.                                                        |
| `packages/protocol` | Types and schemas shared by the engine and the UI.                                           |
| `packages/codex`    | A typed client for `codex app-server`, with types generated from the installed Codex.        |
