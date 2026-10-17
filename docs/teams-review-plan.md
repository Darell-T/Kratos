# Teams review loop: implementation plan

Status: the plan for the PR that builds version 1 of [the Teams review spec](teams-review-spec.md). Read the spec
first. This plan says how the code meets it, where it deliberately differs, and how a reviewer can check the result.

## What the PR delivers

A Team gives one task to an implementer and a reviewer, each on Codex or Claude. Kratos runs the loop the spec
describes: handoff, review, evidence checks, fixes, re-review, approval or escalation. The user picks each agent's
provider and model and makes every final call. Both agents run sandboxed in their own worktree.

Like both providers today, Teams is an engine module with a typed API. Nothing calls it from `main.ts`, the server, or
the UI yet. That wiring waits for the UI design.

## Acceptance criteria

The reviewer checks the PR against these. Each one names where it is proven.

1. A task runs from start to approval with a Codex or Claude implementer and a Codex or Claude reviewer, through the
   public API, against a real git repository. Proven by loop scenario tests.
2. The reviewer never sees the implementer's conversation. Every message an agent gets is written by
   `teams/prompts.ts`.
3. Both reports are forced through each provider (Codex `outputSchema`, Claude `outputFormat`) and validated with
   zod. An invalid report goes back once, then the task pauses.
4. The handoff check runs in the spec's order: a valid report, `status: done`, a response for every open finding,
   and no file edited after the last command. The last rule holds across turns, so restating a report in a new turn
   does not clear an unchecked edit.
5. Kratos commits each round with the user's git identity and a message it writes itself, with no agent text, so no
   trailer can appear. Commits the implementer made on its own are folded into that one commit.
6. A finding reaches the implementer only when it is `important`, not a design opinion, confirmed by Kratos, and, after
   round 1, on a line the new diff changed. A bug is confirmed only by a command that exits non-zero in the reviewer's
   reset worktree. A quote is confirmed only by text found in the task, `AGENTS.md`, `CLAUDE.md`, or `REVIEW.md`.
7. Kratos reruns every open bug's command on each new commit, and the exit code decides whether it is fixed.
8. An approval covers one commit. The reviewer's verdict must agree with the confirmed findings, or it goes back once.
9. The task pauses for the user when a finding is still open after two fix attempts, when three rounds since the
   user's last decision end with blocking findings, on a dispute, on `blocked` or `needsUser`, after a second invalid
   report, and after a second lost turn of the same kind. Only that task pauses.
10. Each agent can write only inside its own worktree and its own cache folder, plus the Team's extra folders for the
    implementer. This holds for every tool, including Claude's file tools. Writes outside go to the user for the
    implementer and are refused for the reviewer. Windows is verified live. macOS and Linux are unit-tested at the
    option level, because Kratos does not run there yet.
11. Worktrees never leak. Kratos deletes both when a task closes, and at startup it deletes any that no open task owns.
12. An engine restart at any point continues each task from its record, and the record holds every round, report,
    check, and finding with how it ended.
13. The security pass runs once, on the approved commit, when the task's diff matches a trigger.
14. `createTeam` refuses a Team when the folder is not a git repository with a commit and an identity, a member's
    provider is not signed in, or the sandbox is not ready.
15. `npm run check` and `npm run build` pass, and the existing provider tests still pass.

## Design

Two candidate designs were sketched and compared against a rubric. This plan uses one as the base and adds the
strongest parts of the other. [Alternatives considered](#alternatives-considered) lists what was taken from each.

### Module map

| File                   | Owns                                                                                                |
| ---------------------- | --------------------------------------------------------------------------------------------------- |
| `teams/model.ts`       | Ids, Team settings, the report schemas, the `Fact` union, user decisions. Persisted shapes only.    |
| `teams/task.ts`        | Every process rule, as pure functions: `fold`, `plan`, `allowedDecisions`.                          |
| `teams/prompts.ts`     | Every message an agent receives. Pure.                                                              |
| `teams/evidence.ts`    | Location, quote, and command checks, and parsing changed line ranges.                               |
| `teams/security.ts`    | Parsing and matching security triggers. Pure.                                                       |
| `teams/git.ts`         | Branches, worktrees, commits, diffs, probe capture, cleanup, and the startup sweep.                 |
| `teams/agents.ts`      | The `Agent` port and its Codex and Claude adapters. The only Teams file that knows provider shapes. |
| `teams/teams.ts`       | The public API and the shell that runs one step at a time.                                          |
| `providers/sandbox.ts` | The generic thread sandbox setting and the command runner types. Used by both providers.            |
| `store.ts`             | Migration 5 and the Teams store methods.                                                            |

Dependencies point one way. `teams/` imports provider types. Providers never import `teams/`.

### The record: facts, a fold, and a plan

A task's record is an append-only list of facts in a new `team_facts` table, shaped like `thread_events`. A fact
says what happened, never what to do next. The pure `fold(facts)` derives a `TaskView`, and the pure `plan(view)`
names the one step that must happen next. The shell runs that step, appends the facts it produced, and repeats until
the step is `wait`.

```text
facts --fold--> TaskView --plan--> Step --shell runs it--> new facts --append--> facts
```

Restart needs no special code. The shell folds the same facts and gets the same step. A crash leaves either the old
facts or the new ones, because each append is one transaction.

The facts:

| Fact                   | Records                                                                                                                                            |
| ---------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| `task.started`         | The team, the task text and title, the branch, the base commit, the rules snapshot, the parsed triggers, and the Team policy frozen for this task. |
| `implementer.opened`   | The implementer's thread, and whether it is fresh.                                                                                                 |
| `plan.drafted`         | The plan text, when the Team plans first.                                                                                                          |
| `implementer.reported` | The parsed report, or the validation error and raw text, and the turn's change record.                                                             |
| `committed`            | The round's commit.                                                                                                                                |
| `reviewer.reported`    | The review flavor (`standard` or `security`), the thread, and the parsed report.                                                                   |
| `evidence.checked`     | Each finding's check, each open bug's rerun, the reviewer's probe files, and the security trigger hits.                                            |
| `agent.lost`           | A turn that produced nothing, and why.                                                                                                             |
| `step.failed`          | A Kratos step that threw, such as a git error.                                                                                                     |
| `user.decided`         | A user decision.                                                                                                                                   |
| `cleaned`              | Cleanup ran, and what a file lock left behind.                                                                                                     |

The phase is a discriminated union, so each phase carries exactly what its next step needs:

```ts
type Phase =
  | { kind: 'preparing' }
  | { kind: 'implementing'; since: number; brief: Brief; retries: Retry[]; note: string | null }
  | { kind: 'planning'; plan: string } // waiting for the user to approve the plan
  | { kind: 'committing'; then: 'review'; report: ImplementerReport }
  | { kind: 'committing'; then: 'accept' } // the user accepted the work as it is
  | { kind: 'reviewing'; since: number; flavor: Flavor; commit: Sha; retries: Retry[]; note: string | null }
  | { kind: 'checking'; flavor: Flavor; commit: Sha; report: ReviewReport; retries: Retry[] }
  | { kind: 'paused'; pause: Pause }
  | { kind: 'approved'; commit: Sha; by: 'reviewer' | 'user' } // one commit, always
  | { kind: 'closing'; outcome: Outcome }
  | { kind: 'closed'; outcome: Outcome };

type Retry = 'invalidReport' | 'lost' | 'uncheckedEdits' | 'verdictMismatch'; // one of each per phase
type Outcome = 'merged' | 'rejected' | 'cancelled';
```

`since` is the sequence number of the fact that entered the phase. It feeds the provider message id, so a step
repeated after a restart sends the same id and the provider does not run it twice.

A finding is in exactly one status:

```ts
type FindingStatus =
  | { kind: 'open'; attempts: number; response: Response | null; diagnosis: string | null }
  | { kind: 'closed'; how: 'fixed' | 'ruledNotBug' }
  | { kind: 'dropped'; reason: DropReason } // Kratos could not confirm it
  | { kind: 'listed'; why: 'nit' | 'preExisting' | 'design' | 'outsideDiff' }; // the user reads it, it never blocks
```

Kratos assigns finding ids (`F1`, `F2`, and so on) from the record, so "the same issue" is a fact Kratos can check.
Rounds, fix attempts, the round cap, and whether a file changed after the last command are all derived from the facts.
None is stored twice.

### Agents

Teams reaches providers only through two small interfaces in `teams/agents.ts`:

```ts
interface AgentHost {
  readonly provider: Provider;
  signedIn(): Promise<boolean>;
  sandboxProblem(): Promise<string | null>; // runs Codex's one-time Windows sandbox setup when needed
  open(spec: AgentSpec, resume?: string): Promise<Agent>;
}
interface Agent {
  readonly thread: ThreadRef;
  readonly fresh: boolean; // a new thread, so its first message must carry the task
  ask(request: { messageId: string; text: string; signal: AbortSignal }): Promise<Reply>;
  leavePlan(): void;
  release(): Promise<void>; // stops the turn and frees the provider's session for this thread
}
type Reply = { kind: 'reply'; output: unknown; changes: TurnChanges } | { kind: 'lost'; reason: string };
```

`ask` sends one message and resolves when its turn ends. It never throws for agent trouble. A failed turn, a timeout,
a paused queue, and a turn cut off by a restart all return `lost`. The adapters subscribe to provider events before
they send, and they match a Codex turn by the `turnId` that `sendMessage` returns, so a turn that finishes while it is
still starting is not missed. The Codex adapter reads the last `agentMessage` of the turn (or its `plan` item in plan
mode) from `item.completed` events. The Claude adapter reads `structured_output` from the result.

Thread lifetimes:

- **The implementer keeps one thread for the task**, because it owns the work and its context. After a lost turn or
  a restart, `open(spec, resume)` resumes the thread and clears the provider's paused queue (`discardMessage` for an
  uncertain message, then `resumeQueue`). If that fails, it opens a fresh thread, and the next message tells the
  agent to inspect its worktree because an earlier attempt was cut off.
- **The reviewer gets a fresh thread for each review round.** It keeps that thread for corrections within the round,
  and Kratos releases it when the round ends. A lost reviewer turn gets a fresh thread. This gives the reviewer fresh
  context every round and removes reviewer queue repair entirely. The review package lists every finding already
  reported, so a fresh reviewer does not report one again.

### Provider changes

Every change is generic. No provider learns what a Team, a role, a round, or a finding is.

Both providers:

- New thread settings `sandbox: ThreadSandbox | null` and `outputSchema: JsonSchema | null`, accepted by
  `startThread`, saved by migration 5, and restored by `resumeThread`. `null` keeps today's behavior.
- `outputSchema` applies to every turn that is not in plan mode. A plan turn ends with a plan.
- `TurnChanges` gains `ranCommand`, true when the turn ran at least one command. Teams uses it to carry "a file changed
  after the last command" across turns.
- A new `releaseThread(threadId)` stops the thread's turn and forgets the thread. Claude closes its Claude Code
  process, which on Windows would otherwise keep the worktree folder locked. Messages stay in the store.

```ts
// providers/sandbox.ts
interface ThreadSandbox {
  extraWritableRoots: string[]; // writable besides the thread's own folder
  outsideWrites: 'ask' | 'deny'; // the implementer asks, the reviewer is refused
  env: Record<string, string>; // for example npm_config_cache inside a writable root
}
type RunCommand = (request: CommandRequest) => Promise<CommandResult>;
type CommandResult =
  | { kind: 'exited'; exitCode: number; stdout: string; stderr: string }
  | { kind: 'timedOut'; output: string }
  | { kind: 'refused'; error: string };
```

Codex:

- A sandboxed thread runs every turn with `workspaceWrite` over its folder and the extra roots, `networkAccess:
true`, and the temp folder writable. In plan mode it keeps today's read-only policy. `approvalPolicy` is `never` for `outsideWrites: 'deny'` and `on-request`
  otherwise, and `approvalsReviewer` is always `user`. It never gets `dangerFullAccess`, whatever the permission.
- `codex-approvals.ts` treats a sandboxed thread as `askMe`, so an escalation out of the sandbox waits for the user
  instead of being accepted.
- `thread/start` and `thread/resume` pass `config: { shell_environment_policy: { set: env } }`.
- `runCommand(request)` runs one command line through `command/exec` (PowerShell on Windows, `sh` elsewhere) with the
  same sandbox policy, or `dangerFullAccess` when `outsideSandbox` is set. A request error that says "command timed
  out" is `timedOut`, and any other sandbox error is `refused`.
- `ensureSandbox()` calls `windowsSandbox/readiness` on Windows. When the answer is not `ready`, it runs
  `windowsSandbox/setupStart` in `unelevated` mode and waits for `windowsSandbox/setupCompleted`.

Claude:

- A sandboxed thread never runs in `bypassPermissions` or `auto`, because those modes let Claude Code skip
  `canUseTool`, and `canUseTool` is the only guard on Claude's file tools. It runs in `default`, or in `plan` while
  planning.
- It loads only the user's settings (`settingSources: ['user']`), for both roles. Project settings, hooks, and MCP
  servers would otherwise load from the worktree, which the agent itself can write, and a hook runs outside every
  sandbox. The project's `AGENTS.md` and `CLAUDE.md` reach the agent through Kratos's first message instead.
- On Windows, `disallowedTools` removes `Bash`, `PowerShell`, and `Monitor`. An in-process MCP server named `kratos`
  offers `run_command`, which calls the `runCommand` that `createClaudeProvider({ runCommand })` receives from Codex.
  The implementer's tool takes an `outsideSandbox` flag. The reviewer's tool does not have one.
- On macOS and Linux, the thread uses Claude Code's own sandbox: `sandbox: { enabled: true, failIfUnavailable: true,
filesystem: { allowWrite: roots }, allowUnsandboxedCommands: outsideWrites === 'ask' }`, and the SDK `env` option.
  This branch is unit-tested at the option level only, because Kratos runs only on Windows today.
- `canUseTool` checks a sandboxed thread against one pure rule table before today's rules:

  | Tool call                                    | Inside the roots                                                           | Outside the roots                        |
  | -------------------------------------------- | -------------------------------------------------------------------------- | ---------------------------------------- |
  | `Write`, `Edit`, `MultiEdit`, `NotebookEdit` | `askMe` asks, others allow                                                 | `ask` or deny, by `outsideWrites`        |
  | `run_command` from the `kratos` SDK server   | `askMe` asks, others allow                                                 | `outsideSandbox: true` asks or is denied |
  | Any other MCP tool                           | Asks or is denied, by `outsideWrites`, because it runs outside the sandbox | Same                                     |
  | `AskUserQuestion`                            | Always the question path, as today                                         | Same                                     |
  | Anything else                                | `askMe` asks, others allow                                                 | Same                                     |

  The `run_command` rule checks `options.mcpServer.source === 'sdk'`, so a different server named `kratos` cannot
  claim it. Paths resolve through `realpath` of their deepest existing folder, so a link inside a root cannot point a
  write outside it.

- `mcp__kratos__run_command` counts as a command for `TurnChanges`.
- `outputSchema` becomes `outputFormat: { type: 'json_schema', schema }`.

### Rules

All of these are pure functions in `teams/task.ts`, tested without fakes.

- **Handoff.** In order: the report parses, its `status` is `done` (`blocked` and `needsUser` pause), its responses
  cover every open finding exactly once with a reason for each dispute, and no file changed after the last command.
  The last rule folds every implementer turn since the last commit:
  `dirty = changes.ranCommand ? changes.editedSinceCommand : dirty || changes.editedSinceCommand`. A lost turn counts as
  dirty, because its change record is gone. Each failing rule goes back once with a note, then pauses.
- **Disputes.** Kratos commits, then pauses, and the user rules on each disputed finding. The reviewer never judges a
  dispute.
- **Triage.** A new finding is `listed` when it is a nit, pre-existing, or a design opinion, and Kratos never checks or
  sends it. Otherwise its evidence is checked. Unconfirmed means `dropped` with the reason. Confirmed but outside the
  new diff's changed lines, after round 1, means `listed` as missed earlier. Anything else is `open` and blocks.
- **Re-review.** An open bug's rerun decides it: a passing command closes it, a failing one keeps it open, and a
  rerun that cannot run falls back to the reviewer's `stillOpen`. Other open findings follow `stillOpen`.
- **Verdict.** `approve` with a confirmed blocking finding, or `changesRequested` with none, goes back once on the same
  reviewer thread, then pauses. A report Kratos rejects keeps its record, but its findings are not added.
- **Escalation.** A blocking finding with two fix attempts pauses as stuck, with the reviewer's diagnosis. Three rounds
  since the user's last decision ending with blocking findings pause at the cap. A user decision starts a new count
  of rounds and of each open finding's attempts.
- **Decisions.** `approvePlan`, `direct`, `stepBack`, `rule`, `resume`, `accept`, `cancel`, and `finish` (merged or
  rejected). `allowedDecisions(view)` lists what the current phase accepts, and `decide` refuses anything else before
  it records the fact. `accept` commits the worktree first, so accepted work is never lost.

### Git and worktrees

- **Layout.** `<home>/worktrees/<task id>/implementer` and `/reviewer` for the worktrees, and
  `/cache/implementer` and `/cache/reviewer` for each agent's package cache. A cache inside a worktree would be
  committed, and a shared one would let one agent poison the other's. Task ids are 8 hex characters, because the
  Windows path limit is near with `node_modules` inside.
- **Branch.** `<implementer provider>/<slug of the task>`, made unique with `-2`, `-3`. Kratos reserves it with
  `git branch` at the base commit, which fails if the ref exists, so two tasks cannot take one name.
- **Kratos's git commands** run with `-c core.hooksPath=<empty folder>`, so no hook in a tree an agent wrote can run.
  Commands on one repository run one at a time, because all worktrees share its `.git`.
- **Commit.** `git reset --soft <last round's commit, or the base>`, then `git add -A`, then a commit with the
  repository's identity, `--no-verify`, and the subject `<task title> (round n)`. This folds any commits the
  implementer made on its own, which Codex's sandbox allows on the worktree's own branch. A clean tree returns the
  current commit, so a repeated step is safe.
- **Reviewer worktree.** Added detached at the first review, then moved with a forced detached checkout. Before
  evidence runs, `git reset --hard <commit>` restores tracked files, so a reviewer edit cannot fake a bug. Untracked
  probe files stay, and Kratos copies them into the record before anything can delete them, capped at 64 KB per file
  and 512 KB per review.
- **Cleanup.** On `finish` or `cancel`, Kratos releases both threads, then removes both worktrees and the cache folder,
  falling back to deleting the folder and running `worktree prune`. Cleanup never fails a step, and the branch stays.
- **Startup.** Kratos deletes every folder under `<home>/worktrees` that no open task owns, then prunes each Team
  repository. Paused and approved tasks keep their worktrees.

### Evidence

- **Location.** The file must exist at the reviewed commit, with at least that many lines, and the path must be
  relative with no `..`.
- **Bug.** The command runs through Codex's `runCommand` in the reviewer's reset worktree, writable only there and in
  the reviewer's cache folder, with the reviewer's cache environment and a 120 second limit. Only `exited` with a non-zero code confirms. A pass, a timeout, or a
  refusal drops the finding with that reason.
- **Requirement or constraint.** The quote, with whitespace collapsed, must be at least 12 characters and appear in
  the task text or the rules snapshot.
- **Changed lines.** `git diff -U0 <last reviewed>..<commit>` hunk headers give each file's changed line ranges.

### Reports

The zod schemas in `teams/model.ts` follow the spec's three tables. Every field is required, and "no value" is
`null`. Rules that strict JSON Schema cannot express, such as "not blank", are zod refinements. `z.toJSONSchema(...)`
derives each provider schema, and a test asserts that both stay in the strict subset (every property required,
`additionalProperties: false`, no `anyOf`, `minLength`, or `pattern`). An invalid report is stored with its error and
raw text, goes back once with the error, and pauses on the second failure.

### Rules snapshot

At task start, Kratos copies `AGENTS.md`, `CLAUDE.md`, and `REVIEW.md` from the Team's folder, matching names without
regard to case, into `task.started`. The review package, the quote check, and the security triggers all read that
snapshot. The implementer cannot weaken the standard it is judged by. A rules file the user has not committed yet,
such as this repository's `Review.MD`, still counts.

### Security pass

`REVIEW.md` declares triggers in a fenced block whose info string is `kratos-security`:

````md
```kratos-security
path: packages/engine/src/providers/**
code: child_process|spawn\(
```
````

A `path` line is a glob matched against the files the task changed. A `code` line is a regular expression matched
against the lines the task added. Without the block, Kratos uses built-in defaults for process spawning, network
calls, credentials, permission and sandbox code, and `.env` files. An empty block turns the pass off for the
project. A bad line refuses the task at start and names the line.

After the first reviewer approval, if the Team runs the pass and a trigger matches the whole task diff, Kratos asks a
fresh reviewer thread to review the approved commit with Kratos's security instructions and the matched triggers. The
report format and the evidence checks are the same. A blocking security finding sends the task back to the
implementer, and its round counts toward the cap. The pass runs once per task.

### Team setup

`createTeam(settings)` runs every check and reports every problem at once:

- The folder is the root of a git repository with at least one commit, and `user.name` and `user.email` resolve.
- Each extra writable folder is absolute, exists, and lies outside the repository and Kratos's home.
- Each member's provider reports that it is signed in.
- The sandbox is ready. Codex's sandbox serves every Team, through Codex agents, Claude agents on Windows, and
  Kratos's evidence commands, so `ensureSandbox()` must succeed.

Both providers can force a report format, so there is no runtime check for it. A provider that cannot would not fit the
`AgentHost` interface.

### Public API

```ts
function agentHosts(codex: CodexProvider, claude: ClaudeProvider): Record<Provider, AgentHost>;
function createTeams(options: {
  store: Store;
  home: string;
  hosts: Record<Provider, AgentHost>;
  runCommand: RunCommand; // Codex's, for evidence
}): Promise<Teams>; // sweeps worktrees and resumes every open task before it returns

interface Teams {
  createTeam(settings: TeamSettingsInput): Promise<Team>; // throws TeamSetupError with every problem
  teams(): Team[];
  startTask(teamId: TeamId, text: string): Promise<TaskView>;
  decide(taskId: TaskId, decision: Decision): TaskView; // throws TeamsRefusal when not allowed now
  task(taskId: TaskId): TaskView | undefined;
  tasks(teamId: TeamId): TaskView[];
  onEvent(listener: (event: { kind: 'task.changed'; view: TaskView }) => void): () => void;
  close(): Promise<void>;
}
```

`TaskView` carries `allowed`, the decisions the task accepts now. Agents' live output, approvals, and questions stay
on the providers' own events, keyed by the thread ids in the view, so Teams does not wrap them again.

## Decisions that differ from the spec or settle its open details

Each one can be reversed in a later PR.

| Decision                                                                                             | Why                                                                                                                                                                                                                                               |
| ---------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Team Claude agents load only user settings, the implementer included.                                | The spec left implementer hooks open. A hook the implementer writes into its own worktree would run outside every sandbox at its next launch.                                                                                                     |
| Inside a Team, `approveForMe` behaves like `fullAccess`: no prompts inside the worktree.             | The spec sends every write outside the worktree to the user, which leaves `approveForMe` nothing extra to approve.                                                                                                                                |
| The setting that lets the implementer keep the reviewer's probe tests is not built.                  | The default, "the user decides", is the only behavior v1 needs. The approval view lists every probe with the output that showed the bug.                                                                                                          |
| The security pass always uses Kratos's instructions, never Claude Code's `/security-review`.         | The spec says the pass can use it, not must. One path works on both providers, and the command picks its own diff base.                                                                                                                           |
| The Linux `bubblewrap` and `socat` setup check is not built.                                         | Claude Code's `failIfUnavailable` already refuses to start without them, which pauses the task with Claude Code's own message. Kratos does not run on Linux yet.                                                                                  |
| Each agent gets its own cache folder, writable only by it, with `npm_config_cache` pointing into it. | Measured: `npm install` fails inside Codex's sandbox because npm's default cache is outside the writable roots. Separate folders mean no agent can poison another's cache. Other package managers' cache variables can join the same `env` later. |
| The rules snapshot comes from the Team folder at task start, not from the base commit.               | This repository's `Review.MD` is not committed, so a read at the base commit would find no rules.                                                                                                                                                 |
| Kratos folds commits the implementer made on its own into its round commit.                          | Measured: Codex's sandbox lets a worktree agent commit on its own branch, so "no trailers" needs code, not a prompt.                                                                                                                              |

## Measured facts

These were run on this Windows machine with Codex 0.159.1 and SDK 0.3.296, with no paid model calls unless noted.

- Codex `turn/start` with a strict `outputSchema` ends the turn with one `agentMessage` whose text is the matching JSON.
  Claude `outputFormat` returns the parsed object as `structured_output` (real Haiku run).
- Codex `command/exec` with `workspaceWrite` let a command write inside its root and refused a write in the home
  folder. A command over its time limit rejects with "exec failed: sandbox error: command timed out".
- A sandboxed command in a worktree could commit on that worktree's branch. It could not move `main` ("cannot lock ref
  'refs/heads/main': Permission denied"), write a hook into the repository's `.git/hooks`, or write a file in the main
  checkout.
- `npm install` failed inside the sandbox writing `C:\Users\...\AppData\Local\npm-cache`, and succeeded with
  `npm_config_cache` set inside the root.
- `thread/start` with `config: { shell_environment_policy: { set: { NAME: value } } }` sets the variable for the
  thread's shell.
- Claude Code's own sandbox refuses to start on native Windows ("feature gate off"). With `disallowedTools` and an
  in-process `kratos` MCP server, the model is offered `mcp__kratos__run_command` and not `Bash` or `PowerShell`, and a
  forced `Bash` call fails "in subagents as well as here".
- The `codex sandbox` CLI needs a named permission profile from the user's Codex config, so it cannot wrap Claude's
  own Bash tool.

## Build order

Each unit ends with `npm run check` green, in its own commit, so a reviewer can follow the story.

1. **Provider settings.** `providers/sandbox.ts`, the `sandbox` and `outputSchema` thread settings, the migration 5
   columns, and `TurnChanges.ranCommand`, with tests in the existing provider test files.
2. **Codex.** The turn policy, approvals, the env config, `runCommand`, `ensureSandbox`, and `releaseThread`. The fake
   Codex gains `command/exec`, `windowsSandbox/readiness`, and `windowsSandbox/setupStart`.
3. **Claude.** The sandboxed session options for each platform, `run_command`, the `canUseTool` table,
   `outputFormat`, and `releaseThread`. The SDK fixture gains `createSdkMcpServer` and `tool`.
4. **The pure core.** `teams/model.ts`, `teams/task.ts`, `teams/prompts.ts`, `teams/security.ts`, and the pure half of
   `teams/evidence.ts`, with scenario tests on `fold` and `plan`.
5. **Git.** `teams/git.ts`, with tests on temporary repositories.
6. **Agents.** `teams/agents.ts`, with tests against the existing Codex and Claude fixtures.
7. **The shell.** `teams/teams.ts` and the store methods, with loop scenario tests.
8. **Live checks**, run by hand at the end. See [Test plan](#test-plan).

## Test plan

The tests are scenarios, each tied to a rule and the defect it catches. Each new rule is broken on purpose once to show
a test fails.

| Test file              | Proves                                                                                                                                                                                                                                                                                              | Uses                                                                                           |
| ---------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| `teams-task.test.ts`   | Every rule in [Rules](#rules): triage, evidence beating the reviewer's opinion, the handoff order, dirtiness across turns, disputes, both escalations, the cap reset, verdict agreement, the security pass running once, restart from any prefix of the facts, and the decisions each phase allows. | Nothing. Pure.                                                                                 |
| `teams-loop.test.ts`   | A task end to end: the commit's author and message, a nit never reaching the implementer, an agent's own commit folded away, accepted work committed before cleanup, cancel mid-turn, a restart mid-turn, and the startup sweep keeping paused worktrees.                                           | A real `Store`, real git in a temp repository, scripted `AgentHost`s, and a fake `RunCommand`. |
| `teams-git.test.ts`    | Worktree creation and reuse, the reserved branch, an idempotent commit, a move that keeps probes, the reset before evidence, removal under a held file, and the sweep.                                                                                                                              | Real git.                                                                                      |
| `teams-agents.test.ts` | Reading each provider's report, `lost` for each failure, a turn that finishes while still starting, resume with queue repair, and release.                                                                                                                                                          | The existing Codex and Claude fixtures.                                                        |
| Provider test files    | The sandboxed Codex `turn/start` parameters, an escalation on a sandboxed `fullAccess` thread waiting for the user, the env config, Claude's options for `win32`, `darwin`, and `linux`, the `canUseTool` table, `run_command` reaching `runCommand`, and the report format on non-plan turns.      | The existing fixtures.                                                                         |

Live checks before the PR is ready, on this machine, with cheap models:

1. A Codex agent's own command runs `npm install` in a sandboxed Team thread, using the env config.
2. A full task on a toy repository: a Codex implementer and a Claude Haiku reviewer, one planted bug, a fix round, and
   approval.
3. A Claude reviewer on Windows tries to write outside its worktree with `run_command`, and is refused.

## Open questions

1. Implementer turns can run longer than the providers' 10-minute default deadline, and a timeout counts as a lost
   turn. The engine wiring PR should choose `turnTimeoutMs` for Teams.
2. Which hosts should Claude Code's sandbox reach on macOS and Linux for installs? It cannot be tested here.
3. Does Codex load project hooks from a worktree? If it does, a Codex implementer has the hook escape this plan closes
   for Claude.

## Alternatives considered

| Shape                                                                            | Kept or rejected                                                                                                                                                                                                                                                                    |
| -------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| One mutable JSON row per task, rewritten on every transition                     | Rejected for the fact log, which gives a full history and needs no recovery code. Kept from it: per-cause retries, dirtiness across turns, the commit subject from the task title, hooks off for Kratos's git, the per-repository git queue, accept committing first, and `finish`. |
| A provider adapter that repairs every queue, with one long-lived reviewer thread | Rejected. Reviewers get a fresh thread per round, so only the implementer's queue ever needs repair, and a failed repair falls back to a fresh thread.                                                                                                                              |
| A field on the Codex `turn.completed` event for the final message                | Rejected. The adapter reads it from `item.completed`, which leaves the provider's event shape alone.                                                                                                                                                                                |
| Checking "inside the new diff" by changed file                                   | Rejected for changed line ranges. Most re-flagged findings in later rounds are old code in files the fix touched.                                                                                                                                                                   |
| Reading the rules at the base commit                                             | Rejected, because uncommitted rules files would be missed.                                                                                                                                                                                                                          |
| Wrapping Claude's Bash with the `codex sandbox` CLI                              | Rejected, because it needs a permission profile in the user's Codex config.                                                                                                                                                                                                         |
