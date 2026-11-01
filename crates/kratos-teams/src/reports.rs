//! What agents hand back at the end of a turn, and the boundary that turns it into typed facts.
//!
//! Two shapes exist on purpose.
//!
//! - The wire shape is a flat struct for each kind of report. Both CLIs force it through a JSON Schema
//!   that schemars derives from the struct, and the schema has to stay inside the subset both accept:
//!   every property required, `additionalProperties: false`, and no `anyOf`, `oneOf`, `allOf`, `$ref`,
//!   `minLength`, `maxLength`, or `pattern`. So no field is optional or null. "No value" is an empty
//!   string or an empty array.
//! - The domain shape is what the rest of the crate sees. A report that asks a question is a different
//!   variant from one that finished, so no code can read a question from a report that did not ask one.
//!
//! `parse_report` is the only function that crosses from one to the other. It never fails. A reply that
//! cannot be used becomes `Report::Invalid`, whose text tells the agent what to fix.

use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU32;
use std::str::FromStr;
use std::sync::LazyLock;

use kratos_protocol::TurnChanges;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::task_plan::{PlanChangeRequest, ScopePattern, TaskPlan};

// ---------------------------------------------------------------------------------------------
// Findings.
// ---------------------------------------------------------------------------------------------

/// `F1`, `F2`, and so on. Kratos assigns them from the record, so "the same finding" is a fact Kratos
/// can check and not something a reviewer asserts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FindingId(NonZeroU32);

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a finding id like F1")]
pub struct FindingIdError(pub String);

impl FindingId {
    /// Findings count from 1.
    pub fn new(number: u32) -> Option<FindingId> {
        NonZeroU32::new(number).map(FindingId)
    }

    pub fn number(self) -> u32 {
        self.0.get()
    }

    /// The id of the finding at zero-based `position` in the record.
    pub fn nth(position: usize) -> FindingId {
        let number = u32::try_from(position.saturating_add(1)).unwrap_or(u32::MAX);
        FindingId(NonZeroU32::new(number).unwrap_or(NonZeroU32::MAX))
    }
}

impl FromStr for FindingId {
    type Err = FindingIdError;

    fn from_str(text: &str) -> Result<FindingId, FindingIdError> {
        let digits = text.strip_prefix('F').filter(|digits| {
            digits.bytes().all(|byte| byte.is_ascii_digit()) && !digits.starts_with('0')
        });
        digits
            .and_then(|digits| digits.parse().ok())
            .and_then(FindingId::new)
            .ok_or_else(|| FindingIdError(text.to_owned()))
    }
}

impl fmt::Display for FindingId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "F{}", self.0)
    }
}

impl Serialize for FindingId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for FindingId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<FindingId, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

// Variants have no doc comments here on purpose. schemars turns a documented variant into a `oneOf`,
// which the providers' strict mode refuses. The field that holds the enum explains it instead.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Important,
    PreExisting,
    Nit,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Bug,
    Constraint,
    Requirement,
    Design,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Location {
    /// Relative to the repository root.
    pub path: String,
    /// The line number, counting from 1.
    pub line: u32,
}

/// One new finding, in the spec's format.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindingReport {
    /// important blocks the work. nit is a small point. pre_existing means the problem was there
    /// before this task.
    pub severity: Severity,
    /// bug: a command shows it. constraint and requirement: the task or the rules say so. design: an
    /// opinion.
    pub kind: Kind,
    /// A short name for the kind of mistake, so every instance of it gets fixed.
    pub class: String,
    pub location: Location,
    /// What goes wrong, and for whom.
    pub problem: String,
    /// For a bug, one shell command that exits non-zero on the reviewed commit. For a requirement or
    /// constraint, a quote of at least 12 characters from the task, the approved plan, the user's
    /// directions, AGENTS.md, CLAUDE.md, or REVIEW.md.
    pub evidence: String,
    /// The condition that shows the finding is fixed.
    pub fixed_when: String,
    /// Paths of the probe test files that show this finding, or an empty list. Kratos copies them into
    /// the record, and the approval report lists each with the output that showed the bug (D78).
    pub probes: Vec<String>,
}

impl FindingReport {
    /// Only an important finding that is not a design opinion can block the work.
    pub fn can_block(&self) -> bool {
        self.severity == Severity::Important && self.kind != Kind::Design
    }
}

// ---------------------------------------------------------------------------------------------
// Finished reports, one per kind of work.
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub command: String,
    pub exit_code: i32,
}

/// A probe the implementer added to the project. The reviewer's file `from` now lives at `to`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddedProbe {
    pub from: String,
    pub to: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResponseOutcome {
    Fixed,
    /// A dispute goes to whoever rules, the planner or the user, and never to the reviewer. The reason
    /// may be blank here. The handoff check refuses a blank one with a message that names every
    /// unexplained finding at once.
    Disputed {
        reason: String,
    },
}

/// The implementer's answer to one open finding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub finding: FindingId,
    pub outcome: ResponseOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImplementerDone {
    pub summary: String,
    pub checks: Vec<Check>,
    /// One entry for each open finding Kratos sent. The handoff check enforces exactly one each.
    pub responses: Vec<Response>,
    pub added_probes: Vec<AddedProbe>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    ChangesRequested,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Approve => "approve",
            Verdict::ChangesRequested => "changes_requested",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StillOpen {
    pub finding: FindingId,
    /// What is still wrong, the evidence, and why the earlier fix did not work.
    pub diagnosis: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewDone {
    pub verdict: Verdict,
    pub summary: String,
    /// New findings only.
    pub findings: Vec<FindingReport>,
    pub still_open: Vec<StillOpen>,
}

/// The planner's answer to a question from the implementer or the reviewer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnswerDone {
    pub answer: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RulingVerdict {
    Bug,
    NotBug,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ruling {
    pub finding: FindingId,
    pub verdict: RulingVerdict,
    pub reason: String,
}

/// The planner's rulings on disputed findings. The fold checks that they cover every disputed finding
/// exactly once, because only the fold knows which findings were disputed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulingsDone {
    pub rulings: Vec<Ruling>,
}

// ---------------------------------------------------------------------------------------------
// The typed outcome of any turn's final reply.
// ---------------------------------------------------------------------------------------------

/// How a member's final reply parsed. Every kind of work shares these five outcomes, so the question,
/// blocked, plan-change, and invalid-report handling is written once, in the fold.
///
/// A question is not a blocked report. It suspends the step and gets answered. A blocked report means
/// something outside the member's control stops it, and only the user can help.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Report<T> {
    Finished {
        done: T,
    },
    Blocked {
        summary: String,
    },
    Question {
        summary: String,
        question: String,
    },
    PlanChange {
        summary: String,
        request: PlanChangeRequest,
    },
    /// The reply could not be used. `error` is written for the agent, and `raw` keeps what it sent.
    Invalid {
        error: String,
        raw: String,
    },
}

/// A parsed report with the kind of work it answers. Only implementer reports carry the turn's file
/// changes, because those feed the unchecked-edit rule.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reported {
    Plan {
        report: Report<TaskPlan>,
    },
    Implementer {
        report: Report<ImplementerDone>,
        changes: TurnChanges,
    },
    Review {
        report: Report<ReviewDone>,
    },
    Answer {
        report: Report<AnswerDone>,
    },
    Rulings {
        report: Report<RulingsDone>,
    },
}

/// Which report a piece of work must end with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportKind {
    Plan,
    Implementer,
    Review,
    Answer,
    Rulings,
}

impl Reported {
    pub fn kind(&self) -> ReportKind {
        match self {
            Reported::Plan { .. } => ReportKind::Plan,
            Reported::Implementer { .. } => ReportKind::Implementer,
            Reported::Review { .. } => ReportKind::Review,
            Reported::Answer { .. } => ReportKind::Answer,
            Reported::Rulings { .. } => ReportKind::Rulings,
        }
    }
}

/// What the provider adapter has when a turn ends: nothing, JSON text (Codex), or an already parsed
/// value (Claude's structured output).
#[derive(Clone, Debug, PartialEq)]
pub enum FinalReply {
    Missing,
    Text(String),
    Json(Value),
}

impl FinalReply {
    fn raw(&self) -> String {
        match self {
            FinalReply::Missing => String::new(),
            FinalReply::Text(text) => text.clone(),
            FinalReply::Json(value) => value.to_string(),
        }
    }
}

/// Validates a final reply against the schema of `kind` and the rules a schema cannot express: text
/// that must not be blank, finding ids that parse, scope globs that compile. It never fails and never
/// panics. A reply that cannot be used becomes `Report::Invalid`.
pub fn parse_report(kind: ReportKind, reply: &FinalReply, changes: &TurnChanges) -> Reported {
    let raw = reply.raw();
    let parsed = reply_json(reply).and_then(|value| match kind {
        ReportKind::Plan => PlanWire::read(value).map(|report| Reported::Plan { report }),
        ReportKind::Implementer => {
            ImplementerWire::read(value).map(|report| Reported::Implementer {
                report,
                changes: changes.clone(),
            })
        }
        ReportKind::Review => ReviewWire::read(value).map(|report| Reported::Review { report }),
        ReportKind::Answer => AnswerWire::read(value).map(|report| Reported::Answer { report }),
        ReportKind::Rulings => RulingsWire::read(value).map(|report| Reported::Rulings { report }),
    });
    match parsed {
        Ok(reported) => reported,
        Err(error) => invalid(kind, error, raw, changes),
    }
}

fn reply_json(reply: &FinalReply) -> Result<Value, String> {
    match reply {
        FinalReply::Missing => Err("The turn ended without a report.".to_owned()),
        FinalReply::Text(text) => {
            serde_json::from_str(text).map_err(|_| "The report is not valid JSON.".to_owned())
        }
        FinalReply::Json(value) => Ok(value.clone()),
    }
}

fn invalid(kind: ReportKind, error: String, raw: String, changes: &TurnChanges) -> Reported {
    match kind {
        ReportKind::Plan => Reported::Plan {
            report: Report::Invalid { error, raw },
        },
        ReportKind::Implementer => Reported::Implementer {
            report: Report::Invalid { error, raw },
            changes: changes.clone(),
        },
        ReportKind::Review => Reported::Review {
            report: Report::Invalid { error, raw },
        },
        ReportKind::Answer => Reported::Answer {
            report: Report::Invalid { error, raw },
        },
        ReportKind::Rulings => Reported::Rulings {
            report: Report::Invalid { error, raw },
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Wire shapes. They are private, and nothing outside this module names them.
// ---------------------------------------------------------------------------------------------

// The statuses every kind of report shares. Their meaning is on the `status` field of each report.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Status {
    Done,
    Blocked,
    Question,
    PlanChange,
}

/// The fields every report carries for the ways a turn can stop short of finishing.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AskWire {
    /// Fill in only when status is question: the one question you need answered. Otherwise "".
    question: String,
    /// Fill in only when status is plan_change: why the plan must change. Otherwise "".
    plan_change_reason: String,
    /// Fill in only when status is plan_change: extra files you need in scope, as globs. Otherwise [].
    plan_change_scope: Vec<String>,
}

/// The wire fields of one kind of report that decide how it ends.
struct Ending {
    status: Status,
    summary: String,
    ask: AskWire,
    /// Whether this kind of work may ask to change the plan.
    can_change_plan: bool,
}

impl Ending {
    /// Everything but the finished case, which each kind builds itself.
    fn report<T>(self, finished: impl FnOnce() -> Result<T, String>) -> Result<Report<T>, String> {
        require_text("summary", &self.summary)?;
        let Ending {
            status,
            summary,
            ask,
            can_change_plan,
        } = self;
        match status {
            Status::Done => finished().map(|done| Report::Finished { done }),
            Status::Blocked => Ok(Report::Blocked { summary }),
            Status::Question => {
                require_text("ask.question", &ask.question)?;
                Ok(Report::Question {
                    summary,
                    question: ask.question,
                })
            }
            Status::PlanChange if !can_change_plan => Err(
                "status: plan_change is not available for this report. Use question or blocked."
                    .to_owned(),
            ),
            Status::PlanChange => plan_change(summary, ask),
        }
    }
}

fn plan_change<T>(summary: String, ask: AskWire) -> Result<Report<T>, String> {
    require_text("ask.plan_change_reason", &ask.plan_change_reason)?;
    let extra_scope = scope_patterns("ask.plan_change_scope", &ask.plan_change_scope)?;
    Ok(Report::PlanChange {
        summary,
        request: PlanChangeRequest {
            reason: ask.plan_change_reason,
            extra_scope,
        },
    })
}

fn require_text(field: &str, text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err(format!("{field}: must not be blank"));
    }
    Ok(())
}

fn scope_patterns(field: &str, globs: &[String]) -> Result<Vec<ScopePattern>, String> {
    globs
        .iter()
        .map(|glob| ScopePattern::new(glob).map_err(|error| format!("{field}: {error}")))
        .collect()
}

fn finding_id(field: &str, text: &str) -> Result<FindingId, String> {
    text.parse().map_err(|error| format!("{field}: {error}"))
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PlanWire {
    /// done: the plan is ready. blocked: something outside your control stops you. question: you
    /// need an answer first. Never guess when something is unclear.
    status: Status,
    /// What you did, in a sentence.
    summary: String,
    /// What to build.
    build: String,
    /// What done means, as short statements a reviewer can check. Empty unless status is done.
    done_means: Vec<String>,
    /// The files the work may touch, as globs relative to the repository root such as src/login/**.
    /// Empty unless status is done. Write ** only when every file is in scope.
    files_in_scope: Vec<String>,
    ask: AskWire,
}

impl PlanWire {
    fn read(value: Value) -> Result<Report<TaskPlan>, String> {
        let wire: PlanWire = from_value(value)?;
        let PlanWire {
            status,
            summary,
            build,
            done_means,
            files_in_scope,
            ask,
        } = wire;
        let ending = Ending {
            status,
            summary,
            ask,
            can_change_plan: false,
        };
        ending.report(|| {
            let scope = scope_patterns("files_in_scope", &files_in_scope)?;
            TaskPlan::new(build, done_means, scope).map_err(|problem| problem.to_string())
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ResponseWire {
    finding_id: String,
    /// fixed or disputed.
    response: ResponseKind,
    /// Why you dispute the finding. "" for a fix.
    reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ResponseKind {
    Fixed,
    Disputed,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ImplementerWire {
    /// done: the work is complete. blocked: something outside your control stops you. question: you
    /// need an answer before you can go on. plan_change: the plan must change, most often because the
    /// work needs more files than the plan allows.
    status: Status,
    /// What changed and why, in a few sentences.
    summary: String,
    /// The commands you ran to verify the work, each with its exit code.
    checks: Vec<Check>,
    /// One entry per open finding Kratos sent you. Empty before the first review.
    responses: Vec<ResponseWire>,
    /// Probe tests you added to the project. Empty unless you were told you may.
    added_probes: Vec<AddedProbe>,
    ask: AskWire,
}

impl ImplementerWire {
    fn read(value: Value) -> Result<Report<ImplementerDone>, String> {
        let wire: ImplementerWire = from_value(value)?;
        let ImplementerWire {
            status,
            summary,
            checks,
            responses,
            added_probes,
            ask,
        } = wire;
        let ending = Ending {
            status,
            summary: summary.clone(),
            ask,
            can_change_plan: true,
        };
        ending.report(|| {
            let responses = responses
                .into_iter()
                .map(ResponseWire::into_response)
                .collect::<Result<_, _>>()?;
            Ok(ImplementerDone {
                summary,
                checks,
                responses,
                added_probes,
            })
        })
    }
}

impl ResponseWire {
    fn into_response(self) -> Result<Response, String> {
        let outcome = match self.response {
            ResponseKind::Fixed => ResponseOutcome::Fixed,
            ResponseKind::Disputed => ResponseOutcome::Disputed {
                reason: self.reason,
            },
        };
        Ok(Response {
            finding: finding_id("responses.finding_id", &self.finding_id)?,
            outcome,
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StillOpenWire {
    finding_id: String,
    /// What is still wrong, the evidence, and why the earlier fix did not work.
    diagnosis: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReviewWire {
    /// done: you finished the review. blocked: only the user can decide, and Kratos will not check any
    /// finding. question: you need an answer first, for example about the reasons for a change.
    status: Status,
    /// Starts with the number of findings, and says plainly when there is nothing to fix.
    summary: String,
    /// approve when nothing blocks, or changes_requested when an important finding blocks. Your verdict
    /// must agree with the findings Kratos confirms. Ignored unless status is done.
    verdict: Verdict,
    /// New findings only.
    findings: Vec<FindingReport>,
    /// Earlier findings that this commit does not fix, each with a diagnosis.
    still_open: Vec<StillOpenWire>,
    ask: AskWire,
}

impl ReviewWire {
    fn read(value: Value) -> Result<Report<ReviewDone>, String> {
        let wire: ReviewWire = from_value(value)?;
        let ReviewWire {
            status,
            summary,
            verdict,
            findings,
            still_open,
            ask,
        } = wire;
        let ending = Ending {
            status,
            summary: summary.clone(),
            ask,
            can_change_plan: false,
        };
        ending.report(|| {
            let still_open = still_open
                .into_iter()
                .map(|entry| {
                    require_text("still_open.diagnosis", &entry.diagnosis)?;
                    Ok(StillOpen {
                        finding: finding_id("still_open.finding_id", &entry.finding_id)?,
                        diagnosis: entry.diagnosis,
                    })
                })
                .collect::<Result<_, String>>()?;
            Ok(ReviewDone {
                verdict,
                summary,
                findings,
                still_open,
            })
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AnswerWire {
    /// done: you answered. blocked: something outside your control stops you. question: you cannot
    /// answer without the user. Never guess. plan_change: your answer would contradict the plan.
    status: Status,
    summary: String,
    /// Your answer to the question. Empty unless status is done.
    answer: String,
    ask: AskWire,
}

impl AnswerWire {
    fn read(value: Value) -> Result<Report<AnswerDone>, String> {
        let wire: AnswerWire = from_value(value)?;
        let ending = Ending {
            status: wire.status,
            summary: wire.summary,
            ask: wire.ask,
            can_change_plan: true,
        };
        ending.report(|| {
            require_text("answer", &wire.answer)?;
            Ok(AnswerDone {
                answer: wire.answer,
            })
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RulingWire {
    finding_id: String,
    /// bug: the finding is real, and the implementer must fix it. not_bug: the finding is not a bug.
    ruling: RulingVerdict,
    reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RulingsWire {
    /// done: you ruled on every disputed finding. blocked: something outside your control stops you.
    /// question: you cannot rule without the user. Never guess. plan_change: your ruling would
    /// contradict the plan.
    status: Status,
    summary: String,
    /// One ruling for each disputed finding. Empty unless status is done.
    rulings: Vec<RulingWire>,
    ask: AskWire,
}

impl RulingsWire {
    fn read(value: Value) -> Result<Report<RulingsDone>, String> {
        let wire: RulingsWire = from_value(value)?;
        let ending = Ending {
            status: wire.status,
            summary: wire.summary,
            ask: wire.ask,
            can_change_plan: true,
        };
        ending.report(|| {
            let rulings = wire
                .rulings
                .into_iter()
                .map(|ruling| {
                    require_text("rulings.reason", &ruling.reason)?;
                    Ok(Ruling {
                        finding: finding_id("rulings.finding_id", &ruling.finding_id)?,
                        verdict: ruling.ruling,
                        reason: ruling.reason,
                    })
                })
                .collect::<Result<_, String>>()?;
            Ok(RulingsDone { rulings })
        })
    }
}

fn from_value<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|error| error.to_string())
}

// ---------------------------------------------------------------------------------------------
// Schemas and the strict-subset check.
// ---------------------------------------------------------------------------------------------

/// Why a derived schema is outside the subset both CLIs accept.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SchemaError {
    #[error("report schema {at} uses {keyword}, which the providers' strict mode does not accept")]
    BannedKeyword { at: String, keyword: String },
    #[error("report schema {at} has optional properties: {properties:?}")]
    OptionalProperties { at: String, properties: Vec<String> },
    #[error("report schema {at} allows extra properties")]
    ExtraProperties { at: String },
    #[error("report schema {at} has a type the providers' strict mode does not accept")]
    UnsupportedType { at: String },
}

/// Keywords the providers' strict mode refuses.
const BANNED_KEYWORDS: [&str; 9] = [
    "anyOf",
    "oneOf",
    "allOf",
    "$ref",
    "$defs",
    "minLength",
    "maxLength",
    "pattern",
    "format",
];

/// Derives the JSON Schema of `T` and refuses it unless it stays inside the strict subset: objects
/// with every property required and `additionalProperties: false`, arrays, strings, integers,
/// booleans, and string enums. It drops `$schema`, and the `format` and bounds that schemars adds to
/// sized integers.
pub fn strict_json_schema<T: JsonSchema>() -> Result<Value, SchemaError> {
    let settings = schemars::generate::SchemaSettings::draft2020_12().with(|settings| {
        settings.inline_subschemas = true;
    });
    let root = schemars::SchemaGenerator::new(settings).into_root_schema_for::<T>();
    let mut schema = serde_json::to_value(&root).unwrap_or(Value::Null);
    if let Some(object) = schema.as_object_mut() {
        object.remove("$schema");
    }
    strict_node(schema, "#")
}

fn strict_node(node: Value, at: &str) -> Result<Value, SchemaError> {
    let Value::Object(mut object) = node else {
        return Err(SchemaError::UnsupportedType { at: at.to_owned() });
    };
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if kind.as_deref() == Some("integer") {
        // schemars bounds a u32 or an i32, which carries no meaning for a report.
        for key in ["format", "minimum", "maximum"] {
            object.remove(key);
        }
    }
    if let Some(keyword) = BANNED_KEYWORDS
        .iter()
        .find(|key| object.contains_key(**key))
    {
        return Err(SchemaError::BannedKeyword {
            at: at.to_owned(),
            keyword: (*keyword).to_owned(),
        });
    }
    match kind.as_deref() {
        Some("object") => strict_object(object, at),
        Some("array") => {
            let items = object.remove("items").unwrap_or(Value::Null);
            object.insert(
                "items".to_owned(),
                strict_node(items, &format!("{at}/items"))?,
            );
            Ok(Value::Object(object))
        }
        Some("string" | "integer" | "boolean") => Ok(Value::Object(object)),
        _ => Err(SchemaError::UnsupportedType { at: at.to_owned() }),
    }
}

fn strict_object(
    mut object: serde_json::Map<String, Value>,
    at: &str,
) -> Result<Value, SchemaError> {
    if object.get("additionalProperties") != Some(&Value::Bool(false)) {
        return Err(SchemaError::ExtraProperties { at: at.to_owned() });
    }
    let properties = match object.remove("properties") {
        Some(Value::Object(properties)) => properties,
        _ => serde_json::Map::new(),
    };
    let required: Vec<&str> = object
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let optional: Vec<String> = properties
        .keys()
        .filter(|name| !required.contains(&name.as_str()))
        .cloned()
        .collect();
    if !optional.is_empty() {
        return Err(SchemaError::OptionalProperties {
            at: at.to_owned(),
            properties: optional,
        });
    }
    let strict = properties
        .into_iter()
        .map(|(name, property)| {
            strict_node(property, &format!("{at}/{name}")).map(|node| (name, node))
        })
        .collect::<Result<serde_json::Map<_, _>, _>>()?;
    object.insert("properties".to_owned(), Value::Object(strict));
    Ok(Value::Object(object))
}

/// The JSON Schema a provider forces on the final reply of a turn of this kind. Derived once from the
/// wire struct and checked by `strict_json_schema`.
pub fn report_json_schema(kind: ReportKind) -> &'static Value {
    static SCHEMAS: LazyLock<BTreeMap<ReportKind, Value>> = LazyLock::new(|| {
        let derived = [
            (ReportKind::Plan, strict_json_schema::<PlanWire>()),
            (
                ReportKind::Implementer,
                strict_json_schema::<ImplementerWire>(),
            ),
            (ReportKind::Review, strict_json_schema::<ReviewWire>()),
            (ReportKind::Answer, strict_json_schema::<AnswerWire>()),
            (ReportKind::Rulings, strict_json_schema::<RulingsWire>()),
        ];
        derived
            .into_iter()
            .map(|(kind, schema)| {
                let schema = schema.unwrap_or_else(|error| {
                    // A wire struct outside the strict subset is a defect in this file. The
                    // `report_schemas` test fails on it first, so no live turn meets this panic.
                    panic!("the {kind:?} report schema is outside the strict subset: {error}")
                });
                (kind, schema)
            })
            .collect()
    });
    SCHEMAS.get(&kind).unwrap_or(&Value::Null)
}
