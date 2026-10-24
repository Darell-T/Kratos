use std::thread;
use std::time::Duration;

use kratos_protocol::{Seq, ServerMessage, ThreadEvent, ThreadId};

use crate::{Client, ClientError, EventSocket, Incoming};

/// How many times a lost connection is retried inside one [`ThreadFollower::next_event`] call, and
/// the pause before each retry: about ten seconds, which covers an engine restart.
const RECONNECT_ATTEMPTS: u32 = 100;
const RECONNECT_PAUSE: Duration = Duration::from_millis(100);

/// Follows one thread's events across disconnects and engine restarts.
///
/// It remembers the seq of the last event it returned, subscribes after it on every new socket, and
/// drops any event at or below it. Callers see each event once, in order, however often the socket
/// drops.
pub struct ThreadFollower {
    client: Client,
    thread_id: ThreadId,
    last_seq: Seq,
    socket: Option<EventSocket>,
}

impl ThreadFollower {
    pub(crate) fn new(client: Client, thread_id: ThreadId, after: Seq) -> ThreadFollower {
        ThreadFollower {
            client,
            thread_id,
            last_seq: after,
            socket: None,
        }
    }

    /// Connects and subscribes now, in one attempt, instead of on the first [`Self::next_event`].
    pub fn connect(&mut self) -> Result<(), ClientError> {
        self.socket().map(|_| ())
    }

    /// The next event after the last one returned, waiting at most `wait` for each message.
    ///
    /// Connects on first use and again after the socket drops, up to the reconnect budget (then
    /// `Err(EngineGone)`). `Err(TimedOut)` when the engine stays silent, and `Err(Engine)` when it
    /// sends an error message.
    pub fn next_event(&mut self, wait: Duration) -> Result<ThreadEvent, ClientError> {
        for _ in 0..=RECONNECT_ATTEMPTS {
            match self.read_event(wait) {
                Ok(Some(event)) => return Ok(event),
                Ok(None) => return Err(ClientError::TimedOut(wait)),
                Err(error) if error.is_connection_loss() => {
                    self.socket = None;
                    thread::sleep(RECONNECT_PAUSE);
                }
                Err(error) => return Err(error),
            }
        }
        Err(ClientError::EngineGone(self.client.authority().to_owned()))
    }

    fn read_event(&mut self, wait: Duration) -> Result<Option<ThreadEvent>, ClientError> {
        loop {
            let message = match self.socket()?.next_incoming(wait)? {
                None => return Ok(None),
                Some(Incoming::Closed(_)) => {
                    return Err(ClientError::Socket(tungstenite::Error::ConnectionClosed));
                }
                Some(Incoming::Text(text)) => {
                    serde_json::from_str(&text).map_err(|source| ClientError::Json {
                        what: "a socket message",
                        source,
                    })?
                }
            };
            match message {
                ServerMessage::Event { event } if event.seq > self.last_seq => {
                    self.last_seq = event.seq;
                    return Ok(Some(event));
                }
                // The contract tells clients to drop events they have already returned.
                ServerMessage::Event { .. } => {}
                ServerMessage::Error { message } => return Err(ClientError::Engine(message)),
            }
        }
    }

    fn socket(&mut self) -> Result<&mut EventSocket, ClientError> {
        match self.socket {
            Some(ref mut socket) => Ok(socket),
            None => {
                let mut socket = self.client.connect_events(&[])?;
                socket.subscribe(&self.thread_id, self.last_seq)?;
                Ok(self.socket.insert(socket))
            }
        }
    }
}
