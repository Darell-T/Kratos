# R4: what the `finish-claude-provider` branch does

This branch is R4 of `docs/rust-migration-plan.md` on branch `rust-migration-plan`. It lets the Rust engine run Claude Code threads with no Node and no Claude Agent SDK. The engine starts the user's own `claude` CLI, speaks its stream-json protocol, decides every tool permission itself, and runs Claude's commands in Codex's sandbox.

Use this report to read the diff against `origin/main`. It says what each part is for, where it lives, and how it was checked, so you can tell intended changes from drift.

```bash
git diff origin/main..finish-claude-provider
```

## Size and shape

The branch holds 61 commits on `origin/main` (da30d7e). It changes 198 files, with about 21,700 lines added and 1,000 deleted. About 3,000 of the added lines are scrubbed CLI recordings under `crates/kratos-claude-client/tests/transcripts/`.

| Area | Lines | What it is |
| --- | --- | --- |
| `crates/kratos-claude-client/src` | 2,200 | New crate. The CLI driver and protocol types. |
| `crates/kratos-engine/src/providers/claude` | 5,600 | New module. The Claude thread actor. |
| `crates/kratos-engine/tests/claude` | 5,600 | New tests against the scripted fake CLI. |
| `tools/fake-agents/src/bin/fake-claude` | 650 | New fake CLI: a recording player and a scripted mode. |
| Shared engine, protocol, and store code | about 1,500 | Changes so the server, scheduler, and store serve two providers. |

The work landed in six stages. Each stage was reviewed before the next began.

1. Client crate, `a9aa46b` to `8b69c5e`.
2. Claude threads in the engine, `10eea35` to `4a89295`.
3. Permissions and sandboxed commands, `7ceb1d8` to `d9cd97e`.
4. Usage limits, continuations, slash commands, and context, `063617a` to `8051b3c`.
5. No turn deadline, idle close, live context, and live usage, `4d2bc05` to `95bed63`.
6. CLI release and update, report formats, L7, command items, and this review's fixes, `cfb68fc` to the branch head.

## The client crate: `kratos-claude-client`

The crate starts `claude` and gives the engine typed frames. It has no engine dependency. `PORTING.md` maps it to the TypeScript code it replaces.

- `launch.rs` builds the command line the CLI prototype proved: stream-json in and out, `--permission-prompt-tool=stdio`, partial messages, and replayed user messages. It never uses `-p` or `--bare` (D2, D5). It removes every inherited `CLAUDE*`, `ANTHROPIC*`, and `KRATOS*` variable before it sets its own (L8, D22).
- `connection.rs` sends `initialize` first, puts a deadline on every control request, and answers each request the CLI sends. Each CLI runs in a job object from `kratos-process`, so killing it ends its tool processes too.
- `protocol.rs` and `usage.rs` type the frames Kratos reads: messages, results, control requests, `rate_limit_event`, `get_usage`, and `api_retry`.
- `transcript.rs` reads Claude's session file to learn which messages Claude took. The engine uses it to reconcile after a crash (D11).
- `release.rs` reads the installed release with `claude --version`, the newest one from the native install's feed with the system `curl`, and runs `claude update` for the native install only.

## The Claude thread actor: `providers/claude/`

One actor owns every Claude thread, the way the Codex provider does. Its inputs are commands from handles, frames from the CLIs, and its own finished work. One handler runs at a time, and a handler awaits only the store, never a CLI. `actor.rs` holds the loop, and `mod.rs` is the handle the routes call.

| File | Purpose |
| --- | --- |
| `session.rs` | Starts a CLI when a message needs one, resumes the thread's session, and restarts with a doubling wait after failures (L4). It closes a session nobody used for 30 minutes. |
| `turns.rs`, `queue.rs` | Send queued messages, steer, and stop under the two delivery rules from R3 (D11). |
| `incoming.rs`, `wire.rs` | Turn Claude's frames into thread events and live updates. |
| `ends.rs` | Record how a turn ended: its outcome, report, context, and whether it hit the usage limit, in one write. |
| `recovery.rs` | At startup, settle each unfinished thread from Claude's transcript (L1). |
| `permissions.rs`, `policy.rs`, `paths.rs` | The D21 rule table. Each tool request is parsed, its paths resolved, then one pure function decides. |
| `tools.rs`, `hooks.rs` | The in-process `kratos` tool server. `run_command` runs through Codex's command runner in the thread's sandbox profile (D17), and flagged commands go to the user (D18). |
| `requests.rs` | Approvals and questions, each settled once (D83). |
| `limits.rs`, `account.rs` | Usage per Claude account, limit stops, and continuations (D38, D39, L6). |
| `release.rs` | The oldest release Kratos runs (2.1.303) and the update route's side. |

## Changes to code that already existed

These are the edits to look at hardest, because they touch Codex threads and shared paths.

- **One map from provider to agent.** `providers/agent.rs` (`Providers`, `Agent`) and `server/agent.rs` replace the Codex-typed `ServerState.provider`. A route reaches a thread's provider through the thread's own `provider` field, so routes are written once for both providers. A thread whose provider cannot run gets 503 with the reason.
- **Shared queue rules.** `providers/queue.rs` and `ProviderError` moved out of the Codex module, so both providers use them (`d96586c`).
- **One scheduler for both providers.** `scheduler.rs` continues a limit-stopped turn for either provider (`063617a`).
- **No turn deadline.** The 10-minute turn timer is gone for Codex and for Claude (`4d2bc05`). A live CLI is working, and each way it can die reports its own error. Zeron has no deadline either. One Codex test of the removed path was deleted, with the owner's approval. `PauseReason::TimedOut` stays readable, because an older engine may have stored it.
- **Report formats on Codex.** A Codex turn sends `outputSchema` and now ends with an `AgentItem::Report`, the same item a Claude turn records.
- **Live usage over the socket.** A client sends `watchUsage` and receives a `usage` message on each change (`bbe5398`).
- **Command safety.** `-Force` written as a prefix such as `-Fo`, or with an en dash or em dash, now counts as a forced delete (`cfb68fc`). This change is security-sensitive.

## Wire and storage changes

| Kind | Change |
| --- | --- |
| Routes | `GET /api/agents/claude/cli` and `POST /api/agents/claude/cli/update`. Both are user routes, so agents cannot reach them. |
| Thread settings | `reportFormat`, a JSON Schema object. It is skipped when unset, so stored settings from before it read the same. |
| Thread items | `AgentItem::Report`. Claude threads also publish the existing `LiveUpdate::Context` while a turn runs, and record each command Kratos runs as `AgentItem::Command` with its output. |
| Socket | `watchUsage`, `unwatchUsage`, and `usage`. |
| Skills | `ThreadSkills` gains `slashCommands`, a list of `SlashCommand`. A Claude thread fills it from its session (D97). Codex still reports none and says why. |
| Store | Migration `0007_report_format.sql` adds `threads.report_format`. Schema version 7. |

`packages/protocol` did not change. The TypeScript protocol has been frozen since R2 and goes away in R9.

## Decisions this branch made on its own

These choices are not written in the plan. Each one is in its commit message.

- **Old CLIs pause instead of failing.** An installed release older than 2.1.303 pauses the thread as unavailable and names both releases. Kratos offers the update instead of refusing. A release Kratos cannot read does not stop a start.
- **Kratos updates only the native install.** An npm, Homebrew, or WinGet updater replaces files a running CLI still loads, so those installs get 400 naming their manager.
- **A thread's first session id comes from the thread id.** It is a UUID v5 of the thread id, so an engine that stops after Claude took the first message finds that session again (L7). This needed no migration.
- **Only a failed turn after a `rejected` event is a limit stop.** A CLI that waits out the limit and resumes by itself gets nothing sent (D39).
- **Claude processes close after 30 idle minutes,** as Zeron's do. The next message resumes the session.

## Still open

### Blocks the merge

- **Comparison against the reference SDK.** This is the parity evidence for the new provider. Run the SDK provider's tests at trunk on the same Haiku cases as lanes 1 to 9, and save `r4-regression.jsonl` with both sides.
- **The Windows `real-codex` sandbox tests (D99).** The sandbox is one of Kratos's highest-risk areas. The job needs the pinned Codex 0.159.1 and a provisioned sandbox home, so it runs only in CI. CI runs on every pull request, drafts included.
- **Linux and macOS CI.** Every Claude test so far ran on Windows. The fake CLI's Unix `close_stdout` has never run.

### Can follow the merge

- **Live lane 10, a real usage limit.** It closes only when a limit happens on its own, so it gates nothing. Condition: an unrecognized limit response must fail loudly, never hang or retry without end, with a test that proves it. Not done yet.
- **Skills.** They wait on the unified skills design. Condition: nothing in R4 may fix how skills will work. `docs/unified-skills.md` records the open questions. The audit of what R4 commits to is not done yet.
- **`kratos-client bench-post`.** Stage 2 deleted it. The owner decides whether it stays deleted.

### Update rule for every provider

Kratos never updates a CLI on its own and never under a running session. When an update is available, Kratos tells the user, and the user decides. For Claude Code, `GET /api/agents/claude/cli` reports the newest release, `POST /api/agents/claude/cli/update` runs only when the user asks, it refuses while a Claude thread works, and every session starts with `DISABLE_AUTOUPDATER=1`. Codex has no update route yet, and Kratos never updates it.

## How it was checked

- **Tests.** The full workspace suite passes: 844 passed, 0 failed, and 9 ignored, of which 7 need the pinned Codex and run in CI, 1 runs only as another test's child process, and 1 is the hand-run perf probe. It includes 85 Claude engine tests against the scripted fake CLI, and client tests that replay the scrubbed recordings. `transcripts_are_scrubbed` fails on a recording that holds an email address, a user path, or a session id.
- **Gates.** `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings`, and `lizard -C 11` are clean.
- **Live lanes.** Lanes 1 to 9 passed against the real CLI (2.1.322) with Haiku at `2827d59`. They covered approval, interrupt, resume after restart, a denied write, `run_command`, a declined dangerous command, settings isolation, a question, and a killed CLI.
- **Performance.** The budgets were written before measuring. With the fake CLI, stream forwarding is 1.8 ms at p95 and dispatch is 5.8 ms at p95. Over ten real Haiku turns, the first token arrives in 524 ms at the median. One `claude.exe` runs per thread, engine memory stayed near 18 MB, and no process outlived a killed engine.
- **Reviews.** Each stage had a Review.MD review and a security review. A final Review.MD, rust-review, and thermo-nuclear pass covered stages 5 and 6 and found four issues, which are fixed on the branch.

## A reading order for the review

1. `crates/kratos-claude-client/src/lib.rs`, then `connection.rs`, for the driver's contract.
2. `crates/kratos-engine/src/providers/claude/actor.rs` and `mod.rs`, for the actor's inputs and handle.
3. `providers/agent.rs` and `server/agent.rs`, for how routes and the scheduler reach either provider.
4. `providers/claude/policy.rs` and `tools.rs`, for the permission and sandbox boundary.
5. `providers/claude/limits.rs` with `scheduler.rs`, for usage limits.
