//! What an engine that stopped left unfinished, settled before the actor serves anything (L1).
//!
//! No Codex process outlives the engine that started it, so after a restart nothing runs: a turn
//! the log shows running ended, a message being sent may or may not have reached Codex, and every
//! approval or question waits on a process that is gone.

use std::collections::{BTreeMap, HashMap};

use kratos_protocol::{
    ContextUsage, MessageId, PauseReason, Provider, Seq, ThreadEventBody, ThreadId, TurnChanges,
    TurnId, TurnOutcome,
};
use kratos_store::{Store, StoreError, ThreadWrite};
use tokio::sync::mpsc;

use super::actor::{Actor, Server};
use super::approvals::{Closed, Requests, settle_closed};
use super::{CodexSettings, queue};
use crate::hub::{Hub, PAGE_BYTES};
use crate::providers::changes::ChangesRecorder;
use crate::store_thread::StoreThread;
use crate::usage::LimitStop;
use crate::usage::UsageTracker;

fn add_waiting_requests(
    store: &Store,
    thread_id: &ThreadId,
    work: &mut Unfinished,
) -> Result<(), StoreError> {
    let approvals = store.waiting_approvals(thread_id)?;
    let questions = store.waiting_questions(thread_id)?;
    work.requests
        .approvals
        .extend(approvals.into_iter().map(|approval| approval.id));
    work.requests
        .questions
        .extend(questions.into_iter().map(|question| question.id));
    Ok(())
}

const UNCONFIRMED: &str = "Kratos stopped before Codex confirmed it had the message";
const RESTARTED: &str = "Kratos restarted, and the Codex process that asked is gone";

/// One thread's unfinished work.
#[derive(Default)]
pub(super) struct Unfinished {
    /// The turn the log shows running, and what its recorded items changed.
    pub running_turn: Option<EndingTurn>,
    pub sending: Vec<MessageId>,
    pub requests: Closed,
}

pub(super) struct EndingTurn {
    pub id: TurnId,
    pub changes: TurnChanges,
    pub context: Option<ContextUsage>,
    pub outcome: TurnOutcome,
    pub pause: PauseReason,
}

pub(super) enum EndReason {
    Restarted,
    Exited(String),
}

impl Actor {
    /// Settles what the last engine left, then returns the actor and the threads whose uncertain
    /// messages Codex's history may settle once Codex runs again.
    pub(super) async fn recover(
        settings: CodexSettings,
        hub: Hub,
        store: StoreThread,
        usage: UsageTracker,
        limit_stops: mpsc::UnboundedSender<LimitStop>,
    ) -> Result<(Actor, Vec<ThreadId>), StoreError> {
        let unfinished = store.call(unfinished_codex_work).await?;
        let mut uncertain = Vec::new();
        for (thread_id, work) in unfinished {
            if !work.sending.is_empty() {
                uncertain.push(thread_id.clone());
            }
            hub.write(thread_id, move |write| {
                settle(write, work, EndReason::Restarted)
            })
            .await?;
        }
        let (done, completions) = mpsc::unbounded_channel();
        let actor = Actor {
            settings,
            hub,
            store,
            usage,
            server: Server::Down,
            generation: 0,
            restarts: Default::default(),
            windows: HashMap::new(),
            plan: None,
            threads: HashMap::new(),
            requests: Requests::default(),
            ready: Default::default(),
            deferred: Vec::new(),
            reconcile: Vec::new(),
            done,
            completions,
            limit_stops,
        };
        Ok((actor, uncertain))
    }
}

/// Closes the thread's waiting requests, ends its running turn, and marks each
/// message being sent as uncertain, in one write.
pub(super) fn settle(
    write: &mut ThreadWrite<'_>,
    work: Unfinished,
    reason: EndReason,
) -> Result<(), StoreError> {
    let (request_reason, uncertain_reason) = match &reason {
        EndReason::Restarted => (RESTARTED, UNCONFIRMED),
        EndReason::Exited(error) => ("Codex exited", error.as_str()),
    };
    settle_closed(write, &work.requests, request_reason)?;
    if let Some(turn) = work.running_turn {
        write.complete_turn(turn.id, turn.outcome, turn.changes, turn.context)?;
        queue::pause_if_waiting(write, turn.pause)?;
    }
    work.sending.into_iter().try_for_each(|message_id| {
        queue::mark_uncertain(write, message_id, uncertain_reason.to_owned())
    })?;
    Ok(())
}

fn unfinished_codex_work(store: &mut Store) -> Result<BTreeMap<ThreadId, Unfinished>, StoreError> {
    let mut unfinished: BTreeMap<ThreadId, Unfinished> = BTreeMap::new();
    for (thread_id, turn_id) in store.threads_with_running_turns()? {
        let changes = recorded_changes(store, &thread_id, &turn_id)?;
        unfinished.entry(thread_id).or_default().running_turn = Some(EndingTurn {
            id: turn_id,
            changes,
            context: None,
            outcome: TurnOutcome::Interrupted,
            pause: PauseReason::Interrupted,
        });
    }
    for (thread_id, message_id) in store.submitting_messages()? {
        unfinished
            .entry(thread_id)
            .or_default()
            .sending
            .push(message_id);
    }
    for thread_id in store.threads_with_waiting_requests()? {
        let work = unfinished.entry(thread_id.clone()).or_default();
        add_waiting_requests(store, &thread_id, work)?;
    }
    codex_threads_only(store, unfinished)
}

/// The work of Codex threads. Another provider's threads are not this actor's to settle.
fn codex_threads_only(
    store: &Store,
    unfinished: BTreeMap<ThreadId, Unfinished>,
) -> Result<BTreeMap<ThreadId, Unfinished>, StoreError> {
    let mut codex = BTreeMap::new();
    for (thread_id, work) in unfinished {
        if store.provider_thread(&thread_id)?.thread.provider == Provider::Codex {
            codex.insert(thread_id, work);
        }
    }
    Ok(codex)
}

/// What the turn changed, counted from the items the log recorded for it the way the live turn
/// counts them.
fn recorded_changes(
    store: &Store,
    thread_id: &ThreadId,
    turn_id: &TurnId,
) -> Result<TurnChanges, StoreError> {
    let mut changes = ChangesRecorder::default();
    let mut after = Seq::BEFORE_FIRST;
    loop {
        let page = store.events_after(thread_id, after, PAGE_BYTES)?;
        let Some(last) = page.last() else {
            return Ok(changes.finish());
        };
        after = last.seq;
        for event in &page {
            if let ThreadEventBody::ItemCompleted {
                turn_id: item_turn,
                item,
            } = &event.event
                && item_turn == turn_id
            {
                changes.record(item);
            }
        }
    }
}
