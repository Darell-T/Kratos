//! Claude's permission table. Parsing and filesystem resolution precede this pure decision.

use kratos_protocol::{AskReason, Permission, ThreadSettings};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Destination {
    Inside,
    Outside,
    Protected(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Action {
    File(Destination),
    Command { flagged: bool, outside: bool },
    Read,
    ProtectedRead,
    ExternalTool,
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
        (Action::Invalid, _, _) => Decision::Deny,
        (Action::Question, _, _) => Decision::Question,
        (Action::ProtectedRead, false, Permission::FullAccess) => Decision::Allow,
        (Action::ProtectedRead, _, _) => Decision::Deny,
        (Action::ExitPlan | Action::Command { outside: true, .. }, true, _) => Decision::Deny,
        (Action::File(Destination::Protected(path)), _, _) => {
            Decision::Ask(AskReason::ProtectedFolder { path })
        }
        (Action::Command { flagged: true, .. }, _, _) => Decision::Ask(AskReason::Dangerous),
        (Action::Command { outside: true, .. }, _, _) => Decision::Ask(AskReason::Escalation),
        (Action::ExternalTool, _, _) => Decision::Ask(AskReason::Unchecked),
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
                for action in [
                    Action::Read,
                    Action::File(Destination::Inside),
                    Action::File(Destination::Outside),
                    Action::Command {
                        flagged: false,
                        outside: false,
                    },
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
                    decide(&settings, Action::ExternalTool),
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
                    decide(
                        &settings,
                        Action::Command {
                            flagged: true,
                            outside: false
                        }
                    ),
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
                assert_eq!(
                    decide(
                        &settings,
                        Action::Command {
                            flagged: true,
                            outside: true
                        }
                    ),
                    if plan_mode {
                        Decision::Deny
                    } else {
                        Decision::Ask(AskReason::Dangerous)
                    }
                );
                assert_eq!(
                    decide(
                        &settings,
                        Action::Command {
                            flagged: false,
                            outside: true
                        }
                    ),
                    if plan_mode {
                        Decision::Deny
                    } else {
                        Decision::Ask(AskReason::Escalation)
                    }
                );
            }
        }
    }
}
