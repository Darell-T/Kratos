//! The reviewer's probe tests (D78). A probe is a test file a reviewer wrote to show a bug. Only
//! the implementer adds one to the project, and only when the user chose it or delegated the
//! choice. This file decides which probes an implementer may add, what becomes of each one it
//! added, and what the record keeps of each one the reviewer left.

use crate::evidence::EvidenceCheck;
use crate::facts::{EvidenceChecked, ProbePolicy, ProbeRun};
use crate::fold::Rejection;
use crate::paths::RepoPath;
use crate::reports::{AddedProbe, ImplementerDone};
use crate::task_plan::{ProbeConstraints, Scope};
use crate::view::{Brief, Finding, Probe, ProbeRejection, ProbeStatus, TaskView, Work};

impl TaskView {
    /// `None` when the probes an implementer added are ones it may add, and name probes the
    /// reviewer left. Only the implementer adds a probe to the project, and only when the user
    /// chose it or delegated the choice (D78).
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

    pub(crate) fn offers_probe(&self, path: &str) -> bool {
        self.probes
            .iter()
            .any(|probe| probe.path == path && probe.status == ProbeStatus::Offered)
    }

    pub(crate) fn reject_offered_probes(&mut self, except: &[String], reason: ProbeRejection) {
        for probe in &mut self.probes {
            if probe.status == ProbeStatus::Offered && !except.contains(&probe.path) {
                probe.status = ProbeStatus::Rejected { reason };
            }
        }
    }

    /// Decides the fate of each probe the implementer added. Git showed whether it fails before the
    /// fix and passes after it. The constraints are the user's, and the rules apply them here, so
    /// the engine cannot keep a probe the user refused.
    pub(crate) fn apply_probe_runs(
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
            let Some(entry) = self.probes.iter_mut().find(|p| p.path == probe.from) else {
                return Err(Rejection::InvalidFact(format!(
                    "committed names {}, which is not a probe the reviewer left",
                    probe.from
                )));
            };
            entry.status = status;
        }
        Ok(())
    }

    fn judge_added_probe(&self, probe: &AddedProbe, run: &ProbeRun) -> ProbeStatus {
        let ProbePolicy::Delegated { constraints } = &self.policy.probes else {
            return Self::probe_by_run(probe, run);
        };
        match probe_copy_problem(constraints, &self.scope(), &probe.to) {
            Some(reason) => ProbeStatus::Rejected { reason },
            None => Self::probe_by_run(probe, run),
        }
    }

    fn probe_by_run(probe: &AddedProbe, run: &ProbeRun) -> ProbeStatus {
        let reason = if !run.failed_before_fix {
            ProbeRejection::PassedBeforeTheFix
        } else if !run.passed_after_fix {
            ProbeRejection::FailedAfterTheFix
        } else {
            return ProbeStatus::Retained {
                as_path: probe.to.as_str().to_owned(),
                evidence: run.evidence.clone(),
            };
        };
        ProbeStatus::Rejected { reason }
    }

    /// Probes nobody added are rejected at approval when the user delegated the choice, so the
    /// approval always lists what was kept and what was not.
    pub(crate) fn reject_unadded_probes(&mut self) {
        if matches!(self.policy.probes, ProbePolicy::Delegated { .. }) {
            self.reject_offered_probes(&[], ProbeRejection::NotAdded);
        }
    }

    pub(crate) fn merge_probes(&mut self, checked: &EvidenceChecked, added: &[Finding]) {
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
}

/// Whether the implementer may keep a probe at `to`: under one of the user's test paths and inside
/// the agreed scope. A pure path rule, so the engine cannot apply it more loosely than the user
/// wrote it. The git half of the check, that the probe fails before the fix and passes after, is
/// the engine's.
fn probe_copy_problem(
    constraints: &ProbeConstraints,
    scope: &Scope,
    to: &RepoPath,
) -> Option<ProbeRejection> {
    let allowed = constraints
        .test_paths
        .iter()
        .any(|pattern| pattern.covers(to.as_str()));
    if !allowed {
        return Some(ProbeRejection::OutsideAllowedPaths);
    }
    (!scope.covers(to.as_str())).then_some(ProbeRejection::OutsideScope)
}
