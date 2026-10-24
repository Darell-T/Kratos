//! The review loop: committing a handoff, checking a review's evidence, settling findings, rulings on
//! disputes, and the pauses and approvals that end a round.
//!
//! A round is one implementer handoff plus the review of it. Rounds count from the user's last explicit
//! request to go on (D89), and a blocking finding gets two fix attempts before the task pauses as stuck.

use std::collections::BTreeMap;

use crate::evidence::{EvidenceCheck, Rerun, RerunVerdict};
use crate::facts::{ChangeSize, Depth, EvidenceChecked, FactSeq, ProbePolicy, ProbeRun, Sha};
use crate::fold::Rejection;
use crate::reports::{
    AddedProbe, FindingId, FindingReport, ImplementerDone, Kind, Response, ResponseOutcome,
    ReviewDone, RulingVerdict, RulingsDone, Severity, Verdict,
};
use crate::security_triggers::MAX_HITS;
use crate::task_plan::{CopyProblem, probe_copy_problem};
use crate::view::{
    Activity, AfterCommit, Answerer, ApprovedBy, Brief, ClosedHow, FIX_ATTEMPTS, Finding,
    FindingStatus, FindingsPause, ListedWhy, Note, Pass, PassKind, Phase, Probe, ProbeRejection,
    ProbeStatus, Retry, Review, ReviewJob, Round, RulingRecord, TaskView, Turn, Waiting, Work,
};

/// A report of the reviewer, taken out of the view for checking.
struct Checking {
    turn: Turn,
    job: ReviewJob,
    report: ReviewDone,
}

/// Every finding after a review, and which of them block.
struct Judged {
    /// Every finding, with this review's findings added and the earlier open ones settled.
    findings: Vec<Finding>,
    /// The findings this review spoke to, which are the earlier open ones and its new ones.
    reviewed: Vec<Finding>,
    /// Only the findings this review reported.
    added: Vec<Finding>,
    blocking: Vec<FindingId>,
}

impl TaskView {
    // -----------------------------------------------------------------------------------------
    // The handoff check.
    // -----------------------------------------------------------------------------------------

    /// `None` when the responses answer every open finding exactly once, and each dispute has a reason.
    pub(crate) fn responses_problem(&self, responses: &[Response]) -> Option<String> {
        let answers = |finding: &Finding| {
            let id = finding.id;
            responses.iter().filter(move |r| r.finding == id)
        };
        let open: Vec<&Finding> = self.open_findings().collect();
        let ids_where = |wanted: &dyn Fn(&Finding) -> bool| -> String {
            let ids: Vec<String> = open
                .iter()
                .filter(|finding| wanted(finding))
                .map(|finding| finding.id.to_string())
                .collect();
            ids.join(", ")
        };
        let is_blank_dispute = |response: &Response| matches!(&response.outcome, ResponseOutcome::Disputed { reason } if reason.trim().is_empty());
        let missing = ids_where(&|f| answers(f).count() == 0);
        let repeated = ids_where(&|f| answers(f).count() > 1);
        let unexplained = ids_where(&|f| answers(f).any(is_blank_dispute));
        let problems: Vec<String> = [
            (!missing.is_empty())
                .then(|| format!("Answer every open finding in responses. Missing: {missing}.")),
            (!repeated.is_empty())
                .then(|| format!("Answer each finding once. Answered more than once: {repeated}.")),
            (!unexplained.is_empty())
                .then(|| format!("Give a reason for each dispute: {unexplained}.")),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!problems.is_empty()).then(|| problems.join(" "))
    }

    /// `None` when the probes an implementer added are ones it may add, and name probes the reviewer
    /// left. Only the implementer adds a probe to the project, and only when the user chose it or
    /// delegated the choice (D78).
    pub(crate) fn probes_problem(&self, work: &Work, done: &ImplementerDone) -> Option<String> {
        let picks = match work {
            Work::Implement {
                brief: Brief::KeepProbes { paths },
            } => Some(paths),
            _ => None,
        };
        let delegated = matches!(self.policy.probes, ProbePolicy::Delegated { .. });
        let added: Vec<&str> = done.added_probes.iter().map(|p| p.from.as_str()).collect();
        if picks.is_none() && !delegated {
            return (!added.is_empty())
                .then(|| "added_probes: you may not add probes now. Leave it empty.".to_owned());
        }
        if let Some(unknown) = added.iter().find(|from| !self.offers_probe(from)) {
            return Some(format!(
                "added_probes: {unknown} is not a probe the reviewer left."
            ));
        }
        let missing: Vec<&str> = picks
            .into_iter()
            .flatten()
            .map(String::as_str)
            .filter(|path| !added.contains(path))
            .collect();
        (!missing.is_empty()).then(|| {
            format!(
                "added_probes: add each probe the user chose. Missing: {}.",
                missing.join(", ")
            )
        })
    }

    fn offers_probe(&self, path: &str) -> bool {
        self.probes
            .iter()
            .any(|probe| probe.path == path && probe.status == ProbeStatus::Offered)
    }

    // -----------------------------------------------------------------------------------------
    // Commits.
    // -----------------------------------------------------------------------------------------

    /// Records the round. After a handoff, each open finding gains an attempt and the response the
    /// implementer gave it. A dispute goes to whoever rules, and otherwise the review begins.
    pub(crate) fn on_committed(
        self: Box<Self>,
        seq: FactSeq,
        commit: Sha,
        files: Vec<String>,
        size: ChangeSize,
        probe_runs: Vec<ProbeRun>,
    ) -> Result<Box<TaskView>, Rejection> {
        let (mut view, then) = self.take_committing()?;
        let report = match then {
            AfterCommit::Review { report } => Some(report),
            AfterCommit::Accept => None,
        };
        if let Some(report) = &report {
            view.apply_probe_runs(&report.added_probes, &probe_runs)?;
        }
        view.changed_files.extend(files.iter().cloned());
        view.rounds.push(Round {
            n: u32::try_from(view.rounds.len() + 1).unwrap_or(u32::MAX),
            commit: commit.clone(),
            report,
            files,
            size,
            reviews: Vec::new(),
        });
        view.dirty = false;
        let Some(report) = view.rounds.last().and_then(|round| round.report.as_ref()) else {
            return Ok(view.enter_approved(commit, ApprovedBy::User));
        };
        record_attempts(&mut view.findings, report);
        view.after_handoff(seq, commit)
    }

    fn take_committing(mut self: Box<Self>) -> Result<(Box<TaskView>, AfterCommit), Rejection> {
        let phase = std::mem::replace(
            &mut self.phase,
            Phase::Closed {
                outcome: crate::facts::Outcome::Cancelled,
            },
        );
        match phase {
            Phase::Active {
                activity: Activity::Committing { then, .. },
            } => Ok((self, then)),
            other => {
                self.phase = other;
                Err(self.misfit("committed"))
            }
        }
    }

    /// A dispute goes to the planner, or to the user when there is no planner. With no dispute, the
    /// review of the commit begins.
    fn after_handoff(
        self: Box<Self>,
        seq: FactSeq,
        commit: Sha,
    ) -> Result<Box<TaskView>, Rejection> {
        let mut disputed: Vec<&Finding> = self
            .open_findings()
            .filter(|f| {
                matches!(
                    f.status,
                    FindingStatus::Open {
                        response: Some(ResponseOutcome::Disputed { .. }),
                        ..
                    }
                )
            })
            .collect();
        disputed.sort_by_key(|f| f.rank());
        let disputed: Vec<FindingId> = disputed.iter().map(|f| f.id).collect();
        if disputed.is_empty() {
            return self.begin_review(seq, commit);
        }
        if self.bindings.has_planner() {
            return self.begin_step(Work::Rule { disputed, commit }, seq);
        }
        Ok(self.waiting(Waiting::Ruling { disputed, commit }))
    }

    /// The standard review of `commit`. Once a handoff requires deeper review, its unfinished passes
    /// belong to the task even when later fixes are small (D94).
    pub(crate) fn begin_review(
        self: Box<Self>,
        seq: FactSeq,
        commit: Sha,
    ) -> Result<Box<TaskView>, Rejection> {
        let thorough = self
            .rounds
            .iter()
            .any(|round| self.policy.large_change.depth_of(round.size) == Depth::Thorough);
        let job = ReviewJob {
            pass: Pass::Standard,
            commit,
            diff_base: self
                .last_reviewed
                .clone()
                .unwrap_or_else(|| self.base.clone()),
            depth: if thorough {
                Depth::Thorough
            } else {
                Depth::Single
            },
        };
        self.begin_step(Work::Review { job }, seq)
    }

    // -----------------------------------------------------------------------------------------
    // Probes.
    // -----------------------------------------------------------------------------------------

    /// Decides the fate of each probe the implementer added. Git showed whether it fails before the fix
    /// and passes after it. The constraints are the user's, and the rules apply them here, so the
    /// engine cannot keep a probe the user refused.
    fn apply_probe_runs(
        &mut self,
        added: &[AddedProbe],
        runs: &[ProbeRun],
    ) -> Result<(), Rejection> {
        for probe in added {
            let Some(run) = runs.iter().find(|run| run.from == probe.from) else {
                return Err(Rejection::InvalidFact(format!(
                    "committed has no probe run for {}",
                    probe.from
                )));
            };
            let status = self.judge_added_probe(probe, run);
            if let Some(entry) = self.probes.iter_mut().find(|p| p.path == probe.from) {
                entry.status = status;
            }
        }
        Ok(())
    }

    fn judge_added_probe(&self, probe: &AddedProbe, run: &ProbeRun) -> ProbeStatus {
        let ProbePolicy::Delegated { constraints } = &self.policy.probes else {
            return Self::probe_by_run(probe, run);
        };
        match probe_copy_problem(constraints, &self.scope(), &probe.to) {
            Some(CopyProblem::OutsideAllowedPaths) => ProbeStatus::Rejected {
                reason: ProbeRejection::OutsideAllowedPaths,
            },
            Some(CopyProblem::OutsideScope) => ProbeStatus::Rejected {
                reason: ProbeRejection::OutsideScope,
            },
            None => Self::probe_by_run(probe, run),
        }
    }

    /// A kept probe must fail before the fix and pass after it.
    fn probe_by_run(probe: &AddedProbe, run: &ProbeRun) -> ProbeStatus {
        let reason = if !run.failed_before_fix {
            ProbeRejection::PassedBeforeTheFix
        } else if !run.passed_after_fix {
            ProbeRejection::FailedAfterTheFix
        } else {
            return ProbeStatus::Retained {
                as_path: probe.to.clone(),
                evidence: run.evidence.clone(),
            };
        };
        ProbeStatus::Rejected { reason }
    }

    /// Approval covers one commit. Probes nobody added are rejected when the user delegated the choice,
    /// so the approval always lists what was kept and what was not.
    pub(crate) fn enter_approved(
        mut self: Box<Self>,
        commit: Sha,
        by: ApprovedBy,
    ) -> Box<TaskView> {
        if matches!(self.policy.probes, ProbePolicy::Delegated { .. }) {
            for probe in &mut self.probes {
                if probe.status == ProbeStatus::Offered {
                    probe.status = ProbeStatus::Rejected {
                        reason: ProbeRejection::NotAdded,
                    };
                }
            }
        }
        self.waiting(Waiting::Approved { commit, by })
    }

    fn merge_probes(&mut self, checked: &EvidenceChecked, added: &[Finding]) {
        for capture in &checked.probes {
            let finding = added
                .iter()
                .find(|f| f.report.probes.contains(&capture.path));
            let showed = finding.and_then(|f| match checked.checks.get(&f.id) {
                Some(EvidenceCheck::Confirmed { output, .. }) => output.clone(),
                _ => None,
            });
            let entry = Probe {
                path: capture.path.clone(),
                content: capture.content.clone(),
                truncated: capture.truncated,
                finding: finding.map(|f| f.id),
                showed,
                status: ProbeStatus::Offered,
            };
            match self.probes.iter_mut().find(|p| p.path == capture.path) {
                Some(existing) => {
                    existing.content = entry.content;
                    existing.truncated = entry.truncated;
                    existing.finding = existing.finding.or(entry.finding);
                    existing.showed = existing.showed.take().or(entry.showed);
                }
                None => self.probes.push(entry),
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Rulings on disputes (D31).
    // -----------------------------------------------------------------------------------------

    /// The planner ruled. A ruling must cover every disputed finding exactly once, and a ruling that
    /// does not goes back once and then pauses with the user able to rule instead.
    pub(crate) fn on_rulings_done(
        mut self: Box<Self>,
        seq: FactSeq,
        turn: Turn,
        done: RulingsDone,
    ) -> Result<Box<TaskView>, Rejection> {
        let Work::Rule { disputed, commit } = turn.work.clone() else {
            return Err(self.misfit("rulings"));
        };
        if let Some(error) = rulings_problem(&disputed, &done) {
            return Ok(self.retry_turn(
                turn,
                Retry::InvalidReport,
                Note::InvalidReport { error },
                seq,
            ));
        }
        let mut not_bug = Vec::new();
        for ruling in done.rulings {
            if ruling.verdict == RulingVerdict::NotBug {
                not_bug.push(ruling.finding);
            }
            self.rulings.push(RulingRecord {
                finding: ruling.finding,
                verdict: ruling.verdict,
                reason: ruling.reason,
                by: Answerer::Planner,
            });
        }
        self.apply_rulings(&not_bug, &disputed, commit, seq)
    }

    /// A ruling closes the findings ruled not a bug. The rest go back to the implementer with the ruling.
    /// With none left, the review goes on at the same commit with no extra implementer step.
    pub(crate) fn apply_rulings(
        mut self: Box<Self>,
        not_bug: &[FindingId],
        disputed: &[FindingId],
        commit: Sha,
        seq: FactSeq,
    ) -> Result<Box<TaskView>, Rejection> {
        for finding in &mut self.findings {
            if not_bug.contains(&finding.id) {
                finding.status = FindingStatus::Closed {
                    how: ClosedHow::RuledNotBug,
                };
            }
        }
        let bugs: Vec<FindingId> = disputed
            .iter()
            .filter(|id| !not_bug.contains(id))
            .copied()
            .collect();
        if bugs.is_empty() {
            return self.begin_review(seq, commit);
        }
        self.begin_step(
            Work::Implement {
                brief: Brief::Rulings { bugs },
            },
            seq,
        )
    }

    // -----------------------------------------------------------------------------------------
    // Checking a review.
    // -----------------------------------------------------------------------------------------

    fn take_checking(mut self: Box<Self>) -> Result<(Box<TaskView>, Checking), Rejection> {
        let phase = std::mem::replace(
            &mut self.phase,
            Phase::Closed {
                outcome: crate::facts::Outcome::Cancelled,
            },
        );
        match phase {
            Phase::Active {
                activity:
                    Activity::Checking {
                        turn, job, report, ..
                    },
            } => Ok((self, Checking { turn, job, report })),
            other => {
                self.phase = other;
                Err(self.misfit("evidence_checked"))
            }
        }
    }

    /// Triages the review's new findings, settles the earlier ones by Kratos's reruns, and then decides
    /// what follows. A verdict that disagrees with the confirmed findings goes back once on the same
    /// step and then pauses, and the report's findings are not added.
    pub(crate) fn on_evidence_checked(
        self: Box<Self>,
        seq: FactSeq,
        checked: EvidenceChecked,
    ) -> Result<Box<TaskView>, Rejection> {
        let (mut view, Checking { turn, job, report }) = self.take_checking()?;
        let judged = view.judge(&job, &report, &checked)?;
        let accepted = agrees(report.verdict, judged.blocking.len());
        view.record_review(&job, &report, &checked, accepted);
        if !accepted {
            view.merge_probes(&checked, &[]);
            let note = Note::VerdictMismatch {
                verdict: report.verdict,
                findings: judged.reviewed,
            };
            return Ok(view.retry_turn(turn, Retry::VerdictMismatch, note, seq));
        }
        view.merge_probes(&checked, &judged.added);
        view.findings = judged.findings;
        if job.pass.kind() == PassKind::Standard {
            view.last_reviewed = Some(job.commit.clone());
        }
        if judged.blocking.is_empty() {
            view.after_approval(seq, &job)
        } else {
            view.after_blocking(seq, &job, &judged.blocking)
        }
    }

    fn judge(
        &self,
        job: &ReviewJob,
        report: &ReviewDone,
        checked: &EvidenceChecked,
    ) -> Result<Judged, Rejection> {
        let round = u32::try_from(self.rounds.len()).unwrap_or(u32::MAX);
        let mut added = Vec::new();
        for (index, finding) in report.findings.iter().enumerate() {
            let id = FindingId::nth(self.findings.len() + index);
            added.push(Finding {
                id,
                round,
                pass: job.pass.kind(),
                report: finding.clone(),
                status: triage(id, finding, checked.checks.get(&id))?,
            });
        }
        let settled: Vec<Finding> = self
            .open_findings()
            .map(|finding| Finding {
                status: settle(finding, report, checked.rechecks.get(&finding.id)),
                ..finding.clone()
            })
            .collect();
        let mut findings: Vec<Finding> = self
            .findings
            .iter()
            .map(|finding| {
                settled
                    .iter()
                    .find(|s| s.id == finding.id)
                    .unwrap_or(finding)
                    .clone()
            })
            .collect();
        findings.extend(added.iter().cloned());
        let blocking = findings
            .iter()
            .filter(|f| f.is_open())
            .map(|f| f.id)
            .collect();
        let reviewed = settled.into_iter().chain(added.iter().cloned()).collect();
        Ok(Judged {
            findings,
            reviewed,
            added,
            blocking,
        })
    }

    fn record_review(
        &mut self,
        job: &ReviewJob,
        report: &ReviewDone,
        checked: &EvidenceChecked,
        accepted: bool,
    ) {
        let Some(last) = self.rounds.last_mut() else {
            return;
        };
        last.reviews.push(Review {
            pass: job.pass.kind(),
            report: report.clone(),
            accepted,
            checks: checked.checks.clone(),
            rechecks: checked.rechecks.clone(),
            hits: checked.security_hits.clone(),
        });
    }

    fn finish_pass(&mut self, job: &ReviewJob) {
        if job.pass.kind() != PassKind::Standard {
            self.passes_done.insert(job.pass.kind());
        }
    }

    /// A blocking finding that survived its fix attempts pauses the task as stuck. Otherwise the cap
    /// pauses it, or another round starts.
    fn after_blocking(
        mut self: Box<Self>,
        seq: FactSeq,
        job: &ReviewJob,
        blocking: &[FindingId],
    ) -> Result<Box<TaskView>, Rejection> {
        self.finish_pass(job);
        let ids = |select: &dyn Fn(&Finding) -> bool| -> Vec<FindingId> {
            let mut chosen: Vec<&Finding> = self
                .findings
                .iter()
                .filter(|f| blocking.contains(&f.id) && select(f))
                .collect();
            chosen.sort_by_key(|f| f.rank());
            chosen.iter().map(|f| f.id).collect()
        };
        let stuck = ids(
            &|f| matches!(f.status, FindingStatus::Open { attempts, .. } if attempts >= FIX_ATTEMPTS),
        );
        if !stuck.is_empty() {
            let why = FindingsPause::Stuck;
            return Ok(self.waiting(Waiting::Findings { why, ids: stuck }));
        }
        if self.rounds_left() == 0 {
            let why = FindingsPause::Cap;
            let ids = ids(&|_| true);
            return Ok(self.waiting(Waiting::Findings { why, ids }));
        }
        self.begin_step(
            Work::Implement {
                brief: Brief::Findings,
            },
            seq,
        )
    }

    /// The next pass in the round, or the reviewer's approval (D94).
    fn after_approval(
        mut self: Box<Self>,
        seq: FactSeq,
        job: &ReviewJob,
    ) -> Result<Box<TaskView>, Rejection> {
        self.finish_pass(job);
        let Some(pass) = self.next_pass(job) else {
            return Ok(self.enter_approved(job.commit.clone(), ApprovedBy::Reviewer));
        };
        let next = ReviewJob {
            pass,
            commit: job.commit.clone(),
            diff_base: self.base.clone(),
            depth: job.depth,
        };
        self.begin_step(Work::Review { job: next }, seq)
    }

    /// Security comes after the standard pass, then regressions. A small change runs security only when a
    /// trigger matched. A large change runs it whenever the Team runs the security pass (D94).
    fn next_pass(&self, job: &ReviewJob) -> Option<Pass> {
        let thorough = job.depth == Depth::Thorough;
        let security = || {
            if !self.scans_for_security() {
                return None;
            }
            // A blocking review can defer security, but a later empty scan cannot cancel it.
            let mut hits = Vec::new();
            for hit in self
                .rounds
                .iter()
                .flat_map(|round| &round.reviews)
                .filter(|review| review.pass == PassKind::Standard)
                .flat_map(|review| review.hits.iter().flatten())
            {
                if !hits.contains(hit) {
                    hits.push(hit.clone());
                    if hits.len() == MAX_HITS {
                        break;
                    }
                }
            }
            (thorough || !hits.is_empty()).then_some(Pass::Security { hits })
        };
        let regressions = || {
            (thorough && !self.passes_done.contains(&PassKind::Regressions))
                .then_some(Pass::Regressions)
        };
        match job.pass.kind() {
            PassKind::Standard => security().or_else(regressions),
            PassKind::Security => regressions(),
            PassKind::Regressions => None,
        }
    }

    /// The one place that decides whether the security pass still runs.
    pub(crate) fn scans_for_security(&self) -> bool {
        self.policy.security_pass
            && !self.passes_done.contains(&PassKind::Security)
            && !self.triggers.is_empty()
    }
}

/// Each open finding takes the response the implementer gave it. A fix is an attempt, and a dispute
/// is not, because the implementer has not tried to fix anything until the ruling says it must.
fn record_attempts(findings: &mut [Finding], report: &ImplementerDone) {
    for finding in findings {
        let FindingStatus::Open {
            attempts, response, ..
        } = &mut finding.status
        else {
            continue;
        };
        *response = report
            .responses
            .iter()
            .find(|r| r.finding == finding.id)
            .map(|r| r.outcome.clone());
        if matches!(response, Some(ResponseOutcome::Fixed)) {
            *attempts += 1;
        }
    }
}

fn agrees(verdict: Verdict, blocking: usize) -> bool {
    match verdict {
        Verdict::Approve => blocking == 0,
        Verdict::ChangesRequested => blocking > 0,
    }
}

/// Only an important finding that is not a design opinion can block (HANDOFF, evidence rules).
fn triage(
    id: FindingId,
    report: &FindingReport,
    check: Option<&EvidenceCheck>,
) -> Result<FindingStatus, Rejection> {
    let listed = |why| Ok(FindingStatus::Listed { why });
    match (report.severity, report.kind) {
        (Severity::PreExisting, _) => return listed(ListedWhy::PreExisting),
        (Severity::Nit, _) => return listed(ListedWhy::Nit),
        (Severity::Important, Kind::Design) => return listed(ListedWhy::Design),
        (Severity::Important, _) => {}
    }
    match check {
        None => Err(Rejection::InvalidFact(format!(
            "finding {id} can block, so evidence_checked must carry its check"
        ))),
        Some(EvidenceCheck::Dropped { reason }) => Ok(FindingStatus::Dropped {
            reason: reason.clone(),
        }),
        Some(EvidenceCheck::Confirmed {
            inside_new_diff: false,
            ..
        }) => listed(ListedWhy::OutsideDiff),
        Some(EvidenceCheck::Confirmed { .. }) => Ok(FindingStatus::Open {
            attempts: 0,
            response: None,
            diagnosis: None,
        }),
    }
}

/// An open bug's rerun decides it. Only a rerun that could not run falls back to the reviewer's word.
fn settle(finding: &Finding, report: &ReviewDone, rerun: Option<&Rerun>) -> FindingStatus {
    let claim = report
        .still_open
        .iter()
        .find(|entry| entry.finding == finding.id);
    let FindingStatus::Open {
        attempts, response, ..
    } = &finding.status
    else {
        return finding.status.clone();
    };
    let still_open = FindingStatus::Open {
        attempts: *attempts,
        response: response.clone(),
        diagnosis: claim.map(|entry| entry.diagnosis.clone()),
    };
    let fixed = FindingStatus::Closed {
        how: ClosedHow::Fixed,
    };
    let decisive = rerun.filter(|rerun| {
        finding.report.kind == Kind::Bug && rerun.verdict != RerunVerdict::Inconclusive
    });
    match decisive {
        Some(rerun) if rerun.verdict == RerunVerdict::Fails => still_open,
        Some(_) => fixed,
        None if claim.is_some() => still_open,
        None => fixed,
    }
}

/// `None` when the rulings cover every disputed finding exactly once and give a reason for each.
fn rulings_problem(disputed: &[FindingId], done: &RulingsDone) -> Option<String> {
    let mut counts: BTreeMap<FindingId, usize> = BTreeMap::new();
    for ruling in &done.rulings {
        *counts.entry(ruling.finding).or_default() += 1;
    }
    let ids = |select: &dyn Fn(&FindingId) -> bool| -> Vec<String> {
        disputed
            .iter()
            .filter(|id| select(id))
            .map(ToString::to_string)
            .collect()
    };
    let missing = ids(&|id| !counts.contains_key(id));
    let repeated = ids(&|id| counts.get(id).is_some_and(|n| *n > 1));
    let strangers: Vec<String> = counts
        .keys()
        .filter(|id| !disputed.contains(id))
        .map(ToString::to_string)
        .collect();
    let problems: Vec<String> = [
        (!missing.is_empty()).then(|| {
            format!(
                "Rule on every disputed finding. Missing: {}.",
                missing.join(", ")
            )
        }),
        (!repeated.is_empty()).then(|| {
            format!(
                "Rule on each finding once. Ruled more than once: {}.",
                repeated.join(", ")
            )
        }),
        (!strangers.is_empty()).then(|| {
            format!(
                "Rule only on the disputed findings. Not disputed: {}.",
                strangers.join(", ")
            )
        }),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!problems.is_empty()).then(|| problems.join(" "))
}
