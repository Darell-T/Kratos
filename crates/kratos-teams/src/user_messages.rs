//! Messages from the user to a member (D33). The user can message any member at any time, and a message
//! never replaces the step that is running.
//!
//! The engine records the message with its target member and a delivery id before it tries to deliver
//! it, and then records what it did. Either the message reached the member's running turn, or it is
//! kept for the member's next message. A step cannot complete while one of its member's messages has
//! no recorded fate. A message that missed the running turn goes out with the member's next message
//! once, and a lost turn does not count as received, so the message goes again.

use crate::facts::{DeliveryId, Disposition, FactSeq};
use crate::fold::Rejection;
use crate::members::MemberId;
use crate::view::{Direction, DirectionStatus, Phase, Run, TaskView, Turn};

impl TaskView {
    /// Records the message as `Received`. It does not touch the phase.
    pub(crate) fn on_user_message(
        mut self: Box<Self>,
        seq: FactSeq,
        delivery: DeliveryId,
        member: MemberId,
        text: String,
    ) -> Result<Box<TaskView>, Rejection> {
        if matches!(self.phase, Phase::Closing { .. } | Phase::Closed { .. }) {
            return Err(self.misfit("user_message"));
        }
        let Some(role) = self.bindings.role_of(&member) else {
            return Err(Rejection::UnknownMember(member));
        };
        if self.directions.iter().any(|d| d.delivery == delivery) {
            return Err(Rejection::DuplicateDelivery(delivery));
        }
        self.directions.push(Direction {
            delivery,
            member,
            role,
            text,
            recorded_at: seq,
            status: DirectionStatus::Received,
        });
        Ok(self)
    }

    pub(crate) fn on_message_disposed(
        mut self: Box<Self>,
        delivery: DeliveryId,
        how: Disposition,
    ) -> Result<Box<TaskView>, Rejection> {
        let known = self.directions.iter().any(|d| d.delivery == delivery);
        let received = self
            .directions
            .iter_mut()
            .find(|d| d.delivery == delivery && d.status == DirectionStatus::Received);
        let Some(direction) = received else {
            return Err(if known {
                self.misfit("message_disposed")
            } else {
                Rejection::UnknownDelivery(delivery)
            });
        };
        direction.status = match how {
            Disposition::Steered => DirectionStatus::Steered,
            Disposition::Queued => DirectionStatus::Queued,
        };
        Ok(self)
    }

    /// A step cannot complete while any message to its member has no recorded fate.
    pub(crate) fn refuse_undisposed(&self, member: &MemberId) -> Result<(), Rejection> {
        let deliveries: Vec<DeliveryId> = self
            .directions
            .iter()
            .filter(|d| d.member == *member && d.status == DirectionStatus::Received)
            .map(|d| d.delivery.clone())
            .collect();
        if deliveries.is_empty() {
            return Ok(());
        }
        Err(Rejection::UndisposedMessages {
            member: member.clone(),
            deliveries,
        })
    }

    /// A turn that ended with a report received what was steered into it and what rode its message.
    pub(crate) fn deliver_to(&mut self, turn: &Turn) {
        for direction in self
            .directions
            .iter_mut()
            .filter(|d| d.member == turn.member)
        {
            let reached = match direction.status {
                DirectionStatus::Steered => true,
                DirectionStatus::Queued => direction.recorded_at < turn.due,
                _ => false,
            };
            if reached {
                direction.status = DirectionStatus::Delivered;
            }
        }
    }

    /// A lost turn does not count as received, so what was steered into it goes again.
    pub(crate) fn requeue_steered(&mut self, member: &MemberId) {
        for direction in self.directions.iter_mut().filter(|d| d.member == *member) {
            if direction.status == DirectionStatus::Steered {
                direction.status = DirectionStatus::Queued;
            }
        }
    }

    /// The messages queued for the turn's member before its message became due. They are sent with it.
    fn carried_by(&self, turn: &Turn) -> Vec<Direction> {
        self.directions
            .iter()
            .filter(|d| {
                d.member == turn.member
                    && d.status == DirectionStatus::Queued
                    && d.recorded_at < turn.due
            })
            .cloned()
            .collect()
    }

    /// What the engine needs to deliver the turn's message.
    pub(crate) fn run_of(&self, turn: &Turn) -> Run {
        Run {
            step: turn.step,
            message: turn.message.clone(),
            follows: turn.follows.clone(),
            member: turn.member.clone(),
            role: turn.work.role(),
            work: turn.work.clone(),
            plan_mode: turn.work.plan_mode(),
            note: turn.note.clone(),
            resolutions: turn.resolutions.clone(),
            carried: self.carried_by(turn),
        }
    }
}
