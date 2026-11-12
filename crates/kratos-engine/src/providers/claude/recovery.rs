//! Settling unfinished Claude work after an engine restart or a CLI exit (L1).
//!
//! No CLI outlives the engine that started it, so after a restart nothing runs: a turn the log
//! shows running ended, and a message written to a CLI may or may not have reached the transcript.
//! The transcript is Claude's own record of what it took, so it settles each message (D11): one it
//! holds was delivered, one it lacks is unknown, never undelivered, unless Claude had not begun a
//! turn, and a transcript cut off mid-write makes every other message unknown.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::Instant;

use kratos_claude_client::{
    DeliveredMessages, Exit, SessionId, delivered_message_ids, transcript_path,
};
use kratos_protocol::{
    MessageId, MessageStatus, PauseReason, Provider, ThreadId, TurnId, TurnOutcome,
};
use kratos_store::{Store, StoreError, ThreadWrite};
use tokio::sync::mpsc;

use super::actor::{Actor, Done, TranscriptWork};
use super::ends::{TurnEnd, end_turn};
use super::threads::{InFlight, Session};
use super::{ClaudeSettings, wire};
use crate::hub::Hub;
use crate::providers::changes::ChangesRecorder;
use crate::providers::queue::{mark_refused, mark_uncertain, pause_if_waiting};
use crate::store_thread::StoreThread;
use crate::usage::{LimitStop, UsageTracker};

const RESTARTED: &str = "Kratos restarted before Claude confirmed it had the message";

/// Messages written to a CLI that is gone, and what is known about the turn they were in.
pub(super) struct Settling {
    pub sent: Vec<InFlight>,
    /// The turn the log shows running, which the accepted messages belong to.
    pub run: Option<TurnId>,
    /// Claude had begun a turn in the process, so a message it lacks may still have been taken.
    pub started: bool,
    /// Why the messages are in doubt: the exit's text.
    pub reason: String,
}

/// What the transcript says of one message.
enum Verdict {
    Delivered,
    /// Claude never began a turn, so it never took the message, and sending it again is safe.
    NeverTaken,
    Uncertain,
}

fn verdict(native: &str, delivered: &DeliveredMessages, started: bool) -> Verdict {
    if delivered.ids.contains(native) {
        Verdict::Delivered
    } else if !delivered.torn_tail && !started {
        Verdict::NeverTaken
    } else {
        Verdict::Uncertain
    }
}

/// Settles each message still being sent by what the transcript holds, in the caller's write.
pub(super) fn settle_messages(
    write: &mut ThreadWrite<'_>,
    settling: &Settling,
    delivered: &DeliveredMessages,
) -> Result<(), StoreError> {
    let submitting: HashSet<MessageId> = write
        .pending_messages()?
        .into_iter()
        .filter(|message| message.status == MessageStatus::Submitting)
        .map(|message| message.id)
        .collect();
    for flight in &settling.sent {
        if !submitting.contains(&flight.message_id) {
            continue;
        }
        let message_id = flight.message_id.clone();
        match verdict(&flight.native, delivered, settling.started) {
            Verdict::Delivered => {
                let turn_id = settling
                    .run
                    .clone()
                    .unwrap_or_else(|| wire::turn_of(&message_id));
                write.set_message_status(&message_id, MessageStatus::Accepted { turn_id })?;
            }
            Verdict::NeverTaken => mark_refused(write, message_id, settling.reason.clone())?,
            Verdict::Uncertain => mark_uncertain(write, message_id, settling.reason.clone())?,
        }
    }
    Ok(())
}

impl Actor {
    /// Settles what the last engine left, then returns the actor.
    pub(super) async fn recover(
        settings: ClaudeSettings,
        hub: Hub,
        store: StoreThread,
        usage: UsageTracker,
        limit_stops: mpsc::UnboundedSender<LimitStop>,
    ) -> Result<Actor, StoreError> {
        let unfinished = store.call(unfinished_claude_work).await?;
        for (thread_id, work) in unfinished {
            let delivered = read_transcript(&settings.claude_home, &thread_id, &work).await;
            let settling = Settling {
                sent: work
                    .sending
                    .into_iter()
                    .map(|message_id| InFlight {
                        native: wire::native_id(&message_id).to_string(),
                        message_id,
                    })
                    .collect(),
                run: work.running,
                started: true,
                reason: RESTARTED.to_owned(),
            };
            hub.write(thread_id, move |write| {
                for id in work.approvals {
                    write.settle_approval(
                        &id,
                        kratos_store::Settle::Close("Kratos restarted".into()),
                    )?;
                }
                for id in work.questions {
                    write.settle_question(
                        &id,
                        kratos_store::Settle::Close("Kratos restarted".into()),
                    )?;
                }
                if let Some(turn_id) = settling.run.clone() {
                    write.complete_turn(
                        turn_id,
                        TurnOutcome::Interrupted,
                        ChangesRecorder::default().finish(),
                        None,
                    )?;
                    pause_if_waiting(write, PauseReason::Interrupted)?;
                }
                settle_messages(write, &settling, &delivered)
            })
            .await?;
        }
        Ok(Actor::new(settings, hub, store, usage, limit_stops))
    }

    /// A CLI ended. The turn it ran ends as failed and the queue behind it pauses. A message it was
    /// sent is settled from its transcript, so none is sent again that Claude may have taken.
    pub(super) async fn on_exit(&mut self, thread_id: &ThreadId, exit: Exit) {
        self.close_requests(thread_id, None, "Claude exited").await;
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return;
        };
        let Session::Open(open) = std::mem::replace(&mut thread.session, Session::Closed) else {
            return;
        };
        thread.epoch += 1;
        thread.stop_epoch += 1;
        thread.stopping = false;
        let reason = thread
            .close_reason
            .take()
            .unwrap_or_else(|| exit.to_string());
        if !open.worked {
            thread
                .restarts
                .failed(Instant::now(), self.settings.restart_wait, reason.clone());
        }
        for waiter in std::mem::take(&mut thread.stop_waiters) {
            let _ = waiter.send(Ok(()));
        }
        let run = thread.run.take();
        let sent = std::mem::take(&mut thread.pending);
        if sent.is_empty() {
            return;
        }
        let settling = Settling {
            sent,
            run: run.clone(),
            started: open.started,
            reason: reason.clone(),
        };
        let changes = std::mem::take(
            &mut self
                .threads
                .get_mut(thread_id)
                .expect("found above")
                .changes,
        )
        .finish();
        let ended = self
            .write(thread_id, move |write| match run {
                Some(turn_id) => end_turn(write, turn_id, TurnEnd::failed(reason), changes),
                // No turn started, but messages were in flight, so the user must hear why the
                // process ended.
                None => write
                    .pause(PauseReason::Unavailable { error: reason })
                    .map(|_| ()),
            })
            .await;
        if ended.is_err() {
            return;
        }
        let path = self.transcript_of(thread_id);
        let thread_id = thread_id.clone();
        self.spawn(async move {
            let result = match path {
                Some(path) => delivered_message_ids(&path).await,
                None => Ok(DeliveredMessages::default()),
            };
            Done::Transcript {
                thread_id,
                work: TranscriptWork::Settle(settling),
                result,
            }
        });
    }

    pub(super) async fn on_transcript(
        &mut self,
        thread_id: ThreadId,
        work: TranscriptWork,
        result: Result<DeliveredMessages, kratos_claude_client::TranscriptError>,
    ) {
        match work {
            TranscriptWork::Discard { message_id, reply } => {
                self.finish_discard(thread_id, message_id, reply, result)
                    .await;
            }
            TranscriptWork::Settle(settling) => {
                // A transcript that cannot be read proves nothing, so every message is unknown.
                let delivered = result.unwrap_or_else(|error| {
                    eprintln!("claude: cannot read the transcript: {error}");
                    DeliveredMessages {
                        ids: HashSet::new(),
                        torn_tail: true,
                    }
                });
                let _ = self
                    .write(&thread_id, move |write| {
                        settle_messages(write, &settling, &delivered)
                    })
                    .await;
                self.dispatch(&thread_id).await;
            }
        }
    }
}

/// What a Claude thread has unfinished: the turn the log shows running and the messages being
/// sent, with where its transcript lives.
struct Unfinished {
    approvals: Vec<kratos_protocol::ApprovalId>,
    questions: Vec<kratos_protocol::QuestionId>,
    running: Option<TurnId>,
    sending: Vec<MessageId>,
    folder: String,
    session: Option<String>,
}

fn unfinished_claude_work(store: &mut Store) -> Result<BTreeMap<ThreadId, Unfinished>, StoreError> {
    let mut found: BTreeMap<ThreadId, (Option<TurnId>, Vec<MessageId>)> = BTreeMap::new();
    for (thread_id, turn_id) in store.threads_with_running_turns()? {
        found.entry(thread_id).or_default().0 = Some(turn_id);
    }
    for (thread_id, message_id) in store.submitting_messages()? {
        found.entry(thread_id).or_default().1.push(message_id);
    }
    let mut unfinished = BTreeMap::new();
    for (thread_id, (running, sending)) in found {
        let thread = store.provider_thread(&thread_id)?;
        if thread.thread.provider == Provider::Claude {
            let approvals = store
                .waiting_approvals(&thread_id)?
                .into_iter()
                .map(|approval| approval.id)
                .collect();
            let questions = store
                .waiting_questions(&thread_id)?
                .into_iter()
                .map(|question| question.id)
                .collect();
            unfinished.insert(
                thread_id,
                Unfinished {
                    approvals,
                    questions,
                    running,
                    sending,
                    folder: thread.folder,
                    session: thread.native_id,
                },
            );
        }
    }
    Ok(unfinished)
}

/// The transcript of an unfinished thread. One that cannot be read, or does not parse as a
/// session, is treated as cut off: nothing it holds is proof either way. A thread whose session
/// the store never recorded reads its first session, and a missing transcript there means Claude
/// took no message, since Claude writes it when it takes the first.
async fn read_transcript(
    claude_home: &Path,
    thread_id: &ThreadId,
    work: &Unfinished,
) -> DeliveredMessages {
    let cut_off = DeliveredMessages {
        ids: HashSet::new(),
        torn_tail: true,
    };
    let session = match work.session.as_deref().map(str::parse::<SessionId>) {
        Some(Ok(stored)) => stored,
        Some(Err(_)) => return cut_off,
        None => wire::first_session(thread_id),
    };
    let path = transcript_path(claude_home, Path::new(&work.folder), session);
    if work.session.is_none() && !tokio::fs::try_exists(&path).await.unwrap_or(true) {
        return DeliveredMessages::default();
    }
    delivered_message_ids(&path).await.unwrap_or(cut_off)
}
