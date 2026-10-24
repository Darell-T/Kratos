//! Every message an agent receives, and the text Kratos writes from the task, which is the title, the
//! commit subject, and the branch name. The functions are pure, so the same view and run always give the
//! same text.
//!
//! Members never talk to each other. Kratos writes each message from the record, so Kratos alone decides
//! what each member sees.
//!
//! | Work      | Sections |
//! | --------- | -------- |
//! | Plan      | intro, task, the user's constraints and directions, project instructions, the previous plan and the user's feedback when revising, plan format |
//! | Implement | intro, task, how to work, project instructions, the approved plan, the brief, the answers and decisions so far, carried messages, report format |
//! | Answer    | planner intro, task, approved plan, the question and what the asker had done, answer format |
//! | Rule      | planner intro, task, approved plan, each disputed finding with its evidence and the implementer's reason, ruling format |
//! | Review    | reviewer intro, task, directions, plan and scope, files outside the scope, what to diff, project rules, open findings, findings already reported, evidence rules, probe rules, report format |
//!
//! A message that corrects the previous one in the same session carries only the problem and what to do.
//! A new or replacement session always gets the full text, because it has seen nothing.
//!
//! A message never holds the implementer's conversation or summary for the reviewer (D90), the findings
//! Kratos dropped or listed for the implementer, or the reviewer's conversation for anyone.

use kratos_protocol::Provider;

use crate::evidence::{EVIDENCE_TIMEOUT, MIN_QUOTE_LEN};
use crate::facts::ProbePolicy;
use crate::members::Role;
use crate::reports::{FindingId, Kind, ResponseOutcome, Verdict};
use crate::security_triggers::TriggerHit;
use crate::view::{
    Answerer, Brief, ClosedHow, Direction, Finding, FindingStatus, ListedWhy, Note, Pass,
    PlanState, Probe, ProbeStatus, Question, Resolution, ReviewJob, Run, TaskView, Work,
};

/// Whether the member's provider session has seen this task. Only the engine knows, so it says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Session {
    /// A new or replacement session. The message must carry everything.
    Fresh,
    /// The session has the task already.
    Continuing,
}

/// Evidence commands are written for the platform's shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    /// PowerShell.
    Windows,
    /// `sh`.
    Unix,
}

pub struct PromptContext<'a> {
    pub session: Session,
    /// `git diff --stat` for a review, as the engine ran it.
    pub diff_stat: &'a str,
    pub platform: Platform,
}

/// The message for `run`. This is the only entry point the engine needs.
pub fn message(view: &TaskView, run: &Run, ctx: &PromptContext<'_>) -> String {
    if let (Some(note), Session::Continuing) = (&run.note, ctx.session) {
        // Keep the user's queued messages in the correction. Otherwise they would be marked delivered unread.
        return sections([Some(correction(note)), carried_text(&run.carried)]);
    }
    let body = match &run.work {
        Work::Plan { author, feedback } => plan_message(view, *author, feedback.as_deref()),
        Work::Implement { brief } => implement_message(view, brief, ctx),
        Work::Answer { question } => answer_message(view, question),
        Work::Rule { disputed, .. } => ruling_message(view, disputed),
        Work::Review { job } => review_message(view, job, ctx),
    };
    let note = run
        .note
        .as_ref()
        .filter(|_| run.role != Role::Reviewer)
        .map(correction);
    sections([
        note,
        Some(body),
        resolutions_text(&run.resolutions),
        carried_text(&run.carried),
    ])
}

fn sections<I: IntoIterator<Item = Option<String>>>(parts: I) -> String {
    parts
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

// ---------------------------------------------------------------------------------------------
// The text Kratos writes from the task.
// ---------------------------------------------------------------------------------------------

const TITLE_LIMIT: usize = 60;

/// The task's first line, with headings and extra whitespace stripped, at most 60 characters. Kratos
/// names the task's commits with it, so no agent text reaches a commit. `"Task"` for a blank request.
pub fn task_title(text: &str) -> String {
    let first = text
        .lines()
        .map(|line| {
            // A control character in a title would reach the commit subject and the terminal.
            let printable: String = line
                .chars()
                .filter(|c| !c.is_control() || *c == '\t')
                .collect();
            printable
                .trim_start_matches('#')
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .find(|line| !line.is_empty())
        .unwrap_or_else(|| "Task".to_owned());
    if first.chars().count() <= TITLE_LIMIT {
        return first;
    }
    let cut: String = first.chars().take(TITLE_LIMIT - 3).collect();
    format!("{}...", cut.trim_end())
}

pub fn commit_subject(title: &str, round: u32) -> String {
    format!("{title} (round {round})")
}

const SLUG_LIMIT: usize = 40;

/// `codex/add-login`: the implementer's provider, a slash, and a slug of the title (D36). `taken` is the
/// repository's existing branch names, and a clash gets `-2`, `-3`, and so on.
pub fn branch_name(provider: Provider, title: &str, taken: &[String]) -> String {
    let mut slug = String::new();
    for character in title.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug: String = slug.chars().take(SLUG_LIMIT).collect();
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "task" } else { slug };
    let base = format!("{}/{slug}", provider.as_str());
    if !taken.contains(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken.contains(candidate))
        .unwrap_or(base)
}

/// Kratos's own step-back prompt, so the user can send it with one click.
pub const STEP_BACK_TEXT: &str = "The work has not converged, so the approach may be wrong. Before you change more code, step back. Reread the task and the findings, write down what your earlier fixes had in common and why they did not hold, then choose a different approach.";

// ---------------------------------------------------------------------------------------------
// Corrections.
// ---------------------------------------------------------------------------------------------

fn correction(note: &Note) -> String {
    match note {
        Note::InvalidReport { error } => format!(
            "Kratos could not accept your last report:\n\n{error}\n\nEnd your turn with a corrected report."
        ),
        Note::UncheckedEdits => "You changed files after the last command you ran. Run your checks after your last edit, then end your turn with your report.".to_owned(),
        Note::Lost { reason } => format!(
            "Your previous turn did not finish ({reason}). This folder may hold its partial work. Inspect it with git status and git diff, finish the work, then end your turn with your report."
        ),
        Note::VerdictMismatch { verdict, findings } => verdict_correction(*verdict, findings),
    }
}

fn verdict_correction(verdict: Verdict, findings: &[Finding]) -> String {
    let shown = findings
        .iter()
        .filter(|f| verdict == Verdict::ChangesRequested || f.is_open())
        .map(|f| format!("- {}: {}", finding_line(f), outcome(&f.status)))
        .collect::<Vec<_>>()
        .join("\n");
    match verdict {
        Verdict::Approve => format!(
            "Your verdict is approve, but Kratos confirmed findings that block:\n\n{shown}\n\nEnd your turn with a corrected report: use changes_requested, or change a finding that should not block."
        ),
        Verdict::ChangesRequested => format!(
            "Your verdict is changes_requested, but no finding blocks. This is what Kratos made of each one:\n\n{shown}\n\nEnd your turn with a corrected report: approve, or report a finding with evidence Kratos can confirm."
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// Pieces every role reads.
// ---------------------------------------------------------------------------------------------

fn task_section(view: &TaskView) -> String {
    format!("# Task\n\n{}", view.request.trim())
}

/// What the user said when they started the task and what they have said since, to anyone. Every
/// member sees all of it, so each works to what the user wants now.
fn user_words(view: &TaskView) -> Option<String> {
    let constraints = view
        .constraints
        .iter()
        .map(|constraint| format!("- {constraint}"));
    let directions = view
        .directions
        .iter()
        .map(|d| format!("- To the {}: {}", d.role, d.text.trim()));
    let lines: Vec<String> = constraints.chain(directions).collect();
    (!lines.is_empty()).then(|| format!("# What the user said\n\n{}", lines.join("\n")))
}

fn rules_section(heading: &str, files: &[(&str, &Option<String>)]) -> Option<String> {
    let present: Vec<String> = files
        .iter()
        .filter_map(|(name, text)| {
            text.as_deref()
                .map(|text| format!("## {name}\n\n{}", text.trim()))
        })
        .collect();
    (!present.is_empty()).then(|| format!("{heading}\n\n{}", present.join("\n\n")))
}

fn instructions(view: &TaskView) -> Option<String> {
    rules_section(
        "# Project instructions",
        &[
            ("AGENTS.md", &view.rules.agents_md),
            ("CLAUDE.md", &view.rules.claude_md),
        ],
    )
}

/// The approved plan with the conditions the user attached and the changes the user allowed.
fn plan_section(view: &TaskView) -> Option<String> {
    let PlanState::Approved {
        plan, conditions, ..
    } = &view.plan
    else {
        return None;
    };
    let done: Vec<String> = plan.done_means.iter().map(|d| format!("- {d}")).collect();
    let scope: Vec<String> = view
        .scope()
        .patterns
        .iter()
        .map(|p| format!("- {}", p.as_str()))
        .collect();
    Some(sections([
        Some(format!(
            "# The approved plan\n\nBuild: {}",
            plan.build.trim()
        )),
        Some(format!("Done means:\n{}", done.join("\n"))),
        Some(format!("Files in scope:\n{}", scope.join("\n"))),
        conditions
            .as_ref()
            .map(|text| format!("The user's conditions for the plan:\n{}", text.trim())),
        amendments_text(view),
    ]))
}

fn amendments_text(view: &TaskView) -> Option<String> {
    let lines: Vec<String> = view
        .amendments
        .iter()
        .map(|a| {
            let files: Vec<&str> = a.extra_scope.iter().map(|p| p.as_str()).collect();
            let extra = if files.is_empty() {
                String::new()
            } else {
                format!(" Added to the scope: {}.", files.join(", "))
            };
            format!("- {}{extra}", a.reason.trim())
        })
        .collect();
    (!lines.is_empty()).then(|| format!("Changes the user allowed:\n{}", lines.join("\n")))
}

fn finding_line(finding: &Finding) -> String {
    let location = &finding.report.location;
    format!(
        "{} {} at {}:{}",
        finding.id, finding.report.class, location.path, location.line
    )
}

fn finding_text(finding: &Finding) -> String {
    let report = &finding.report;
    let evidence = if report.kind == Kind::Bug {
        format!(
            "Evidence, a command that exits non-zero on the reviewed commit: {}",
            report.evidence
        )
    } else {
        format!("Evidence, the rule it breaks: \"{}\"", report.evidence)
    };
    [
        format!(
            "## {}: {} ({})",
            finding.id,
            report.class,
            kind_name(report.kind)
        ),
        format!(
            "Location: {}:{}",
            report.location.path, report.location.line
        ),
        format!("Problem: {}", report.problem),
        evidence,
        format!("Fixed when: {}", report.fixed_when),
    ]
    .join("\n")
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Bug => "bug",
        Kind::Constraint => "constraint",
        Kind::Requirement => "requirement",
        Kind::Design => "design",
    }
}

fn outcome(status: &FindingStatus) -> String {
    match status {
        FindingStatus::Open { .. } => "confirmed, and it blocks".to_owned(),
        FindingStatus::Closed { how } => match how {
            ClosedHow::Fixed => "fixed".to_owned(),
            ClosedHow::RuledNotBug => "ruled not a bug".to_owned(),
        },
        FindingStatus::Dropped { reason } => {
            let why = reason.why.explanation();
            let detail = reason.detail.trim();
            if detail.is_empty() {
                format!("dropped, because {why}")
            } else {
                format!("dropped, because {why}: {}", tail(detail, 300))
            }
        }
        FindingStatus::Listed { why } => match why {
            ListedWhy::Nit => "listed for the user as a nit",
            ListedWhy::PreExisting => "listed for the user as a pre-existing problem",
            ListedWhy::Design => "listed for the user as a design opinion",
            ListedWhy::OutsideDiff => {
                "listed for the user, because it is outside the lines changed since the last review"
            }
        }
        .to_owned(),
    }
}

fn tail(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text.to_owned();
    }
    let kept: String = text.chars().skip(count - limit).collect();
    format!("...{kept}")
}

// ---------------------------------------------------------------------------------------------
// Resolutions and carried messages. They record what happened to this step while it was suspended.
// ---------------------------------------------------------------------------------------------

fn resolutions_text(resolutions: &[Resolution]) -> Option<String> {
    if resolutions.is_empty() {
        return None;
    }
    let lines: Vec<String> = resolutions
        .iter()
        .map(|resolution| match resolution {
            Resolution::Answered {
                question,
                answer,
                by,
            } => {
                let by = match by {
                    Answerer::Planner => "the planner",
                    Answerer::User => "the user",
                };
                format!(
                    "You asked: {}\nAnswer from {by}: {}",
                    question.trim(),
                    answer.trim()
                )
            }
            Resolution::PlanChanged {
                reason,
                allowed,
                instructions,
            } => {
                let verdict = if *allowed {
                    "The user allowed the plan change you asked for"
                } else {
                    "The user declined the plan change you asked for. Keep to the plan"
                };
                let extra = instructions
                    .as_deref()
                    .map(|text| format!(" The user adds: {}", text.trim()))
                    .unwrap_or_default();
                format!("{verdict} ({}).{extra}", reason.trim())
            }
        })
        .collect();
    Some(format!(
        "# Answers to your questions\n\n{}",
        lines.join("\n\n")
    ))
}

fn carried_text(carried: &[Direction]) -> Option<String> {
    if carried.is_empty() {
        return None;
    }
    let lines: Vec<String> = carried.iter().map(|d| d.text.trim().to_owned()).collect();
    Some(format!(
        "# Messages from the user\n\nThe user wrote to you while you were busy:\n\n{}",
        lines.join("\n\n")
    ))
}

// ---------------------------------------------------------------------------------------------
// Plan.
// ---------------------------------------------------------------------------------------------

const NEVER_GUESS: &str = "Never guess. When something is unclear or outside what you may decide, end your turn with status question and ask the user.";

fn plan_message(view: &TaskView, author: Role, feedback: Option<&str>) -> String {
    let intro = if author == Role::Planner {
        "You are the planner on a Kratos Team. You write the plan that an implementer follows and a reviewer checks the work against. You never write code, and you read the project without changing any file. Later you may answer a question from the implementer or the reviewer, or rule on a dispute between them."
    } else {
        "You are the implementer on a Kratos Team. A separate reviewer checks your work, and Kratos carries every message between you. Before you build, write the plan. Do not change any file."
    };
    let revising = match (&view.plan, feedback) {
        (PlanState::Drafted { plan, .. } | PlanState::Approved { plan, .. }, Some(feedback)) => {
            Some(format!(
                "# The plan so far\n\nBuild: {}\nDone means: {}\nFiles in scope: {}\n\n# What the user wants changed\n\n{}",
                plan.build.trim(),
                plan.done_means.join("; "),
                plan.scope
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                feedback.trim()
            ))
        }
        (_, Some(feedback)) => Some(format!("# What the user wants\n\n{}", feedback.trim())),
        (_, None) => None,
    };
    sections([
        Some(intro.to_owned()),
        Some(task_section(view)),
        user_words(view),
        instructions(view),
        revising,
        Some(PLAN_FORMAT.to_owned()),
        Some(NEVER_GUESS.to_owned()),
    ])
}

const PLAN_FORMAT: &str = "# Your report\n\nExplore the code, then end your turn with the plan:\n- status: done when the plan is ready, blocked when something outside your control stops you, or question when you need an answer first.\n- build: what to build.\n- done_means: short statements a reviewer can check.\n- files_in_scope: the files the work may touch, as globs relative to the repository root such as src/login/**. Write ** only when every file is in scope.\nThe user reads the plan and approves it before any code is written.";

// ---------------------------------------------------------------------------------------------
// Implement.
// ---------------------------------------------------------------------------------------------

const IMPLEMENTER_INTRO: &str = "You are the implementer on a Kratos Team. A separate reviewer checks your work, and Kratos carries every message between you.";

const REPORT_REMINDER: &str =
    "When you finish, run your checks after your last edit, then end your turn with your report.";

const RESPONSES_RULE: &str = "In your report, answer each finding in responses with its id: fixed, or disputed with a reason. A dispute goes to the planner or the user, not to the reviewer.";

const IMPLEMENTER_REPORT: &str = "# Your report\n\nEnd your turn with your report:\n- status: done when the task is complete, blocked when something outside your control stops you, question when you need an answer before you can go on, or plan_change when the plan must change. Needing a file outside the plan's scope is a plan change. Do not widen the scope yourself.\n- summary: what changed and why, in a few sentences.\n- checks: each command you ran to verify the work, with its exit code.\n- responses: one entry for each finding Kratos sends you. Leave it empty until then.\n- added_probes: probe tests you were told to add. Leave it empty otherwise.\n- ask: fill in only when status is question or plan_change.";

fn implement_message(view: &TaskView, brief: &Brief, ctx: &PromptContext<'_>) -> String {
    let fresh = ctx.session == Session::Fresh;
    match brief {
        Brief::Start => start_text(view, false),
        Brief::Planned if fresh => sections([
            Some(start_text(view, true)),
            Some("The user approved the plan. Carry it out.".to_owned()),
        ]),
        Brief::Planned => sections([
            Some("The user approved the plan. Carry it out.".to_owned()),
            plan_section(view),
            Some(REPORT_REMINDER.to_owned()),
        ]),
        other => sections([fresh.then(|| takeover(view)), Some(brief_text(view, other))]),
    }
}

fn work_rules(view: &TaskView) -> String {
    format!(
        "# How to work

- Work only in this folder. It is a git worktree on branch {}.
- Do not commit. Kratos commits your work when you hand it off.
- Run the project's checks after your last edit. Kratos returns a handoff with a file changed after the last command you ran.
- Stay inside the agreed scope: the plan's files, or the task's when there is no plan. If you need more files, report plan_change and let the user decide.",
        view.branch
    )
}

/// The first message of a new implementer session. It holds the task, how to work, and the plan when there is one.
fn start_text(view: &TaskView, with_plan: bool) -> String {
    sections([
        Some(IMPLEMENTER_INTRO.to_owned()),
        Some(task_section(view)),
        user_words(view),
        Some(work_rules(view)),
        instructions(view),
        with_plan.then(|| plan_section(view)).flatten(),
        Some(IMPLEMENTER_REPORT.to_owned()),
    ])
}

/// A replacement session has seen nothing, so it gets the task and the state of the work.
fn takeover(view: &TaskView) -> String {
    sections([
        Some("You are the implementer on a Kratos Team, taking over this task from an earlier session that was cut off. Inspect this folder with git status, git diff, and git log before you change anything.".to_owned()),
        Some(task_section(view)),
        user_words(view),
        Some(work_rules(view)),
        instructions(view),
        plan_section(view),
        Some(IMPLEMENTER_REPORT.to_owned()),
    ])
}

/// The brief of a step that is not the first one. `Start` and `Planned` are written by `start_text`.
fn brief_text(view: &TaskView, brief: &Brief) -> String {
    match brief {
        Brief::Start | Brief::Planned => String::new(),
        Brief::Findings => sections([
            Some("The reviewer found these problems, and Kratos confirmed each one. Fix every instance of each class, not only the line named.".to_owned()),
            Some(finding_list(view)),
            probe_offer(view),
            Some(RESPONSES_RULE.to_owned()),
            Some(REPORT_REMINDER.to_owned()),
        ]),
        Brief::Direction { text } => sections([
            Some(format!("Direction from the user:\n\n{}", text.trim())),
            still_open(view),
            Some(REPORT_REMINDER.to_owned()),
        ]),
        Brief::StepBack => sections([
            Some(STEP_BACK_TEXT.to_owned()),
            still_open(view),
            Some(REPORT_REMINDER.to_owned()),
        ]),
        Brief::Rulings { bugs } => {
            let ids: Vec<String> = bugs.iter().map(ToString::to_string).collect();
            sections([
                Some(format!(
                    "The disputed findings were ruled on. These are real, so fix them: {}.",
                    ids.join(", ")
                )),
                Some(finding_list(view)),
                probe_offer(view),
                Some(RESPONSES_RULE.to_owned()),
                Some(REPORT_REMINDER.to_owned()),
            ])
        }
        Brief::KeepProbes { paths } => sections([
            Some(format!(
                "The user chose these probe tests to keep as permanent tests: {}. Add each to the project's tests, in the project's own style, and list each in added_probes with the place you put it. Change nothing else.",
                paths.join(", ")
            )),
            Some(REPORT_REMINDER.to_owned()),
        ]),
    }
}

fn still_open(view: &TaskView) -> Option<String> {
    view.open_findings().next().map(|_| {
        sections([
            Some(format!("These findings are still open. {RESPONSES_RULE}")),
            Some(finding_list(view)),
        ])
    })
}

fn finding_list(view: &TaskView) -> String {
    let mut open: Vec<&Finding> = view.open_findings().collect();
    open.sort_by_key(|f| f.rank());
    open.iter()
        .map(|finding| sections([Some(finding_text(finding)), history(finding)]))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn history(finding: &Finding) -> Option<String> {
    let FindingStatus::Open {
        attempts,
        diagnosis,
        ..
    } = &finding.status
    else {
        return None;
    };
    let diagnosis = diagnosis
        .as_deref()
        .map(|text| format!("The reviewer's diagnosis: {text}"));
    if *attempts == 0 {
        return diagnosis;
    }
    let tries = if *attempts == 1 {
        "You have tried to fix this once.".to_owned()
    } else {
        format!("You have tried to fix this {attempts} times.")
    };
    let rerun = (diagnosis.is_none() && finding.report.kind == Kind::Bug)
        .then(|| "Its evidence command still fails on your last commit.".to_owned());
    Some(
        [Some(tries), diagnosis, rerun]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// When the user delegated probe retention, the implementer gets the probes for the findings it fixes
/// and the user's constraints (D78).
fn probe_offer(view: &TaskView) -> Option<String> {
    let ProbePolicy::Delegated { constraints } = &view.policy.probes else {
        return None;
    };
    let offered: Vec<String> = view
        .probes
        .iter()
        .filter(|probe| probe.status == ProbeStatus::Offered)
        .filter(|probe| {
            probe
                .finding
                .is_some_and(|id| view.open_findings().any(|f| f.id == id))
        })
        .map(probe_text)
        .collect();
    let folders: Vec<&str> = constraints.test_paths.iter().map(|p| p.as_str()).collect();
    Some(sections([
        Some(format!(
            "# Probe tests\n\nThe user lets you decide which of the reviewer's probe tests become permanent tests. You may add a probe only under these paths: {}. The user adds: {}\n\nKeep a probe only if it fails before your fix and passes after it. List each one you add in added_probes. Kratos checks every one.",
            folders.join(", "),
            constraints.note.trim()
        )),
        (!offered.is_empty()).then(|| offered.join("\n\n")),
    ]))
}

/// How much of a probe file a message shows, so one big file cannot fill the implementer's context.
const PROBE_SHOWN_CHARS: usize = 4000;

fn probe_text(probe: &Probe) -> String {
    let showed = probe
        .showed
        .as_deref()
        .map(|output| format!("\nIt showed the bug with:\n{}", tail(output.trim(), 600)))
        .unwrap_or_default();
    let content = match &probe.content {
        Some(text) => {
            let shown: String = text.chars().take(PROBE_SHOWN_CHARS).collect();
            let is_cut = probe.truncated || shown.len() < text.len();
            let note = if is_cut { "\n(cut short)" } else { "" };
            format!("\n```text\n{shown}\n```{note}")
        }
        None => "\n(the file is not shown)".to_owned(),
    };
    format!("Probe {}{showed}{content}", probe.path)
}

// ---------------------------------------------------------------------------------------------
// Answer and rule, the planner's jobs after the plan.
// ---------------------------------------------------------------------------------------------

fn answer_message(view: &TaskView, question: &Question) -> String {
    let record = (question.role == Role::Reviewer)
        .then(|| implementer_record(view))
        .flatten();
    sections([
        Some(format!(
            "You are the planner on a Kratos Team. The {} asked you a question. Answer it from the task, the plan, and the record below. You read the project without changing any file.",
            question.role
        )),
        Some(task_section(view)),
        user_words(view),
        plan_section(view),
        record,
        Some(format!(
            "# The question\n\nThe {} had done this so far: {}\n\nThe question: {}",
            question.role,
            question.summary.trim(),
            question.text.trim()
        )),
        Some(format!(
            "# Your report\n\nEnd your turn with status done and your answer in answer. {NEVER_GUESS} If your answer would contradict the plan, report plan_change."
        )),
    ])
}

/// What the implementer reported at each handoff, so a planner can answer a reviewer's question about
/// the implementer's reasons without the implementer's conversation (D90).
fn implementer_record(view: &TaskView) -> Option<String> {
    let lines: Vec<String> = view
        .rounds
        .iter()
        .filter_map(|round| {
            round
                .report
                .as_ref()
                .map(|report| format!("- Round {}: {}", round.n, report.summary.trim()))
        })
        .collect();
    (!lines.is_empty()).then(|| format!("# What the implementer reported\n\n{}", lines.join("\n")))
}

fn ruling_message(view: &TaskView, disputed: &[FindingId]) -> String {
    let findings: Vec<String> = disputed
        .iter()
        .filter_map(|id| view.findings.iter().find(|f| f.id == *id))
        .map(|finding| {
            let reason = match &finding.status {
                FindingStatus::Open {
                    response: Some(ResponseOutcome::Disputed { reason }),
                    ..
                } => reason.trim().to_owned(),
                _ => String::new(),
            };
            sections([
                Some(finding_text(finding)),
                Some(format!("The implementer disputes it: {reason}")),
            ])
        })
        .collect();
    sections([
        Some("You are the planner on a Kratos Team. The implementer disputes these findings. Rule on each one: bug if the finding is real and the implementer must fix it, or not_bug if it is not. You read the project without changing any file.".to_owned()),
        Some(task_section(view)),
        user_words(view),
        plan_section(view),
        Some(format!("# Disputed findings\n\n{}", findings.join("\n\n"))),
        Some(format!("# Your report\n\nEnd your turn with status done and one entry in rulings for each disputed finding, with a reason. {NEVER_GUESS}")),
    ])
}

// ---------------------------------------------------------------------------------------------
// Review.
// ---------------------------------------------------------------------------------------------

const REVIEWER_INTRO: &str = "You are the reviewer on a Kratos Team. Review one commit against the task, what the user said, and the approved plan. You see the implementer's work, never its conversation. If you need the implementer's reasons, ask a question. Do not guess at them.";

const SECURITY_INTRO: &str = "You are the security reviewer on a Kratos Team. The change for this task touches code that the project's security triggers watch, or it is large. Review the whole change for security problems only. You see the implementer's work, never its conversation.";

const REGRESSIONS_INTRO: &str = "You are the regressions reviewer on a Kratos Team. This change is large. Review the whole change for what it breaks that already worked: callers of changed code, changed contracts, removed behavior, and tests that no longer cover what they did. You see the implementer's work, never its conversation.";

fn review_message(view: &TaskView, job: &ReviewJob, ctx: &PromptContext<'_>) -> String {
    let (intro, focus) = match &job.pass {
        Pass::Standard => (REVIEWER_INTRO, None),
        Pass::Security { hits } => (SECURITY_INTRO, Some(security_focus(hits))),
        Pass::Regressions => (REGRESSIONS_INTRO, None),
    };
    let is_standard = matches!(job.pass, Pass::Standard);
    sections([
        Some(intro.to_owned()),
        Some(task_section(view)),
        user_words(view),
        plan_section(view),
        outside_scope_text(view),
        Some(what_to_review(view, job, ctx.diff_stat)),
        focus,
        rules_section(
            "# Project rules",
            &[
                ("AGENTS.md", &view.rules.agents_md),
                ("CLAUDE.md", &view.rules.claude_md),
                ("REVIEW.md", &view.rules.review_md),
            ],
        ),
        is_standard.then(|| open_to_review(view)).flatten(),
        already_reported(view),
        Some(evidence_rules(ctx.platform)),
        Some(probe_rules(is_standard)),
        Some(REVIEW_REPORT.to_owned()),
    ])
}

fn what_to_review(view: &TaskView, job: &ReviewJob, diff_stat: &str) -> String {
    let whole = job.diff_base == view.base;
    let stat = diff_stat.trim_end();
    sections([
        Some("# What to review".to_owned()),
        Some(format!(
            "This folder is checked out at commit {}.",
            job.commit
        )),
        Some(
            if whole {
                "Review the whole change for this task:"
            } else {
                "Review what changed since the last review:"
            }
            .to_owned(),
        ),
        Some(format!("    git diff {} {}", job.diff_base, job.commit)),
        (!stat.trim().is_empty()).then(|| format!("```text\n{stat}\n```")),
        (!whole).then(|| "Also check every open finding below.".to_owned()),
    ])
}

/// The changed files the agreed scope does not cover. The reviewer judges them, because lockfiles and
/// generated files change as side effects.
fn outside_scope_text(view: &TaskView) -> Option<String> {
    let outside = view.outside_scope();
    if outside.is_empty() {
        return None;
    }
    let lines: Vec<String> = outside.iter().map(|path| format!("- {path}")).collect();
    Some(format!(
        "# Changed files outside the agreed scope\n\nThe implementer changed these files, and the scope does not cover them. Judge each: a lockfile or a generated file is expected, and anything else is a finding.\n\n{}",
        lines.join("\n")
    ))
}

fn security_focus(hits: &[TriggerHit]) -> String {
    let why = if hits.is_empty() {
        "The change is large.".to_owned()
    } else {
        hits.iter().map(hit_text).collect::<Vec<_>>().join("\n")
    };
    format!(
        "# Why this pass runs\n\n{why}\n\n# What to look for\n\n- Command execution: argument injection, shell interpolation, untrusted paths.\n- Files: path traversal, writes outside the intended folders, links.\n- Network: unvalidated hosts, missing TLS, listeners beyond localhost.\n- Secrets: credentials in code, logs, errors, or URLs.\n- Permissions and approvals: checks that can be skipped, and defaults that widen access.\n- Untrusted input that crosses a boundary without validation."
    )
}

fn hit_text(hit: &TriggerHit) -> String {
    match hit {
        TriggerHit::Path { glob, path } => format!("- {path} matches the path trigger {glob}"),
        TriggerHit::Code {
            pattern,
            path,
            line,
        } => format!("- {path} adds a line that matches /{pattern}/: {line}"),
    }
}

fn open_to_review(view: &TaskView) -> Option<String> {
    let open: Vec<&Finding> = view.open_findings().collect();
    if open.is_empty() {
        return None;
    }
    let how = "For each one, check its fixed-when and run its evidence again. If this commit does not fix it, list it in still_open with a diagnosis: what is still wrong, the evidence, and why the fix did not work. Kratos reruns a bug's evidence command itself, and the exit code decides. Any other finding you leave out counts as fixed.";
    let listed: Vec<String> = open
        .iter()
        .map(|finding| format!("{}\n{}", finding_text(finding), response_text(finding)))
        .collect();
    Some(format!(
        "# Open findings\n\n{how}\n\n{}",
        listed.join("\n\n")
    ))
}

fn response_text(finding: &Finding) -> String {
    match &finding.status {
        FindingStatus::Open {
            response: Some(ResponseOutcome::Fixed),
            ..
        } => "The implementer's response: fixed.".to_owned(),
        FindingStatus::Open {
            response: Some(ResponseOutcome::Disputed { reason }),
            ..
        } => format!("The implementer's response: disputed. {reason}"),
        _ => "The implementer's response: none.".to_owned(),
    }
}

fn already_reported(view: &TaskView) -> Option<String> {
    if view.findings.is_empty() {
        return None;
    }
    let lines: Vec<String> = view
        .findings
        .iter()
        .map(|f| format!("- {}: {}", finding_line(f), outcome(&f.status)))
        .collect();
    Some(format!(
        "# Already reported\n\nDo not report these again.\n\n{}",
        lines.join("\n")
    ))
}

fn evidence_rules(platform: Platform) -> String {
    let shell = match platform {
        Platform::Windows => "PowerShell",
        Platform::Unix => "sh",
    };
    format!(
        "# Evidence\n\nKratos checks every important finding before the implementer sees it, and drops a finding it cannot confirm.\n- A bug's evidence is one {shell} command that exits non-zero on this commit. Kratos runs it in this folder after resetting tracked files to the commit, with a {} second limit. Untracked files you add, such as a probe test, stay for the run. A command that passes, times out, or is refused proves nothing.\n- A requirement's or constraint's evidence is a quote of at least {MIN_QUOTE_LEN} characters, copied exactly from the task, what the user said, the approved plan, AGENTS.md, CLAUDE.md, or REVIEW.md.\n- A location is a path relative to the repository root, and a line that exists in this commit.\n- Only important findings can block. Nits, pre-existing problems, and design opinions go to the user and never block. After round 1, a finding outside the lines changed since the last review was missed earlier, so it goes to the user and does not block.\n- Kratos shows the user the findings ranked by severity, so set severity and kind with care.",
        EVIDENCE_TIMEOUT.as_secs()
    )
}

fn probe_rules(is_standard: bool) -> String {
    let scope = if is_standard {
        ""
    } else {
        " Probes are for bugs you can show."
    };
    format!(
        "# Probe tests\n\nTo show a bug, you may write a test file in this folder and run it. Name each probe file in the finding's probes. Do not edit any implementation file: you only add probe files.{scope}"
    )
}

const REVIEW_REPORT: &str = "# Your report\n\nEnd your turn with your report:\n- status: done when you finished the review, blocked when only the user can decide (Kratos checks no finding then), or question when you need an answer first.\n- verdict: approve when nothing blocks, or changes_requested when an important finding blocks. Your verdict must agree with the findings Kratos confirms.\n- summary: start with the number of findings, and say plainly when there is nothing to fix.\n- findings: new findings only.\n- still_open: earlier findings this commit does not fix, each with a diagnosis.\n- ask: fill in only when status is question.";
