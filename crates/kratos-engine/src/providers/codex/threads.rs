//! What the actor knows about one Codex thread between store reads.

use std::collections::{HashMap, HashSet};

use kratos_codex_client::types;
use kratos_protocol::{
    AccessLimit, ContextLimit, ContextUsage, FileChange, Message, MessageId, ThreadSettings, TurnId,
};

use super::actor::Reply;
use crate::providers::changes::ChangesRecorder;

pub(super) struct ThreadState {
    pub folder: String,
    pub settings: ThreadSettings,
    /// The settings the current or last turn started with. Codex's requests during a turn are
    /// decided by these, so a change applies from the next turn (D87). A request that arrives with
    /// no turn is decided by the last turn's.
    /// `None` before the first turn, when settings changes have no running work to affect.
    pub turn_settings: Option<ThreadSettings>,
    pub link: Link,
    pub turn: Turn,
    /// Messages being steered into the running turn. The queue waits for their answers.
    pub steering: HashSet<MessageId>,
    /// File changes by item id, for the approval Codex sends after the item starts. A turn's start
    /// clears them, and so does recording its end. A request that arrives with no turn reads what
    /// is left.
    pub file_items: HashMap<String, Vec<FileChange>>,
    /// Discards that wait for Codex to load the thread, run again once it has.
    pub discards: Vec<(MessageId, Reply<Message>)>,
}

impl ThreadState {
    pub fn new(folder: String, settings: ThreadSettings) -> ThreadState {
        ThreadState {
            folder,
            turn_settings: None,
            settings,
            link: Link::Detached,
            turn: Turn::Idle,
            steering: HashSet::new(),
            file_items: HashMap::new(),
            discards: Vec::new(),
        }
    }

    pub fn turn_settings(&self) -> &ThreadSettings {
        self.turn_settings.as_ref().unwrap_or(&self.settings)
    }

    /// The thread's Codex id, when it is loaded in the running app-server.
    pub fn native_id(&self) -> Option<&str> {
        match &self.link {
            Link::Attached { native_id, .. } => Some(native_id),
            Link::Detached | Link::Attaching => None,
        }
    }

    /// The model Codex runs the thread's turns with: the setting, else the one Codex chose.
    pub fn model(&self) -> Option<&str> {
        match (&self.settings.model, &self.link) {
            (Some(model), _) => Some(model.as_str()),
            (None, Link::Attached { model, .. }) => Some(model),
            (None, _) => None,
        }
    }
}

/// Whether the thread is loaded in the running app-server.
pub(super) enum Link {
    /// Not loaded. A load that failed leaves the thread here, and the pause says why. Kratos does
    /// not start a new Codex thread in its place (L7), and the next use tries again.
    Detached,
    /// `thread/start` or `thread/resume` is out.
    Attaching,
    Attached {
        native_id: String,
        model: String,
        /// The context limit Codex loaded the thread with (D76).
        context_limit: Option<ContextLimit>,
        /// The access under which this session may have cached approval grants.
        access: AccessLimit,
    },
}

/// What Codex reported during a turn, kept from its start, since reports can arrive before the
/// start's answer, until its end is recorded.
#[derive(Default)]
pub(super) struct TurnWork {
    pub changes: ChangesRecorder,
    /// The latest context report (D76).
    pub context: Option<ContextUsage>,
}

pub(super) enum Turn {
    Idle,
    /// `turn/start` is out for `message_id`. A completion that arrives before Codex answers is
    /// kept, so the turn ends as soon as its id is known.
    Starting {
        message_id: MessageId,
        early_completion: Option<Box<types::Turn>>,
        /// Stops asked for before the turn's id was known; answered once it is.
        stop_waiters: Vec<super::actor::Reply<()>>,
        work: TurnWork,
    },
    Running {
        turn_id: TurnId,
        /// Set once `turn/interrupt` is out or accepted.
        stopping: bool,
        /// Changes whenever the deadline is set again, so an earlier deadline's timer is ignored.
        deadline_epoch: u64,
        work: TurnWork,
    },
}

impl Turn {
    /// What the starting or running turn has gathered. `None` when idle, where Codex's reports
    /// belong to no turn and are not kept.
    pub fn work_mut(&mut self) -> Option<&mut TurnWork> {
        match self {
            Turn::Starting { work, .. } | Turn::Running { work, .. } => Some(work),
            Turn::Idle => None,
        }
    }

    /// Ends a running turn, returning its id and what it gathered. Any other turn stays as it is.
    pub fn end_running(&mut self) -> Option<(TurnId, TurnWork)> {
        if !matches!(self, Turn::Running { .. }) {
            return None;
        }
        let Turn::Running { turn_id, work, .. } = std::mem::replace(self, Turn::Idle) else {
            unreachable!("matched a running turn above");
        };
        Some((turn_id, work))
    }

    pub fn running_id(&self) -> Option<&TurnId> {
        match self {
            Turn::Running { turn_id, .. } => Some(turn_id),
            Turn::Idle | Turn::Starting { .. } => None,
        }
    }
}
