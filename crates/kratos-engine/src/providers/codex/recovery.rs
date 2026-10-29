//! What an engine that stopped left unfinished, settled before the actor serves anything (L1).
//!
//! No Codex process outlives the engine that started it, so after a restart nothing runs: a turn
//! the log shows running ended, a message being sent may or may not have reached Codex, and every
//! approval or question waits on a process that is gone.

use std::collections::{BTreeMap, HashMap};

use kratos_protocol::{
    ApprovalId, MessageId, MessageStatus, PauseReason, Provider, QuestionId, Seq, ThreadEventBody,
    ThreadId, TurnChanges, TurnId, TurnOutcome,
};
use kratos_store::{Settle, Store, StoreError, ThreadWrite};
use tokio::sync::mpsc;

use super::CodexSettings;
use super::actor::{Actor, Server};
use super::requests::Requests;
use super::wire::ChangesRecorder;
use crate::hub::{Hub, PAGE_BYTES};
use crate::store_thread::StoreThread;
use crate::usage::UsageTracker;
use crate::usage::scheduler::LimitStop;

fn add_waiting_requests(
    store: &Store,
    thread_id: &ThreadId,
    work: &mut Unfinished,
) -> Result<(), StoreError> {
    let approvals = store.waiting_approvals(thread_id)?;
    let questions = store.waiting_questions(thread_id)?;
    work.approvals
        .extend(approvals.into_iter().map(|approval| approval.id));
    work.questions
        .extend(questions.into_iter().map(|question| question.id));
    Ok(())
}

const UNCONFIRMED: &str = "Kratos stopped before Codex confirmed it had the message";
const RESTARTED: &str = "Kratos restarted, and the Codex process that asked is gone";

/// One thread's unfinished work.
#[derive(Default)]
struct Unfinished {
    /// The turn the log shows running, and what its recorded items changed.
    running_turn: Option<(TurnId, TurnChanges)>,
    sending: Vec<MessageId>,
    approvals: Vec<ApprovalId>,
    questions: Vec<QuestionId>,
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
            hub.write(thread_id, move |write| settle(write, work))
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
            next_attempt: 0,
            deferred: Vec::new(),
            reconcile: Vec::new(),
            done,
            completions,
            limit_stops: Some(limit_stops),
        };
        Ok((actor, uncertain))
    }
}

/// Closes the thread's waiting requests, ends its running turn as interrupted, and marks each
/// message being sent as uncertain, in one write.
fn settle(write: &mut ThreadWrite<'_>, work: Unfinished) -> Result<(), StoreError> {
    close_requests(write, &work)?;
    if let Some((turn_id, changes)) = work.running_turn {
        write.complete_turn(turn_id, TurnOutcome::Interrupted, changes, None)?;
        if !write.pending_messages()?.is_empty() {
            write.pause(PauseReason::Interrupted)?;
        }
    }
    for message_id in work.sending {
        let error = UNCONFIRMED.to_owned();
        let status = MessageStatus::Unknown {
            error: error.clone(),
        };
        write.set_message_status(&message_id, status)?;
        write.pause(PauseReason::DeliveryUncertain { message_id, error })?;
    }
    Ok(())
}

fn close_requests(write: &mut ThreadWrite<'_>, work: &Unfinished) -> Result<(), StoreError> {
    for approval in &work.approvals {
        write.settle_approval(approval, Settle::Close(RESTARTED.to_owned()))?;
    }
    for question in &work.questions {
        write.settle_question(question, Settle::Close(RESTARTED.to_owned()))?;
    }
    Ok(())
}

fn unfinished_codex_work(store: &mut Store) -> Result<BTreeMap<ThreadId, Unfinished>, StoreError> {
    let mut unfinished: BTreeMap<ThreadId, Unfinished> = BTreeMap::new();
    for (thread_id, turn_id) in store.threads_with_running_turns()? {
        let changes = recorded_changes(store, &thread_id, &turn_id)?;
        unfinished.entry(thread_id).or_default().running_turn = Some((turn_id, changes));
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
            return Ok(changes.take());
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
