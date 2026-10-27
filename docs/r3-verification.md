# R3 verification and follow-up

R3 drives Codex from the Rust engine. It includes durable queues and message identities, streamed replies, approvals,
questions, context settings, usage tracking, continuation scheduling, migration 5, authenticated HTTP/WebSocket routes,
and the corresponding `kratos-client` commands. Claude, worktree ownership, Teams integration, and deleting the
TypeScript engine remain separate migration steps.

## Windows permission limitation to fix later

The user approved this temporary scope on 2026-10-26:

- **Ask me** writes only in the current workspace and temporary folders, after approval.
- **Approve for me** and **Full access** can write in the user's home, temporary folders, and registered workspaces.
- Plan mode remains read-only. Credential folders remain protected, and Kratos's data directory is denied for both
  reads and writes. Full access enables network access; destructive commands still require a decision.

Registered workspace roots refresh at each turn. Switching to Ask me removes the other workspace roots from the next
turn. These settings do not alter a turn already running.

**TODO: restore the intended drive-wide write scope on Windows.** A named Codex profile with `:root = "write"` and
protected-folder exceptions attempts to grant an inheritable write ACE on the system drive. A normal Windows user
cannot make that ACL change. Codex 0.159.1 and 0.164.0 both failed the real harmless-command test with this profile.
The failure appears during sandboxed AGENTS.md discovery as a thread-start timeout, or during execution as
`helper_unknown_error: setup refresh had errors`. The sandbox setup log identifies `SetNamedSecurityInfoW` error 5
on `C:\`.

Until this is fixed, a folder outside the user's home and temporary folders must be registered as a workspace to be
writable in these modes. Do not restore drive-wide writes by removing the data-directory denial, running the engine
as administrator, or weakening the test. A future solution must preserve protected folders and pass the real Windows
command tests as a normal user. The implementation is marked `shortcut:` in `providers/codex/profiles.rs`.

## Reproducible checks

Run the standard gates from the repository root:

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --release --workspace --locked
npm ci
npm run check
python -m lizard -l rust -C 11 -w -x "crates/kratos-command-safety/*" crates tools
```

The real-Codex tests use the pinned native 0.159.1 executable and a scripted local Responses API. Only the model is
fake. Commands run through the production provider, approval handling, and actual Windows sandbox. The tests require
a successful read or command as a positive control before accepting a denied write or request as evidence.

```powershell
$env:KRATOS_REAL_CODEX_EXE = '<native Codex 0.159.1 executable>'
$env:KRATOS_REAL_CODEX_HOME = '<dedicated test home>'
# Provision this isolated home once from an administrator terminal.
& $env:KRATOS_REAL_CODEX_EXE sandbox setup --elevated --current-user --codex-home $env:KRATOS_REAL_CODEX_HOME
cargo test -p kratos-engine --test real_codex --locked -- --ignored --test-threads=1
```

CI runs these tests in its own provisioned home. They cover read-only plan mode, ordinary Full access execution, and
an agent plus its child failing to read the API token or decide a waiting approval. The approval remains pending.

The restore check creates a version-4 database with the TypeScript store, starts the Rust engine to migrate it to 5,
restores the automatic backup, compares all original tables and rows, then starts the TypeScript engine and reads
the restored thread over HTTP:

```text
node --import tsx scripts/verify-r3-restore.mjs
```

## Performance evidence

The workload is 200 sequential turns spread over ten open threads, after ten warmup turns, against `fake-codex`.
Dispatch runs from the client submission to the fake's receipt of `turn/start`; completion runs from the fake's
completion injection to the client event. All timestamps use one observer's monotonic clock. These measurements
include localhost transport, so they are upper bounds on engine overhead, not isolated engine CPU time.

The reference runs the TypeScript provider directly because the TypeScript HTTP API never connected to Codex.
Its completion measurement ends at the provider callback, so it is not an HTTP/WebSocket equivalence benchmark.
The reference was measured before the Rust release result; the candidate limits of 5 ms p95 and 60 MiB engine
working set were retained.

Windows 11 build 26300, AMD Ryzen 7 5800X, Rust release build, Node 24.19.0:

| Measurement                        | TypeScript provider | Rust engine |
| ---------------------------------- | ------------------: | ----------: |
| Dispatch p50                       |            3.585 ms |    3.041 ms |
| Dispatch p95                       |            4.657 ms |    3.563 ms |
| Dispatch p99                       |            5.102 ms |    4.828 ms |
| Completion p50                     |            0.950 ms |    1.343 ms |
| Completion p95                     |            1.700 ms |    1.526 ms |
| Completion p99                     |            2.199 ms |    1.749 ms |
| Engine/process working set         |  176.8 MiB Node RSS |    12.6 MiB |
| Engine plus fake Codex working set |        Not measured |    19.0 MiB |

These are recorded measurements, not a cross-platform performance claim. Reproduce with:

```text
node --import tsx scripts/bench-r3-reference.mjs
powershell -NoProfile -File scripts/bench-r3.ps1
```

`bench-r3.ps1` starts an isolated release engine, invokes `kratos-client bench-turns`, measures the process tree,
and fails if either p95 or the engine memory budget is exceeded.

## Live-provider and review status

The signed-in GPT-6.1 Sol regression turn streamed its reply and completed. A real approval turn reached the client,
the acceptance reached Codex, and the recorded command ran exactly once with exit code 0 and the expected output.
The steering run passed after an earlier attempt hit the account limit. Stop interrupted its running turn within
the five-second assertion and started a new native turn; that next turn hit another account usage limit, so the
script's additional completed-reply check did not pass.
The remaining paid-model lanes must be rerun when allowance is available; passing scripted tests does not stand
in for those receipts. `scripts/verify-r3-live.mjs` provides the regression, approval, steering, stop, sandbox, and
declined-destructive-command probes. It retains isolated fixtures for inspection because Windows sandbox ACLs can
prevent ordinary recursive cleanup.

The independent GPT-6.1 Sol xhigh review identified a quota-stop crash window and lexical-only protected patch
paths before its review was interrupted by a safety filter. Both paths now have focused regressions. A completed
independent security verdict is still required before merge. This PR changes command execution, permissions, and
API authentication and remains security-sensitive. No PR was merged.
