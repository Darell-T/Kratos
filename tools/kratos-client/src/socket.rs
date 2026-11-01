use std::io;
use std::net::TcpStream;
use std::time::Duration;

use kratos_protocol::{ClientMessage, Seq, ThreadId};
use tungstenite::client::IntoClientRequest;
use tungstenite::http::{HeaderName, HeaderValue};
use tungstenite::{Error, HandshakeError, Message, WebSocket};

use crate::ClientError;

pub struct EventSocket {
    socket: WebSocket<TcpStream>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Incoming {
    /// A message's text, exactly as the engine sent it.
    Text(String),
    /// The engine closed the socket, with the close frame's code when it sent one (1009 means
    /// "message too big").
    Closed(Option<u16>),
}

impl EventSocket {
    pub(crate) fn connect(
        authority: &str,
        headers: &[(&str, &str)],
    ) -> Result<EventSocket, ClientError> {
        let stream = TcpStream::connect(authority).map_err(|source| ClientError::Connect {
            authority: authority.to_owned(),
            source,
        })?;
        let mut request = format!("ws://{authority}/api/events").into_client_request()?;
        for (name, value) in headers {
            let name = HeaderName::try_from(*name).map_err(http_format_error)?;
            let value = HeaderValue::from_str(value).map_err(http_format_error)?;
            request.headers_mut().insert(name, value);
        }
        match tungstenite::client(request, stream) {
            Ok((socket, _response)) => Ok(EventSocket { socket }),
            Err(HandshakeError::Failure(Error::Http(response))) => {
                Err(ClientError::Handshake(response.status().as_u16()))
            }
            Err(HandshakeError::Failure(error)) => Err(error.into()),
            Err(HandshakeError::Interrupted(_)) => {
                unreachable!("a blocking stream never interrupts the handshake")
            }
        }
    }

    pub fn subscribe(&mut self, thread_id: &ThreadId, after: Seq) -> Result<(), ClientError> {
        self.send_message(&ClientMessage::Subscribe {
            thread_id: thread_id.clone(),
            after,
        })
    }

    pub fn unsubscribe(&mut self, thread_id: &ThreadId) -> Result<(), ClientError> {
        self.send_message(&ClientMessage::Unsubscribe {
            thread_id: thread_id.clone(),
        })
    }

    fn send_message(&mut self, message: &ClientMessage) -> Result<(), ClientError> {
        let text = serde_json::to_string(message).expect("a client message serializes");
        self.send_text(text)
    }

    /// Sends `text` as one text message, as is: invalid JSON and oversized messages included.
    pub fn send_text(&mut self, text: String) -> Result<(), ClientError> {
        Ok(self.socket.send(Message::text(text))?)
    }

    /// The next message or close, waiting at most `wait`. `Ok(None)` when nothing arrived in time.
    pub fn next_incoming(&mut self, wait: Duration) -> Result<Option<Incoming>, ClientError> {
        self.socket.get_ref().set_read_timeout(Some(wait))?;
        loop {
            match self.socket.read() {
                Ok(Message::Text(text)) => return Ok(Some(Incoming::Text(text.to_string()))),
                Ok(Message::Close(frame)) => {
                    let code = frame.map(|frame| u16::from(frame.code));
                    return Ok(Some(Incoming::Closed(code)));
                }
                // The engine sends only text. tungstenite answers pings by itself.
                Ok(_) => {}
                Err(Error::Io(error)) if is_timeout(&error) => return Ok(None),
                Err(Error::ConnectionClosed | Error::AlreadyClosed) => {
                    return Ok(Some(Incoming::Closed(None)));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

fn http_format_error(error: impl Into<tungstenite::http::Error>) -> Error {
    Error::HttpFormat(error.into())
}

/// A read timeout is `WouldBlock` on Unix and `TimedOut` on Windows.
fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}
