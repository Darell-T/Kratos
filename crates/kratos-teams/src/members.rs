//! Who is on the Team and which job each member holds.
//!
//! Identity and role are separate. A member keeps its Kratos thread and history for the whole task,
//! and a role change moves jobs between members without touching either (D77).

use std::fmt;

use kratos_protocol::Provider;
use serde::{Deserialize, Serialize};

/// A Team member. The engine chooses the id when it builds the Team.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemberId(String);

impl MemberId {
    pub fn new(id: impl Into<String>) -> MemberId {
        MemberId(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Planner,
    Implementer,
    Reviewer,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Planner => "planner",
            Role::Implementer => "implementer",
            Role::Reviewer => "reviewer",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What the rules need to know about a member. The branch name starts with the implementer's
/// provider (D36). The model, permission, and thread belong to the engine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub id: MemberId,
    pub provider: Provider,
}

/// Which member holds which role. Two members hold implementer and reviewer, and three also hold a
/// planner. No member holds two roles. The constructors and `Deserialize` check this, so no value
/// of this type breaks it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "BindingsData", into = "BindingsData")]
pub struct Bindings {
    planner: Option<Member>,
    implementer: Member,
    reviewer: Member,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BindingsData {
    planner: Option<Member>,
    implementer: Member,
    reviewer: Member,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("member {0:?} holds two roles")]
pub struct SharedMember(pub MemberId);

/// Why a requested role change cannot be applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RebindProblem {
    #[error("a role change moves roles between the Team's own members and adds none")]
    DifferentMembers,
    #[error("a two-member Team has no planner, and a three-member Team always has one")]
    PlannerPresenceChanged,
    #[error("nobody would hold a different role")]
    Unchanged,
}

impl Bindings {
    pub fn two(implementer: Member, reviewer: Member) -> Result<Bindings, SharedMember> {
        Bindings::checked(None, implementer, reviewer)
    }

    pub fn three(
        planner: Member,
        implementer: Member,
        reviewer: Member,
    ) -> Result<Bindings, SharedMember> {
        Bindings::checked(Some(planner), implementer, reviewer)
    }

    fn checked(
        planner: Option<Member>,
        implementer: Member,
        reviewer: Member,
    ) -> Result<Bindings, SharedMember> {
        let bindings = Bindings {
            planner,
            implementer,
            reviewer,
        };
        let members = bindings.members();
        let repeated = members
            .iter()
            .enumerate()
            .find(|(index, member)| members[..*index].iter().any(|other| other.id == member.id));
        match repeated {
            Some((_, member)) => Err(SharedMember(member.id.clone())),
            None => Ok(bindings),
        }
    }

    pub fn member(&self, role: Role) -> Option<&Member> {
        match role {
            Role::Planner => self.planner.as_ref(),
            Role::Implementer => Some(&self.implementer),
            Role::Reviewer => Some(&self.reviewer),
        }
    }

    pub fn role_of(&self, id: &MemberId) -> Option<Role> {
        [Role::Planner, Role::Implementer, Role::Reviewer]
            .into_iter()
            .find(|role| self.member(*role).is_some_and(|member| member.id == *id))
    }

    pub fn has_planner(&self) -> bool {
        self.planner.is_some()
    }

    /// The Team's members in role order: planner, implementer, reviewer.
    pub fn members(&self) -> Vec<&Member> {
        self.planner
            .iter()
            .chain([&self.implementer, &self.reviewer])
            .collect()
    }

    /// The members ordered by id, to compare which people two Teams have and ignore their roles.
    fn sorted_members(&self) -> Vec<&Member> {
        let mut members = self.members();
        members.sort_by(|a, b| a.id.cmp(&b.id));
        members
    }

    /// Why `to` cannot replace these bindings, or `None` when it moves the same members between roles.
    pub fn reassignment_problem(&self, to: &Bindings) -> Option<RebindProblem> {
        if self.has_planner() != to.has_planner() {
            return Some(RebindProblem::PlannerPresenceChanged);
        }
        if self.sorted_members() != to.sorted_members() {
            return Some(RebindProblem::DifferentMembers);
        }
        (self == to).then_some(RebindProblem::Unchanged)
    }
}

impl TryFrom<BindingsData> for Bindings {
    type Error = SharedMember;

    fn try_from(data: BindingsData) -> Result<Bindings, SharedMember> {
        Bindings::checked(data.planner, data.implementer, data.reviewer)
    }
}

impl From<Bindings> for BindingsData {
    fn from(bindings: Bindings) -> BindingsData {
        BindingsData {
            planner: bindings.planner,
            implementer: bindings.implementer,
            reviewer: bindings.reviewer,
        }
    }
}
