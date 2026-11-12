# About skills across providers (open design)

This note records where skills stand after R4 and what the unified skills design must settle. Nothing in it is decided yet. The design talk comes after R4.

## What each provider offers today

`GET /api/threads/{id}/skills` returns what the thread's CLI reports for the thread's folder (D97).

- A Codex thread lists the skills Codex reports. Codex reports no slash commands, and the reply says so.
- A Claude thread lists the slash commands from the session's `initialize` reply. Claude lists its skills among them, and the route starts a session when none runs.

The two lists come from two CLIs with two skill folders. A skill the user writes for one provider does not appear on the other.

## Why a Claude agent cannot use a skill

Kratos decides every Claude tool permission itself (D21). The rule table in `crates/kratos-engine/src/providers/claude/permissions.rs` names each tool it allows or asks about. Any other tool reads as `Action::Invalid`, and `policy::decide` denies it. The `Skill`, `Agent`, and `Task` tools fall in that group, so a Claude agent that calls a skill gets a denial.

Kratos also starts Claude with `--setting-sources=user` (`session.rs`). Project settings, hooks, and skills in the thread's folder do not load, which live lane 7 checks. Only the user's own skills reach the slash-command list. D97 wanted project skills listed too, so the two rules conflict.

The deny is deliberate for now. A skill can carry its own instructions and scripts, and Kratos has no rule yet for what a skill may do inside a thread's permission.

## What the unified design must answer

- Where skills live, so one skill serves Claude and Codex threads alike.
- How Kratos finds them. The working idea is to watch the skill folders and reload a thread's list when a folder changes.
- Which permission rule the `Skill` tool gets, and whether a skill's scripts run through Kratos's `run_command` and its sandbox like every other command.
- Whether project skills load for Claude while project settings and hooks stay off.
- What the slash-command list shows for a skill a provider cannot run.

Until then, the `Skill`, `Agent`, and `Task` tools stay denied, and the skills route reports what each CLI reports.
