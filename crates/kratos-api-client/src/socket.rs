use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use kratos_protocol::{ClientMessage, Seq, ThreadId, routes};
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
        let mut request = format!("ws://{authority}{}", routes::EVENTS).into_client_request()?;
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
        read_incoming(&mut self.socket, wait, |socket, timeout| {
            socket.get_ref().set_read_timeout(Some(timeout))
        })
    }
}

fn read_incoming<S: Read + Write>(
    socket: &mut WebSocket<S>,
    wait: Duration,
    mut set_timeout: impl FnMut(&mut WebSocket<S>, Duration) -> io::Result<()>,
) -> Result<Option<Incoming>, ClientError> {
    if wait.is_zero() {
        return Ok(None);
    }
    let deadline = Instant::now().checked_add(wait);
    let mut remaining = wait;
    loop {
        set_timeout(socket, remaining)?;
        if let ControlFlow::Break(outcome) = classify_read(socket.read())? {
            return Ok(outcome);
        }
        let Some(next) = remaining_wait(deadline, wait) else {
            return Ok(None);
        };
        remaining = next;
    }
}

fn classify_read(
    result: Result<Message, Error>,
) -> Result<ControlFlow<Option<Incoming>>, ClientError> {
    match result {
        Ok(Message::Text(text)) => Ok(ControlFlow::Break(Some(Incoming::Text(text.to_string())))),
        Ok(Message::Close(frame)) => {
            let code = frame.map(|frame| u16::from(frame.code));
            Ok(ControlFlow::Break(Some(Incoming::Closed(code))))
        }
        // The engine sends only text. tungstenite answers pings by itself.
        Ok(_) => Ok(ControlFlow::Continue(())),
        // Linux socket reads with SO_RCVTIMEO do not auto-restart on signals (EINTR).
        Err(Error::Io(error)) if error.kind() == io::ErrorKind::Interrupted => {
            Ok(ControlFlow::Continue(()))
        }
        Err(Error::Io(error)) if is_timeout(&error) => Ok(ControlFlow::Break(None)),
        Err(Error::ConnectionClosed | Error::AlreadyClosed) => {
            Ok(ControlFlow::Break(Some(Incoming::Closed(None))))
        }
        Err(error) => Err(error.into()),
    }
}

fn remaining_wait(deadline: Option<Instant>, fallback: Duration) -> Option<Duration> {
    let Some(deadline) = deadline else {
        return Some(fallback);
    };
    let left = deadline.saturating_duration_since(Instant::now());
    (!left.is_zero()).then_some(left)
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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use tungstenite::protocol::Role;

    use super::*;

    enum ReadStep {
        Interrupted,
        Bytes(Vec<u8>),
        PauseThenInterrupted(Duration),
    }

    struct ScriptedStream {
        steps: VecDeque<ReadStep>,
    }

    impl Read for ScriptedStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.steps.pop_front() {
                Some(ReadStep::Interrupted) => Err(io::Error::from(io::ErrorKind::Interrupted)),
                Some(ReadStep::PauseThenInterrupted(pause)) => {
                    std::thread::sleep(pause);
                    Err(io::Error::from(io::ErrorKind::Interrupted))
                }
                Some(ReadStep::Bytes(bytes)) => {
                    let count = bytes.len().min(buf.len());
                    buf[..count].copy_from_slice(&bytes[..count]);
                    if count < bytes.len() {
                        self.steps
                            .push_front(ReadStep::Bytes(bytes[count..].to_vec()));
                    }
                    Ok(count)
                }
                None => Err(io::Error::other(
                    "scripted stream exhausted before deadline",
                )),
            }
        }
    }

    impl Write for ScriptedStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn server_text_frame(text: &str) -> Vec<u8> {
        let mut frame = vec![0x81, u8::try_from(text.len()).expect("short test frame")];
        frame.extend_from_slice(text.as_bytes());
        frame
    }

    #[test]
    fn next_incoming_retries_after_interrupted_read_before_and_mid_frame() {
        let frame = server_text_frame("hello");
        let stream = ScriptedStream {
            steps: VecDeque::from([
                ReadStep::Interrupted,
                ReadStep::Bytes(frame[..2].to_vec()),
                ReadStep::Interrupted,
                ReadStep::Bytes(frame[2..].to_vec()),
            ]),
        };
        let mut socket = WebSocket::from_raw_socket(stream, Role::Client, None);

        let received = read_incoming(&mut socket, Duration::from_secs(1), |_, timeout| {
            assert!(!timeout.is_zero());
            Ok(())
        })
        .unwrap();

        assert_eq!(received, Some(Incoming::Text("hello".to_owned())));
    }

    #[test]
    fn next_incoming_stops_at_deadline_across_repeated_interrupts() {
        let mut steps = VecDeque::from([ReadStep::Interrupted]);
        steps.extend((0..100).map(|_| ReadStep::PauseThenInterrupted(Duration::from_millis(15))));
        let stream = ScriptedStream { steps };
        let mut socket = WebSocket::from_raw_socket(stream, Role::Client, None);
        let mut seen_timeouts = Vec::new();
        let started = Instant::now();

        let received = read_incoming(&mut socket, Duration::from_millis(60), |_, timeout| {
            assert!(!timeout.is_zero());
            seen_timeouts.push(timeout);
            Ok(())
        })
        .unwrap();

        assert_eq!(received, None);
        assert!(seen_timeouts.len() >= 2, "{seen_timeouts:?}");
        assert!(
            seen_timeouts.last() < seen_timeouts.first(),
            "{seen_timeouts:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn next_incoming_with_zero_wait_returns_none_without_setting_timeout() {
        let stream = ScriptedStream {
            steps: VecDeque::new(),
        };
        let mut socket = WebSocket::from_raw_socket(stream, Role::Client, None);
        let mut calls = 0;

        let received = read_incoming(&mut socket, Duration::ZERO, |_, _| {
            calls += 1;
            Ok(())
        })
        .unwrap();

        assert_eq!(received, None);
        assert_eq!(calls, 0);
    }
}
