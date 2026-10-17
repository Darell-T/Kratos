# Teams review spec

Status: draft for review. Nothing here is built yet.

A Team gives one task to an implementer and a reviewer. Kratos passes the work between them until the reviewer
approves it or a person has to decide something. The user picks the provider and model for each role, and makes the
final call on every task.

`REVIEW.md` sets the review standards. Kratos enforces the process in code: who can write where, what each agent
sees, when a round starts, when it stops, and what counts as an approval. An instruction in a prompt can be ignored,
so no rule in this spec depends on one.

## Version 1 scope

Version 1 includes:

- A Team with one implementer and one reviewer, each on any Kratos provider and model.
- The review loop: handoff, review, findings back to the implementer, re-review, approval or escalation.
- A separate worktree for each role, and a sandbox that keeps each agent inside its own.
- Fixed report formats for both agents, checked by Kratos.
- Evidence that Kratos checks before a finding reaches the implementer.
- A saved record of every round.
- An optional planning step before implementation.
- A security pass, on by default, built after the main loop works.

Version 1 leaves out the orchestrator, more than one agent per role, a second agent that double-checks findings, a
spending limit, automatic merging, and the UI design. [Later](#later) lists them.

## The loop

A task moves through these states. Kratos owns every transition.

```text
planning (optional) -> implementing -> handoff check -> reviewing -> approved -> user decides
                            ^                              |
                            +------ changes requested -----+
any state -> paused for the user -> back to the state it left, or cancelled
```

1. The user starts a task on a Team. The task text is the goal the reviewer checks the work against.
2. If the Team plans first, the implementer starts in plan mode. The user approves the plan by turning plan mode off.
3. The implementer works in its worktree and ends its turn with an [implementer report](#implementer-report).
4. Kratos runs the [handoff check](#handoff-check). If it passes, Kratos commits the work and starts a review round.
5. The reviewer reviews that commit in its own worktree and ends with a [review report](#review-report).
6. Kratos checks the evidence for each finding and drops the findings it cannot confirm.
7. If blocking findings remain, Kratos sends them to the implementer, and the loop returns to step 3.
8. If none remain and the reviewer approves, the task waits for the user.

Agents never send messages to each other. Kratos writes every message an agent receives, so Kratos alone decides what
each agent sees.

## What each agent sees

The reviewer gets a review package and nothing else:

- The task text.
- The commit under review and the diff to review.
- `AGENTS.md` and `REVIEW.md` from the project.
- The open findings from earlier rounds, with the implementer's response to each.

The reviewer never sees the implementer's conversation. Fresh context keeps the reviewer from inheriting the
implementer's reasons, and it costs fewer tokens.

The implementer gets the confirmed blocking findings, one message per round. It does not get nits, findings Kratos
dropped, or the reviewer's conversation.

## Reports

Both agents end each turn with a report in a fixed format. Kratos forces the format through each provider: Claude's
`outputFormat` with `type: 'json_schema'`, and Codex's `outputSchema` on `turn/start`. Kratos validates every report.
A report that fails validation goes back to the agent once with the validation error. A second failure pauses the task.

### Implementer report

| Field       | Meaning                                                                                   |
| ----------- | ----------------------------------------------------------------------------------------- |
| `status`    | `done`, `blocked`, or `needsUser`.                                                        |
| `summary`   | What changed and why, in a few sentences.                                                 |
| `checks`    | The commands the implementer ran to verify the work, each with its exit code.             |
| `responses` | One entry per open finding: `fixed`, or `disputed` with a reason. Required after round 1. |

### Review report

| Field       | Meaning                                                                                |
| ----------- | -------------------------------------------------------------------------------------- |
| `verdict`   | `approve`, `changesRequested`, or `blocked`. `blocked` means only the user can decide. |
| `summary`   | Starts with the finding count, and says so plainly when there is nothing to fix.       |
| `findings`  | New findings, in the [finding](#finding) format.                                       |
| `stillOpen` | Earlier findings that the new commit does not fix, each with a `diagnosis`.            |

A `diagnosis` explains what is still wrong, the evidence, and why the earlier fix did not work. Kratos shows it to the
user if the finding escalates.

### Finding

| Field       | Meaning                                                                                       |
| ----------- | --------------------------------------------------------------------------------------------- |
| `severity`  | `important`, `nit`, or `preExisting`. `preExisting` means the bug was there before this task. |
| `kind`      | `bug`, `requirement`, `constraint`, or `design`.                                              |
| `class`     | A short name for the kind of mistake, so the implementer fixes every instance.                |
| `location`  | File and line in the reviewed commit.                                                         |
| `problem`   | What goes wrong, and for whom.                                                                |
| `evidence`  | For a `bug`, a command that fails on the reviewed commit. Otherwise, the quoted rule.         |
| `fixedWhen` | The condition that shows the finding is fixed.                                                |

Kratos gives each finding an ID and keeps it across rounds, so "the same issue" is a fact Kratos can check.

## Rules Kratos enforces

### Handoff check

Before a review round starts, Kratos checks the implementer's handoff:

- The report is valid, and `status` is `done`. `blocked` and `needsUser` pause the task.
- After round 1, every open finding has a response.
- The turn change record shows no file edited after the last command (`editedSinceCommand` is `false`). If it is
  `true`, Kratos returns the work once with "run your checks after your last edit". If the next handoff still has an
  unchecked edit, the task pauses.

When the check passes, Kratos commits the worktree. The commit uses the user's git identity and has no trailers.

### Evidence check

Kratos checks each finding before the implementer sees it:

- The `location` must exist in the reviewed commit.
- For a `bug`, Kratos runs the `evidence` command in the reviewer's worktree with a time limit. The command must exit
  with a non-zero code. A command that passes or times out does not prove the bug, and Kratos drops the finding.
- For a `requirement` or `constraint`, the quoted text must appear in the task, `AGENTS.md`, or `REVIEW.md`.

Dropped findings stay in the task record with the reason, and the user can read them.

### What starts another round

A finding blocks only when all of these hold:

- `severity` is `important`.
- `kind` is `bug`, `requirement`, or `constraint`. Design opinions never block.
- Kratos confirmed its evidence.
- In round 2 or later, its `location` is inside the new diff. A finding outside the new diff was missed earlier.
  Kratos lists it for the user and does not start a round for it.

Nits and pre-existing bugs go in the summary the user reads. They never go to the implementer automatically. Every
nit an agent fixes is another commit and another round.

### Re-review

Round 1 reviews the whole diff from the task's base commit. Each later round does two things:

- Checks each open finding against its `fixedWhen`.
- Reviews only the diff since the last reviewed commit.

The reviewer reruns its earlier `evidence` commands. A finding whose command now passes is fixed.

### Approval

- An approval covers one commit. Any later commit cancels it.
- Kratos rejects an `approve` verdict that leaves a confirmed blocking finding open, and asks the reviewer to correct
  the report once.
- An approved task waits for the user. Kratos never merges.

### Escalation

The task pauses and Kratos asks the user when:

- A finding is still open after two fix attempts.
- Round 3 ends with blocking findings. The cap is three rounds.
- The implementer disputes a finding. The user decides, not the reviewer.
- The implementer reports `blocked` or `needsUser`, or the reviewer reports `blocked`.
- A report fails validation twice.
- An agent's process fails again after one retry.

The escalation shows the open findings with each `diagnosis` and the evidence. The user can:

- Send the implementer a step-back prompt that asks it to reconsider the approach.
- Give the implementer direction.
- Accept the work as it is.
- Cancel the task.

Only that task pauses. Other tasks, Teams, and threads keep running.

## Worktrees and disk space

Git worktrees share the repository's history. A worktree costs disk space only for the files it checks out and for
what installs and builds add, such as `node_modules`. Kratos owns their whole lifecycle, so none are left behind:

- Each task has at most two worktrees: one for the implementer and one for the reviewer.
- Kratos creates the implementer's worktree on a new branch when the task starts. The branch name starts with the
  implementer's provider, such as `claude/add-login` or `codex/add-login`, so the owner of the work is clear.
- Kratos creates the reviewer's worktree at the first review. Later rounds move it to the new commit instead of making
  another one.
- Before moving or deleting the reviewer's worktree, Kratos copies the reviewer's evidence into the task record.
- Kratos deletes both worktrees when the user merges, rejects, or cancels the task. The branch stays until the user
  deletes it.
- When the engine starts, Kratos deletes worktrees that belong to finished tasks. A crash during cleanup cannot leave
  them behind. Paused tasks keep theirs.
- The reviewer installs the project's dependencies in its worktree to run tests. That costs a few hundred MB and a
  minute or two per task, which is accepted.

## Sandbox

Both agents run sandboxed. Every command an agent runs can write only inside that agent's own worktree. The worktree
alone is only a separate folder, so the sandbox is what keeps an agent away from the user's checkout, other tasks'
worktrees, and the rest of the machine.

Which sandbox Kratos uses depends on the agent's provider and the operating system:

| Agent's provider | Windows                                | macOS                 | Linux                 |
| ---------------- | -------------------------------------- | --------------------- | --------------------- |
| Codex            | Codex's sandbox                        | Codex's sandbox       | Codex's sandbox       |
| Claude           | Codex's sandbox, through `run_command` | Claude Code's sandbox | Claude Code's sandbox |

Codex's sandbox is needed for a Claude agent on Windows only. Claude Code's own sandbox runs on macOS and Linux, but
on native Windows Claude Code runs commands unsandboxed. In Kratos's test, turning it on there failed with "the
Windows sandbox is not active on this session (feature gate off)".

### What each role may do

- The implementer writes freely inside its worktree. The Team's permission decides only whether it asks before
  acting there. `fullAccess` means no approval prompts inside the worktree, not access to the whole machine.
- When the implementer needs to write outside its worktree, such as installing a global tool, it asks the user. The
  user approves that one action, or adds the folder to the Team's extra writable folders.
- The reviewer can read anything and write only inside its worktree. It never asks to write elsewhere, and it never
  has write access to the implementer's worktree.

### Codex agent, every operating system

The thread runs with the `workspaceWrite` sandbox policy, with the agent's worktree and the Team's extra writable
folders as the writable roots. Kratos sets `networkAccess: true` so dependency installs work. Kratos's normal Codex
threads keep the network off. The implementer's approval policy is `on-request`, so Codex asks the user before it
leaves the sandbox. `fullAccess` does not switch an implementer to `dangerFullAccess` inside a Team.

### Claude agent on macOS and Linux

Kratos launches the agent with Claude Code's sandbox through the SDK's `sandbox` option:

- `enabled: true` turns the sandbox on.
- `failIfUnavailable: true` makes the session fail instead of running commands unsandboxed.
- `filesystem.allowWrite` lists the agent's worktree and the Team's extra writable folders. Claude Code also allows
  its per-user temp directory.
- For the reviewer, `allowUnsandboxedCommands: false` removes the retry that runs a blocked command outside the
  sandbox. For the implementer, the retry stays, and Kratos's `canUseTool` sends it to the user for approval.

On Linux, Claude Code's sandbox needs the `bubblewrap` and `socat` packages. If they are missing, the Team setup
check fails and tells the user to install them.

### Claude agent on Windows

- `disallowedTools` removes Claude Code's `Bash` and `PowerShell` tools.
- Kratos gives the agent a `run_command` tool through `createSdkMcpServer`. The tool runs each command through
  Codex's `command/exec` with the same `workspaceWrite` policy as a Codex agent.
- For the implementer, `run_command` takes an `outsideSandbox` flag. Kratos asks the user before it runs such a
  command with `dangerFullAccess`. The reviewer's `run_command` has no such flag.
- Codex's Windows sandbox needs a one-time setup. When the user creates the Team, Kratos calls
  `windowsSandbox/readiness`. If the answer is not `ready`, Kratos runs `windowsSandbox/setupStart`.

This path needs the Codex CLI, which the README already requires. When Claude Code turns its Windows sandbox on, a
Claude agent on Windows switches to it, and this path goes away. The SDK upgrade note in `REVIEW.md` covers the check.

### Rules for every Claude agent

- Claude's file tools run outside every sandbox. Kratos's `canUseTool` checks `Write`, `Edit`, and the other file
  tools. A path inside the worktree or an extra writable folder follows the Team's permission. Any other path goes to
  the user for the implementer, and is denied for the reviewer.
- A Claude agent in a Team never runs in `bypassPermissions`, because that mode skips `canUseTool`. `fullAccess`
  is carried out by Kratos's `canUseTool`, which allows actions inside the worktree without asking.
- Hooks and MCP servers from settings files also run outside the sandbox. The reviewer loads only the user's settings
  (`settingSources: ['user']`), so hooks in the reviewed code cannot run. The review package already carries
  `AGENTS.md` and `REVIEW.md`.

### Open details

- Network access on macOS and Linux. Claude Code's sandbox sends traffic through a proxy with an empty domain list by
  default. Implementation must decide which hosts an agent can reach for installs.
- Hooks for the implementer. The implementer loads the project's settings, so the project's own hooks run outside the
  sandbox. Decide whether that is acceptable, since the user owns the project.
- Kratos runs only on Windows today. Test the macOS and Linux paths when Kratos supports those platforms.
- Kratos's normal threads outside Teams do not use this sandbox yet. See idea 32 in `IDEAS.md`.

## Team setup

When the user creates a Team, they choose:

- The project folder.
- The implementer's provider, model, and permission.
- The reviewer's provider and model. The reviewer can use the same provider and model as the implementer.
- Whether to plan first. The default is off.
- Whether to run the security pass. The default is on.
- Who keeps tests. The default is the user.
- Extra folders the implementer can write to outside its worktree. The default is none.

Kratos refuses to create the Team when a member's provider is not signed in or cannot force a report format, or
when the sandbox is not available for either agent. A Team without a working review path would claim reviews that
never happened.

## Security pass

The security pass runs once, on the approved commit, and only when the change touches a security trigger. `REVIEW.md`
lists the triggers as path patterns and code patterns, such as process spawning, network calls, credentials, and
permission checks. Kratos matches them against the diff without a model call.

On Claude, the pass can use Claude Code's `/security-review` command. On other providers, it uses the Kratos security
instructions. Its findings use the same format and checks. A blocking security finding sends the task back to the
implementer, and the round counts toward the cap.

## Tests from the review

By default, the user decides which of the reviewer's probe tests to keep. The approval report lists each probe with
the output that showed the bug.

A Team can let the implementer keep probes. The implementer then adds a probe to the project as part of the fix for
its finding. Kratos keeps the probe only if it fails on the commit before the fix and passes on the fix commit. A probe
that passes on both commits proves nothing.

## Task record

Kratos stores, for each task:

- The task text, the Team, and the base commit.
- Each round's commit, both reports, and the result of each check.
- Each finding's history and how it ended: fixed, disputed, dropped by Kratos, or ruled "not a bug" by the user.
- The reviewer's evidence files.

The user reads the record to decide on the task. Later, the record shows how often findings were wrong, which decides
whether Kratos needs a second agent to double-check findings.

## `REVIEW.md` after this

Once Kratos enforces the process, the process sections of `REVIEW.md` can go: reviewer ownership and permissions,
finding handoff, re-review, and output format (sections 13 to 17). What remains is the standard the reviewer applies:
the goal check, `AGENTS.md` compliance, adversarial correctness, blast radius, YAGNI, Testing on the Toilet, and the
pstack principles. Anthropic found that short review files work better. The good ones say what failure looks like in
the repo, cap the nits, list paths to skip, and ask for a file and line as evidence. See
[How the Claude Code team does code review](https://claude.dev/blog/solving-code-review-at-anthropic/).

## Later

- The orchestrator. It will oversee a task and do the back-and-forth that maps out a spec, unless the user already has
  a plan and a spec. Planning moves to the orchestrator.
- A second agent that double-checks findings before the implementer sees them. Add it only if the task records show
  many findings ending as "not a bug".
- A spending limit per task. See idea 31 in `IDEAS.md`.
- More than one implementer or reviewer per Team.
- Risk tiers that let low-risk changes skip some checks.
- Grok, Gemini, and Cursor providers.
- A disk usage view for each task's worktrees.
