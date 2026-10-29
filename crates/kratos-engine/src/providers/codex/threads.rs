//! What the actor knows about one Codex thread between store reads.

use std::collections::{HashMap, HashSet};

use kratos_codex_client::types;
use kratos_protocol::{ContextUsage, FileChange, Message, MessageId, ThreadSettings, TurnId};

use super::actor::Reply;
use super::wire::ChangesRecorder;

pub(super) struct ThreadState {
    pub folder: String,
    pub settings: ThreadSettings,
    /// The settings the current or last turn started with. Codex's requests during a turn are
    /// decided by these, so a change applies from the next turn (D87).
    pub turn_settings: ThreadSettings,
    pub link: Link,
    pub turn: Turn,
    /// Messages being steered into the running turn. The queue waits for their answers.
    pub steering: HashSet<MessageId>,
    pub changes: ChangesRecorder,
    /// The running turn's file changes by item id, for the approval Codex sends after the item
    /// starts.
    pub file_items: HashMap<String, Vec<FileChange>>,
    /// The latest context report of the running turn (D76).
    pub context: Option<ContextUsage>,
    /// Discards that wait for Codex to load the thread, run again once it has.
    pub discards: Vec<(MessageId, Reply<Message>)>,
}

impl ThreadState {
    pub fn new(folder: String, settings: ThreadSettings) -> ThreadState {
        ThreadState {
            folder,
            turn_settings: settings.clone(),
            settings,
            link: Link::Detached,
            turn: Turn::Idle,
            steering: HashSet::new(),
            changes: ChangesRecorder::default(),
            file_items: HashMap::new(),
            context: None,
            discards: Vec::new(),
        }
    }

    /// The thread's Codex id, when it is loaded in the running app-server.
    pub fn native_id(&self) -> Option<&str> {
        match &self.link {
            Link::Attached { native_id, .. } => Some(native_id),
            Link::Detached | Link::Attaching | Link::Failed => None,
        }
    }

    /// The model Codex runs the thread's turns with: the setting, else the one Codex chose.
    pub fn model(&self) -> Option<&str> {
        match (&self.settings.model, &self.link) {
            (Some(model), _) => Some(model),
            (None, Link::Attached { model, .. }) => Some(model),
            (None, _) => None,
        }
    }
}

/// Whether the thread is loaded in the running app-server.
pub(super) enum Link {
    Detached,
    /// `thread/start` or `thread/resume` is out.
    Attaching,
    Attached {
        native_id: String,
        model: String,
        /// The context limit Codex loaded the thread with (D76).
        context_limit: Option<u64>,
    },
    /// Codex could not load the thread; the pause says why. Kratos does not start a new Codex
    /// thread in its place (L7), and the next use tries again.
    Failed,
}

pub(super) enum Turn {
    Idle,
    /// `turn/start` is out for `message_id`. A completion that arrives before Codex answers is
    /// kept, so the turn ends as soon as its id is known.
    Starting {
        attempt: u64,
        message_id: MessageId,
        early_completion: Option<Box<types::Turn>>,
        /// Stops asked for before the turn's id was known; answered once it is.
        stop_waiters: Vec<super::actor::Reply<()>>,
    },
    Running {
        turn_id: TurnId,
        /// Set once `turn/interrupt` is out or accepted.
        stopping: bool,
        /// Changes whenever the deadline is set again, so an earlier deadline's timer is ignored.
        deadline_epoch: u64,
    },
}

impl Turn {
    pub fn running_id(&self) -> Option<&TurnId> {
        match self {
            Turn::Running { turn_id, .. } => Some(turn_id),
            Turn::Idle | Turn::Starting { .. } => None,
        }
    }

    pub fn is_idle(&self) -> bool {
        matches!(self, Turn::Idle)
    }
}
