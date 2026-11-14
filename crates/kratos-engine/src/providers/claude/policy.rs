//! Claude's permission table. Parsing and filesystem resolution precede this pure decision.

use kratos_protocol::{AskReason, Permission, ThreadSettings};

use crate::providers::writes;
pub(super) use crate::providers::writes::Destination;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Action {
    File(Destination),
    /// A shell command the CLI runs as the user. `flagged` is the dangerous-command check's verdict.
    Command {
        flagged: bool,
    },
    /// Changes nothing on disk or on the network: a read of an unprotected path, the to-do list.
    Read,
    /// A read from the network.
    Fetch,
    ProtectedRead,
    /// Needs no permission in any mode: loading a skill's or a tool's instructions.
    Free,
    /// A tool Kratos cannot judge, which the user decides.
    Unchecked,
    Question,
    ExitPlan,
    Invalid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Decision {
    Allow,
    Ask(AskReason),
    Question,
    Deny,
}

pub(super) fn decide(settings: &ThreadSettings, action: Action) -> Decision {
    match (action, settings.plan_mode, settings.permission) {
        (Action::Question, _, _) => Decision::Question,
        (Action::File(destination), _, _) => {
            writes::asks(settings, destination).map_or(Decision::Allow, Decision::Ask)
        }
        (Action::Free, _, _) => Decision::Allow,
        (Action::ProtectedRead, false, Permission::FullAccess) => Decision::Allow,
        (Action::Invalid | Action::ProtectedRead, _, _) => Decision::Deny,
        (Action::ExitPlan, true, _) => Decision::Deny,
        (Action::Command { flagged: true }, _, _) => Decision::Ask(AskReason::Dangerous),
        (Action::Unchecked, _, _) => Decision::Ask(AskReason::Unchecked),
        // Plan mode reads without asking under every permission; it only writes nothing.
        (Action::Read, true, _) => Decision::Allow,
        (_, true, _) | (_, _, Permission::AskMe) => Decision::Ask(AskReason::AskEveryAction),
        _ => Decision::Allow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_permission_and_plan_mode_obeys_the_complete_tool_table() {
        // Catches permissions bypassing protected writes, dangerous commands, planning, or unknown tools.
        for permission in Permission::ALL {
            for plan_mode in [false, true] {
                let settings = ThreadSettings {
                    permission,
                    plan_mode,
                    ..ThreadSettings::default()
                };
                let ordinary = if permission == Permission::AskMe || plan_mode {
                    Decision::Ask(AskReason::AskEveryAction)
                } else {
                    Decision::Allow
                };
                // Plan mode reads without asking; outside it, Ask me asks before every action.
                let read = if plan_mode || permission != Permission::AskMe {
                    Decision::Allow
                } else {
                    Decision::Ask(AskReason::AskEveryAction)
                };
                assert_eq!(
                    decide(&settings, Action::Read),
                    read,
                    "{permission:?}, plan {plan_mode}"
                );
                assert_eq!(decide(&settings, Action::Free), Decision::Allow);
                for action in [
                    Action::Fetch,
                    Action::File(Destination::Inside),
                    Action::Command { flagged: false },
                ] {
                    assert_eq!(
                        decide(&settings, action.clone()),
                        ordinary,
                        "{permission:?}, plan {plan_mode}, {action:?}"
                    );
                }
                assert_eq!(decide(&settings, Action::Question), Decision::Question);
                assert_eq!(decide(&settings, Action::Invalid), Decision::Deny);
                assert_eq!(
                    decide(&settings, Action::ProtectedRead),
                    if permission == Permission::FullAccess && !plan_mode {
                        Decision::Allow
                    } else {
                        Decision::Deny
                    }
                );
                assert_eq!(
                    decide(&settings, Action::Unchecked),
                    Decision::Ask(AskReason::Unchecked)
                );
                assert_eq!(
                    decide(
                        &settings,
                        Action::File(Destination::Protected("credentials".into()))
                    ),
                    Decision::Ask(AskReason::ProtectedFolder {
                        path: "credentials".into()
                    })
                );
                assert_eq!(
                    decide(&settings, Action::Command { flagged: true }),
                    Decision::Ask(AskReason::Dangerous)
                );
                assert_eq!(
                    decide(&settings, Action::ExitPlan),
                    if plan_mode {
                        Decision::Deny
                    } else {
                        ordinary.clone()
                    }
                );
            }
        }
    }
}
