use std::ops::ControlFlow;

use axum::extract::State;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::response::{IntoResponse, Response};
use kratos_protocol::{ClientMessage, Seq, ServerMessage, ThreadEvent, ThreadId};
use kratos_store::StoreError;
use tokio_stream::{StreamExt, StreamMap};

use super::json::{JsonError, parse_json};
use super::{ApiError, MAX_SOCKET_MESSAGE_BYTES, ServerState};
use crate::hub::{EventPages, FollowError, Hub};

/// What the WebSocket library buffers for one message. Clients are held to
/// [`MAX_SOCKET_MESSAGE_BYTES`], but a message between the two limits is read in full and then
/// refused. Refusing at its first frame would leave unread bytes in the socket, and on Windows
/// closing a socket with unread bytes resets the connection and destroys the close frame.
const BUFFERED_MESSAGE_BYTES: usize = 1024 * 1024;

/// `GET /api/events`. A request without the upgrade headers answers 404.
pub(super) async fn events(
    State(state): State<ServerState>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let Ok(upgrade) = upgrade else {
        return ApiError::not_found().into_response();
    };
    upgrade
        .max_message_size(BUFFERED_MESSAGE_BYTES)
        .max_frame_size(BUFFERED_MESSAGE_BYTES)
        .on_upgrade(move |socket| Connection::new(socket, state.hub).run())
}

struct Connection {
    socket: WebSocket,
    hub: Hub,
    follows: StreamMap<ThreadId, EventPages>,
}

impl Connection {
    fn new(socket: WebSocket, hub: Hub) -> Connection {
        Connection {
            socket,
            hub,
            follows: StreamMap::new(),
        }
    }

    /// Client messages and pages interleave: the loop listens to the client again after every page.
    /// While a send waits on a client that stopped reading, only this socket waits.
    async fn run(mut self) {
        loop {
            let flow = tokio::select! {
                frame = self.socket.recv() => self.on_frame(frame).await,
                Some((thread_id, page)) = self.follows.next(), if !self.follows.is_empty() => {
                    self.on_page(thread_id, page).await
                }
            };
            if flow.is_break() {
                return;
            }
        }
    }

    /// Text and binary frames both carry JSON.
    async fn on_frame(&mut self, frame: Option<Result<Message, axum::Error>>) -> ControlFlow<()> {
        match frame {
            Some(Ok(Message::Text(text))) => self.on_message(text.as_bytes()).await,
            Some(Ok(Message::Binary(bytes))) => self.on_message(&bytes).await,
            // The WebSocket library answers pings itself.
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => ControlFlow::Continue(()),
            Some(Ok(Message::Close(_)) | Err(_)) | None => ControlFlow::Break(()),
        }
    }

    /// A client message. A message over 64 KiB closes the socket with 1009. One that does not
    /// parse, or names a thread that does not exist, gets an error message, and the socket stays
    /// open.
    async fn on_message(&mut self, message: &[u8]) -> ControlFlow<()> {
        if message.len() > MAX_SOCKET_MESSAGE_BYTES {
            self.close_too_big().await;
            return ControlFlow::Break(());
        }
        match parse_json::<ClientMessage>(message) {
            Err(error) => self.send_error(client_message_error(error)).await,
            Ok(ClientMessage::Subscribe { thread_id, after }) => {
                self.subscribe(thread_id, after).await
            }
            Ok(ClientMessage::Unsubscribe { thread_id }) => {
                self.follows.remove(&thread_id);
                ControlFlow::Continue(())
            }
        }
    }

    async fn subscribe(&mut self, thread_id: ThreadId, after: Seq) -> ControlFlow<()> {
        match self.hub.follow(thread_id.clone(), after).await {
            Ok(pages) => {
                self.follows.insert(thread_id, pages);
                ControlFlow::Continue(())
            }
            Err(error) => self.send_error(follow_error_message(error)).await,
        }
    }

    /// A page that could not be read ends that follow and tells the client "Internal server error".
    async fn on_page(
        &mut self,
        thread_id: ThreadId,
        page: Result<Vec<ThreadEvent>, StoreError>,
    ) -> ControlFlow<()> {
        match page {
            Ok(events) => {
                for event in events {
                    self.send(&ServerMessage::Event { event }).await?;
                }
                ControlFlow::Continue(())
            }
            Err(error) => {
                self.follows.remove(&thread_id);
                self.send_error(ApiError::from(error).message().to_owned())
                    .await
            }
        }
    }

    async fn send_error(&mut self, message: String) -> ControlFlow<()> {
        self.send(&ServerMessage::Error { message }).await
    }

    async fn send(&mut self, message: &ServerMessage) -> ControlFlow<()> {
        let text = serde_json::to_string(message).expect("a server message serializes");
        match self.socket.send(Message::Text(text.into())).await {
            Ok(()) => ControlFlow::Continue(()),
            Err(_) => ControlFlow::Break(()),
        }
    }

    async fn close_too_big(&mut self) {
        let frame = CloseFrame {
            code: close_code::SIZE,
            reason: "Message too big".into(),
        };
        let _ = self.socket.send(Message::Close(Some(frame))).await;
    }
}

fn client_message_error(error: JsonError) -> String {
    match error {
        JsonError::Invalid => "Message must be valid JSON".to_owned(),
        JsonError::Shape(message) => message,
    }
}

fn follow_error_message(error: FollowError) -> String {
    match error {
        FollowError::Store(error) => ApiError::from(error).message().to_owned(),
        cursor_ahead @ FollowError::CursorAhead { .. } => cursor_ahead.to_string(),
    }
}
