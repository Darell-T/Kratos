//! A scripted task is the engine's loop in miniature, with scripted agents, git, and evidence commands.
//!
//! Every scenario builds a `ScriptedTask`, lets it run until the task waits, and asserts on the view
//! and on the steps it ran. The loop is the one the engine runs, so these tests also prove that the
//! public interface is enough to drive a task: `fold`, `next_step`, `accepts`, `check_decision`, and the
//! two-call evidence check. It reads nothing but facts and writes nothing but facts.

use std::collections::{BTreeMap, VecDeque};

use kratos_protocol::{CommandResult, Permission, Provider, TurnChanges};
use kratos_teams::decisions::{Decision, allowed_decisions, check_decision};
use kratos_teams::evidence::{Diff, Seen, Start, finish_check, rerun, start_check};
use kratos_teams::facts::{
    ChangeSize, ContinuationId, Declined, DeliveryId, Ended, EvidenceChecked, Fact, FactSeq,
    HandoffOutcome, LargeChange, MessageId, ProbePolicy, ProbeRun, RoundCap, RulesSnapshot, Sha,
    StepId, StoredFact, TaskPolicy, TaskStarted,
};
use kratos_teams::fold::{Rejection, fold};
use kratos_teams::members::{Bindings, Member, MemberId, Role};
use kratos_teams::next_step::{CheckStep, Step, next_step};
use kratos_teams::prompts::{branch_name, task_title};
use kratos_teams::reports::{
    AddedProbe, AnswerDone, Check, FindingId, FindingReport, ImplementerDone, Kind, Location,
    Report, Reported, Response, ResponseOutcome, ReviewDone, Ruling, RulingVerdict, RulingsDone,
    Severity, StillOpen, Verdict,
};
use kratos_teams::security_triggers::{Trigger, TriggerHit};
use kratos_teams::task_plan::{PlanChangeRequest, ScopePattern, TaskPlan};
use kratos_teams::view::{Activity, Phase, TaskView};

/// No scenario needs more steps than this from one helper call, so more means the rules loop.
const MAX_STEPS: usize = 300;

// ---------------------------------------------------------------------------------------------
// The script says what the agents, git, and the sandbox would do.
// ---------------------------------------------------------------------------------------------

/// How the scheduler answers a step handed to it.
pub enum Scheduler {
    Schedules,
    Declines(Declined),
}

/// What each role's next turns end with, and what evidence commands print. Running dry panics with the
/// role's name, because a scenario that asks for more turns than it scripted is wrong.
#[derive(Default)]
pub struct Script {
    pub planner: VecDeque<Ended>,
    pub implementer: VecDeque<Ended>,
    pub reviewer: VecDeque<Ended>,
    /// Results of an evidence command, by command text. They are used in order, and the last repeats.
    pub commands: BTreeMap<String, Vec<CommandResult>>,
    /// Hits the security scan returns, one entry per scan.
    pub scans: VecDeque<Vec<TriggerHit>>,
    /// Findings that land outside the lines changed since the last review, by class.
    pub outside_diff: Vec<String>,
    pub scheduler: VecDeque<Scheduler>,
    pub handoffs: VecDeque<HandoffOutcome>,
    pub role_change_fails: bool,
    /// Sizes of the commits, by round. Rounds not listed are small.
    pub sizes: BTreeMap<u32, ChangeSize>,
    /// What git shows about each added probe, by the reviewer's path.
    pub probe_runs: BTreeMap<String, ProbeRun>,
    /// Files the reviewer left, copied before the evidence run.
    pub probes: Vec<kratos_teams::evidence::ProbeCapture>,
    /// The files each round's commit changed.
    pub files: Vec<String>,
    pub ran: BTreeMap<String, usize>,
}

impl Script {
    /// How many evidence commands ran, counting a rerun of the same command again.
    pub fn commands_run(&self) -> usize {
        self.ran.values().sum()
    }
}

impl Script {
    fn next_ended(&mut self, role: Role) -> Ended {
        let queue = match role {
            Role::Planner => &mut self.planner,
            Role::Implementer => &mut self.implementer,
            Role::Reviewer => &mut self.reviewer,
        };
        queue
            .pop_front()
            .unwrap_or_else(|| panic!("the {role} script ran out"))
    }

    /// The command's scripted results in order, then the last one again. A command nobody scripted exits 0.
    fn run_command(&mut self, command: &str) -> CommandResult {
        let count = self.ran.entry(command.to_owned()).or_default();
        let result = self
            .commands
            .get(command)
            .and_then(|results| results.get(*count).or(results.last()));
        *count += 1;
        result.cloned().unwrap_or_else(passes)
    }

    fn evidence_for(&mut self, step: &CheckStep) -> EvidenceChecked {
        let sources: Vec<&str> = step.sources.iter().map(String::as_str).collect();
        let outside = self.outside_diff.clone();
        let mut checks = BTreeMap::new();
        for candidate in &step.candidates {
            // Every confirmed finding is inside the diff, except the classes the scenario put outside it.
            let diff =
                (step.ranges_base.is_some()).then(|| diff_covering(&candidate.report, &outside));
            let seen = Seen {
                file_text: Some("one\ntwo\nthree\n"),
                sources: &sources,
                diff: diff.as_ref(),
            };
            let check = match start_check(&candidate.report, &seen) {
                Start::NothingToCheck => continue,
                Start::Done(done) => done,
                Start::RunCommand(command) => finish_check(
                    &candidate.report,
                    &self.run_command(&command),
                    diff.as_ref(),
                ),
            };
            checks.insert(candidate.id, check);
        }
        let rechecks = step
            .recheck
            .iter()
            .map(|open| (open.id, rerun(&self.run_command(&open.report.evidence))))
            .collect();
        let security_hits = step
            .scan
            .as_ref()
            .map(|_| self.scans.pop_front().unwrap_or_default());
        EvidenceChecked {
            checks,
            rechecks,
            probes: std::mem::take(&mut self.probes),
            security_hits,
        }
    }
}

/// A diff whose changed lines include the finding's line, unless its class is in `outside`.
fn diff_covering(report: &FindingReport, outside: &[String]) -> Diff {
    if outside.contains(&report.class) {
        return Diff::default();
    }
    let text = format!(
        "diff --git a/{0} b/{0}\n--- a/{0}\n+++ b/{0}\n@@ -{1} +{1} @@\n-old\n+new\n",
        report.location.path, report.location.line
    );
    Diff::parse(&text)
}

// ---------------------------------------------------------------------------------------------
// The runner.
// ---------------------------------------------------------------------------------------------

pub struct ScriptedTask {
    log: Vec<StoredFact>,
    continuations: u32,
    pub script: Script,
    /// Every step the loop ran, in order.
    pub steps: Vec<Step>,
}

impl ScriptedTask {
    pub fn start(started: TaskStarted, script: Script) -> ScriptedTask {
        let first = StoredFact {
            seq: FactSeq(1),
            fact: Fact::TaskStarted {
                started: Box::new(started),
            },
        };
        ScriptedTask {
            log: vec![first],
            continuations: 0,
            script,
            steps: Vec::new(),
        }
    }

    /// A restart folds the same facts again.
    pub fn view(&self) -> TaskView {
        fold(&self.log).expect("a log the harness built folds")
    }

    pub fn log(&self) -> &[StoredFact] {
        &self.log
    }

    /// The engine's short transaction. It folds the latest facts, asks whether the fact still applies, and
    /// appends it once.
    pub fn append(&mut self, fact: Fact) -> Result<(), Rejection> {
        self.view().accepts(&fact)?;
        let seq = FactSeq(self.log.len() as u64 + 1);
        self.log.push(StoredFact { seq, fact });
        Ok(())
    }

    /// A task that restarted from the first facts of an earlier log.
    pub fn restart_from(log: &[StoredFact], script: Script) -> ScriptedTask {
        ScriptedTask {
            log: log.to_vec(),
            continuations: 0,
            script,
            steps: Vec::new(),
        }
    }

    /// The step the rules name next.
    pub fn next(&self) -> Step {
        next_step(&self.view())
    }

    /// Runs the one step the rules name next, unless the task waits. Returns its name.
    pub fn step(&mut self) -> Option<String> {
        let view = self.view();
        let step = next_step(&view);
        let fact = match &step {
            Step::Wait { on } => match engine_turn(&view) {
                Some(message) if matches!(on, kratos_teams::next_step::Wait::Engine { .. }) => {
                    self.continuation_result(&view, message)
                }
                _ => return None,
            },
            other => self.execute(other),
        };
        let name = step_name(&step);
        self.steps.push(step);
        self.append(fact).expect("the scripted result applies");
        Some(name)
    }

    /// Runs steps until the task waits, and returns their names.
    pub fn run(&mut self) -> Vec<String> {
        let mut ran = Vec::new();
        while let Some(name) = self.step() {
            ran.push(name);
            assert!(
                ran.len() < MAX_STEPS,
                "the task did not settle in {MAX_STEPS} steps"
            );
        }
        ran
    }

    /// Runs steps until `reached` holds. A task that stops to wait for the user first fails the test
    /// at once, naming the step it stopped at, so a broken rule fails a test instead of hanging it.
    pub fn step_until(&mut self, reached: impl Fn(&ScriptedTask) -> bool) {
        for _ in 0..MAX_STEPS {
            if reached(self) {
                return;
            }
            let stopped_at = step_name(&self.next());
            assert!(
                self.step().is_some(),
                "the task stopped at {stopped_at} before reaching the step the test waits for"
            );
        }
        panic!("the step the test waits for did not come within {MAX_STEPS} steps");
    }

    /// The scheduled continuation's turn ends with the next scripted result for its role.
    fn continuation_result(&mut self, view: &TaskView, message: MessageId) -> Fact {
        let role = match &view.phase {
            Phase::Active {
                activity: Activity::Turn { turn },
            } => turn.work.role(),
            _ => panic!("a continuation waits in a turn"),
        };
        Fact::TurnEnded {
            message,
            ended: Box::new(self.script.next_ended(role)),
        }
    }

    /// The user decides. The harness runs the same check the engine runs, then records the fact, then runs the loop.
    pub fn decide(&mut self, decision: Decision) -> Vec<String> {
        let view = self.view();
        assert!(
            allowed_decisions(&view).contains(&decision.kind()),
            "{} is not allowed while the task is {}",
            decision.kind(),
            view.phase.name()
        );
        check_decision(&view, &decision).expect("the decision applies");
        self.append(Fact::UserDecided { decision })
            .expect("the decision applies");
        self.run()
    }

    /// A decision the user makes while an agent works. The loop does not run afterwards.
    pub fn decide_while_working(&mut self, decision: Decision) {
        let view = self.view();
        check_decision(&view, &decision).expect("the decision applies");
        self.append(Fact::UserDecided { decision })
            .expect("the decision applies");
    }

    /// A fact outside the loop, such as a user message, a disposition, or a failed step.
    pub fn record(&mut self, fact: Fact) {
        self.append(fact).expect("the fact applies");
    }

    fn execute(&mut self, step: &Step) -> Fact {
        match step {
            Step::Prepare => Fact::Prepared,
            Step::Run { run } => Fact::TurnEnded {
                message: run.message.clone(),
                ended: Box::new(self.script.next_ended(run.role)),
            },
            Step::Commit { commit } => self.committed(commit.round, &commit.added_probes),
            Step::Check { check } => Fact::EvidenceChecked {
                checked: Box::new(self.script.evidence_for(check)),
            },
            Step::ApplyRoles { .. } if self.script.role_change_fails => Fact::RoleChangeRejected {
                reason: "the reviewer is not signed in".to_owned(),
            },
            Step::ApplyRoles { .. } => Fact::RolesReassigned,
            Step::HandOffAccount { step, .. } => self.hand_off(*step),
            Step::HandToScheduler { step, .. } => self.schedule(*step),
            Step::Cleanup { .. } => Fact::Cleaned { leftover: vec![] },
            Step::Wait { .. } => unreachable!("run() returns before executing a wait"),
        }
    }

    /// Another account takes the role, unless the script says nobody can.
    fn hand_off(&mut self, step: StepId) -> Fact {
        let outcome = self
            .script
            .handoffs
            .pop_front()
            .unwrap_or(HandoffOutcome::HandedOff);
        Fact::AccountHandoff { step, outcome }
    }

    /// The scheduler takes the step with a continuation of its own, or declines.
    fn schedule(&mut self, step: StepId) -> Fact {
        if let Some(Scheduler::Declines(why)) = self.script.scheduler.pop_front() {
            return Fact::ContinuationDeclined { step, why };
        }
        self.continuations += 1;
        let n = self.continuations;
        Fact::ContinuationScheduled {
            step,
            continuation: ContinuationId::new(format!("c{n}")),
            message: MessageId::new(format!("c{n}-message")),
            resumes_at: None,
        }
    }

    fn committed(&mut self, round: u32, added: &[AddedProbe]) -> Fact {
        let probe_runs = added
            .iter()
            .map(|probe| {
                self.script
                    .probe_runs
                    .get(&probe.from)
                    .cloned()
                    .unwrap_or_else(|| probe_run(&probe.from, true, true))
            })
            .collect();
        let files = if self.script.files.is_empty() {
            vec!["src/a.rs".to_owned()]
        } else {
            self.script.files.clone()
        };
        Fact::Committed {
            commit: sha(u64::from(round)),
            files,
            size: self.script.sizes.get(&round).copied().unwrap_or_default(),
            probe_runs,
        }
    }
}

fn engine_turn(view: &TaskView) -> Option<MessageId> {
    view.phase.waiting_message().cloned()
}

pub fn step_name(step: &Step) -> String {
    serde_json::to_value(step).expect("steps serialize")["kind"]
        .as_str()
        .expect("tagged")
        .to_owned()
}

pub fn passes() -> CommandResult {
    CommandResult::Exited {
        exit_code: 0,
        stdout: "ok".to_owned(),
        stderr: String::new(),
    }
}

pub fn fails() -> CommandResult {
    CommandResult::Exited {
        exit_code: 1,
        stdout: "FAIL".to_owned(),
        stderr: String::new(),
    }
}

pub fn times_out() -> CommandResult {
    CommandResult::TimedOut {
        output: "still running".to_owned(),
    }
}

pub fn refused() -> CommandResult {
    CommandResult::Refused {
        error: "write outside the roots".to_owned(),
    }
}

pub fn probe_run(from: &str, failed_before_fix: bool, passed_after_fix: bool) -> ProbeRun {
    ProbeRun {
        from: from.to_owned(),
        failed_before_fix,
        passed_after_fix,
        evidence: "assertion failed".to_owned(),
    }
}

// ---------------------------------------------------------------------------------------------
// Fixtures. They are the same small world every scenario starts from.
// ---------------------------------------------------------------------------------------------

pub fn sha(n: u64) -> Sha {
    Sha::parse(&format!("{n:040x}")).expect("40 hex digits")
}

pub fn fid(n: u32) -> FindingId {
    FindingId::new(n).expect("findings count from 1")
}

pub fn member(id: &str, provider: Provider) -> Member {
    Member {
        id: MemberId::new(id),
        provider,
    }
}

pub fn coder() -> MemberId {
    MemberId::new("codex-1")
}

pub fn checker() -> MemberId {
    MemberId::new("claude-1")
}

pub fn planner_id() -> MemberId {
    MemberId::new("claude-plan")
}

pub fn two_members() -> Bindings {
    Bindings::two(
        member("codex-1", Provider::Codex),
        member("claude-1", Provider::Claude),
    )
    .expect("distinct members")
}

pub fn three_members() -> Bindings {
    Bindings::three(
        member("claude-plan", Provider::Claude),
        member("codex-1", Provider::Codex),
        member("claude-1", Provider::Claude),
    )
    .expect("distinct members")
}

/// The same two members with their jobs swapped.
pub fn swapped_members() -> Bindings {
    Bindings::two(
        member("claude-1", Provider::Claude),
        member("codex-1", Provider::Codex),
    )
    .expect("distinct members")
}

pub const REQUEST: &str = "# Add a login page\n\nUse the existing session helper.";
pub const AGENTS_RULE: &str = "Every public function needs a test.";

/// The task every scenario starts from, changed field by field.
pub struct Task(TaskStarted);

impl Task {
    pub fn two() -> Task {
        Task::with(two_members())
    }

    pub fn three() -> Task {
        Task::with(three_members())
    }

    pub fn with(bindings: Bindings) -> Task {
        let title = task_title(REQUEST);
        let provider = bindings
            .member(Role::Implementer)
            .expect("always bound")
            .provider;
        Task(TaskStarted {
            request: REQUEST.to_owned(),
            constraints: Vec::new(),
            scope: Vec::new(),
            branch: branch_name(provider, &title, &[]),
            title,
            base: sha(0xba5e),
            rules: RulesSnapshot {
                agents_md: Some(AGENTS_RULE.to_owned()),
                claude_md: None,
                review_md: Some("Check the goal first.".to_owned()),
            },
            triggers: vec![Trigger::Code {
                pattern: "spawn".to_owned(),
            }],
            policy: TaskPolicy {
                plan_first: false,
                security_pass: false,
                probes: ProbePolicy::UserDecides,
                account_handoff: false,
                round_cap: RoundCap::default(),
                large_change: LargeChange::default(),
                permission: Permission::AskMe,
            },
            bindings,
        })
    }

    pub fn plan_first(mut self) -> Task {
        self.0.policy.plan_first = true;
        self
    }

    pub fn security_pass(mut self) -> Task {
        self.0.policy.security_pass = true;
        self
    }

    pub fn without_triggers(mut self) -> Task {
        self.0.triggers.clear();
        self
    }

    pub fn account_handoff(mut self) -> Task {
        self.0.policy.account_handoff = true;
        self
    }

    pub fn round_cap(mut self, rounds: u32) -> Task {
        self.0.policy.round_cap = RoundCap::new(rounds).expect("at least one round");
        self
    }

    pub fn scope(mut self, globs: &[&str]) -> Task {
        self.0.scope = globs.iter().map(|g| pattern(g)).collect();
        self
    }

    pub fn constraints(mut self, constraints: &[&str]) -> Task {
        self.0.constraints = constraints.iter().map(|c| (*c).to_owned()).collect();
        self
    }

    pub fn probes(mut self, probes: ProbePolicy) -> Task {
        self.0.policy.probes = probes;
        self
    }

    pub fn large_change(mut self, files: u32, lines: u32) -> Task {
        self.0.policy.large_change = LargeChange { files, lines };
        self
    }

    pub fn script(self, script: Script) -> ScriptedTask {
        ScriptedTask::start(self.0, script)
    }
}

pub fn pattern(glob: &str) -> ScopePattern {
    ScopePattern::new(glob).expect("a valid glob")
}

// ---------------------------------------------------------------------------------------------
// Scripted turn endings.
// ---------------------------------------------------------------------------------------------

pub fn reported(reported: Reported) -> Ended {
    Ended::Reported { reported }
}

/// A turn that ran its checks after its last edit.
pub fn checked() -> TurnChanges {
    TurnChanges {
        files: vec!["src/a.rs".to_owned()],
        edited_since_command: false,
        ran_command: true,
    }
}

pub fn edited_after_check() -> TurnChanges {
    TurnChanges {
        edited_since_command: true,
        ..checked()
    }
}

/// A turn that ran no command and edited nothing.
pub fn untouched() -> TurnChanges {
    TurnChanges {
        files: Vec::new(),
        edited_since_command: false,
        ran_command: false,
    }
}

pub fn done() -> ImplementerDone {
    ImplementerDone {
        summary: "Added the login page.".to_owned(),
        checks: vec![Check {
            command: "cargo test".to_owned(),
            exit_code: 0,
        }],
        responses: Vec::new(),
        added_probes: Vec::new(),
    }
}

pub fn fixed(finding: u32) -> Response {
    Response {
        finding: fid(finding),
        outcome: ResponseOutcome::Fixed,
    }
}

pub fn disputed(finding: u32, reason: &str) -> Response {
    Response {
        finding: fid(finding),
        outcome: ResponseOutcome::Disputed {
            reason: reason.to_owned(),
        },
    }
}

fn implementer_report(report: Report<ImplementerDone>, changes: TurnChanges) -> Ended {
    reported(Reported::Implementer { report, changes })
}

/// An implementer handoff with a clean report.
pub fn implements() -> Ended {
    implementer_report(Report::Finished { done: done() }, checked())
}

pub fn implements_answering(responses: Vec<Response>) -> Ended {
    let done = ImplementerDone {
        responses,
        ..done()
    };
    implementer_report(Report::Finished { done }, checked())
}

pub fn implements_with(done: ImplementerDone, changes: TurnChanges) -> Ended {
    implementer_report(Report::Finished { done }, changes)
}

/// A handoff whose turn left an edit that no command checked.
pub fn implements_unchecked() -> Ended {
    implementer_report(Report::Finished { done: done() }, edited_after_check())
}

/// A handoff that ran no command and so cannot clear an earlier unchecked edit.
pub fn implements_without_running() -> Ended {
    implementer_report(Report::Finished { done: done() }, untouched())
}

pub fn implementer_invalid(error: &str) -> Ended {
    implementer_report(
        Report::Invalid {
            error: error.to_owned(),
            raw: "{}".to_owned(),
        },
        checked(),
    )
}

pub fn implementer_invalid_after_edit(error: &str) -> Ended {
    implementer_report(
        Report::Invalid {
            error: error.to_owned(),
            raw: "{}".to_owned(),
        },
        edited_after_check(),
    )
}

pub fn implementer_blocked(summary: &str) -> Ended {
    implementer_report(
        Report::Blocked {
            summary: summary.to_owned(),
        },
        checked(),
    )
}

pub fn implementer_asks(question: &str) -> Ended {
    implementer_report(
        Report::Question {
            summary: "Half done.".to_owned(),
            question: question.to_owned(),
        },
        checked(),
    )
}

pub fn implementer_wants_plan_change(reason: &str, extra: &[&str]) -> Ended {
    implementer_report(
        Report::PlanChange {
            summary: "Half done.".to_owned(),
            request: PlanChangeRequest {
                reason: reason.to_owned(),
                extra_scope: extra.iter().map(|g| pattern(g)).collect(),
            },
        },
        checked(),
    )
}

pub fn lost() -> Ended {
    Ended::Lost {
        reason: "process exited".to_owned(),
    }
}

/// The provider stopped the turn at a usage limit after the message was delivered.
pub fn hits_usage_limit() -> Ended {
    Ended::QuotaLimit { delivered: true }
}

pub fn plan_ready() -> Ended {
    plan_with("src/login/**")
}

pub fn plan_with(scope: &str) -> Ended {
    reported(Reported::Plan {
        report: Report::Finished {
            done: TaskPlan {
                build: "A login page behind the existing session helper.".to_owned(),
                done_means: vec!["A signed-in user lands on the dashboard.".to_owned()],
                scope: vec![pattern(scope)],
            },
        },
    })
}

pub fn plan_question(question: &str) -> Ended {
    reported(Reported::Plan {
        report: Report::Question {
            summary: "Read the code.".to_owned(),
            question: question.to_owned(),
        },
    })
}

pub fn plan_invalid(error: &str) -> Ended {
    reported(Reported::Plan {
        report: Report::Invalid {
            error: error.to_owned(),
            raw: "{}".to_owned(),
        },
    })
}

pub fn planner_answers(answer: &str) -> Ended {
    reported(Reported::Answer {
        report: Report::Finished {
            done: AnswerDone {
                answer: answer.to_owned(),
            },
        },
    })
}

pub fn planner_asks_user(question: &str) -> Ended {
    reported(Reported::Answer {
        report: Report::Question {
            summary: "Read the code.".to_owned(),
            question: question.to_owned(),
        },
    })
}

pub fn planner_wants_plan_change(reason: &str) -> Ended {
    reported(Reported::Answer {
        report: Report::PlanChange {
            summary: "The answer contradicts the plan.".to_owned(),
            request: PlanChangeRequest {
                reason: reason.to_owned(),
                extra_scope: Vec::new(),
            },
        },
    })
}

pub fn planner_answer_invalid(error: &str) -> Ended {
    reported(Reported::Answer {
        report: Report::Invalid {
            error: error.to_owned(),
            raw: "{}".to_owned(),
        },
    })
}

pub fn planner_rules(rulings: &[(u32, RulingVerdict)]) -> Ended {
    reported(Reported::Rulings {
        report: Report::Finished {
            done: RulingsDone {
                rulings: rulings
                    .iter()
                    .map(|(finding, verdict)| Ruling {
                        finding: fid(*finding),
                        verdict: *verdict,
                        reason: "Because the evidence says so.".to_owned(),
                    })
                    .collect(),
            },
        },
    })
}

pub fn planner_ruling_question(question: &str) -> Ended {
    reported(Reported::Rulings {
        report: Report::Question {
            summary: "Read the code.".to_owned(),
            question: question.to_owned(),
        },
    })
}

fn review(verdict: Verdict, findings: Vec<FindingReport>, still_open: Vec<StillOpen>) -> Ended {
    reported(Reported::Review {
        report: Report::Finished {
            done: ReviewDone {
                verdict,
                summary: format!("{} findings.", findings.len()),
                findings,
                still_open,
            },
        },
    })
}

/// A reviewer who approves with nothing to report.
pub fn approves() -> Ended {
    review(Verdict::Approve, Vec::new(), Vec::new())
}

/// A reviewer who approves and still mentions findings that cannot block.
pub fn approves_listing(findings: Vec<FindingReport>) -> Ended {
    review(Verdict::Approve, findings, Vec::new())
}

pub fn requests_changes(findings: Vec<FindingReport>) -> Ended {
    review(Verdict::ChangesRequested, findings, Vec::new())
}

/// A reviewer who says an earlier finding is still open.
pub fn still_open(finding: u32, diagnosis: &str) -> Ended {
    let claim = StillOpen {
        finding: fid(finding),
        diagnosis: diagnosis.to_owned(),
    };
    review(Verdict::ChangesRequested, Vec::new(), vec![claim])
}

pub fn reviewer_invalid(error: &str) -> Ended {
    reported(Reported::Review {
        report: Report::Invalid {
            error: error.to_owned(),
            raw: "{}".to_owned(),
        },
    })
}

pub fn reviewer_blocked(summary: &str) -> Ended {
    reported(Reported::Review {
        report: Report::Blocked {
            summary: summary.to_owned(),
        },
    })
}

pub fn reviewer_asks(question: &str) -> Ended {
    reported(Reported::Review {
        report: Report::Question {
            summary: "Read the diff.".to_owned(),
            question: question.to_owned(),
        },
    })
}

// ---------------------------------------------------------------------------------------------
// Findings.
// ---------------------------------------------------------------------------------------------

fn finding(severity: Severity, kind: Kind, class: &str, evidence: &str) -> FindingReport {
    FindingReport {
        severity,
        kind,
        class: class.to_owned(),
        location: Location {
            path: "src/a.rs".to_owned(),
            line: 3,
        },
        problem: "Crashes when the session is missing.".to_owned(),
        evidence: evidence.to_owned(),
        fixed_when: format!("{evidence} passes."),
        probes: Vec::new(),
    }
}

/// An important bug whose evidence is `command`.
pub fn bug(class: &str, command: &str) -> FindingReport {
    finding(Severity::Important, Kind::Bug, class, command)
}

/// An important requirement whose evidence is a quote.
pub fn requirement(class: &str, quote: &str) -> FindingReport {
    finding(Severity::Important, Kind::Requirement, class, quote)
}

pub fn nit(class: &str) -> FindingReport {
    finding(Severity::Nit, Kind::Bug, class, "cmd-nit")
}

pub fn design(class: &str) -> FindingReport {
    finding(Severity::Important, Kind::Design, class, "cmd-design")
}

pub fn pre_existing(class: &str) -> FindingReport {
    finding(Severity::PreExisting, Kind::Bug, class, "cmd-old")
}

pub fn with_probes(mut report: FindingReport, probes: &[&str]) -> FindingReport {
    report.probes = probes.iter().map(|p| (*p).to_owned()).collect();
    report
}

pub fn script_commands(script: &mut Script, command: &str, results: Vec<CommandResult>) {
    script.commands.insert(command.to_owned(), results);
}

pub fn delivery(id: &str) -> DeliveryId {
    DeliveryId::new(id)
}
