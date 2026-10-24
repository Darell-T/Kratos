//! HTTP written and read by hand, so a request reaches the engine exactly as a lane wrote it.
//!
//! An HTTP library would repair a foreign Host, an oversized body, or a missing header, and those
//! are the requests the engine's checks exist to refuse.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::Value;

use crate::ClientError;

/// A lane's largest request is a few MiB sent over loopback, so this is only a guard against a
/// hung engine.
const IO_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct ApiRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
}

impl ApiRequest {
    pub fn new(method: &str, path: &str) -> ApiRequest {
        ApiRequest {
            method: method.to_owned(),
            path: path.to_owned(),
            headers: Vec::new(),
            body: None,
        }
    }

    /// Adds a header exactly as given. The value is not checked, so a lane can send a malformed
    /// one.
    pub fn header(mut self, name: &str, value: &str) -> ApiRequest {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn body(mut self, body: Vec<u8>) -> ApiRequest {
        self.body = Some(body);
        self
    }

    /// `Connection: close` lets the client read the reply to the end of the stream.
    pub(crate) fn encode(&self, authority: &str) -> Vec<u8> {
        let names_header = |name: &str| {
            self.headers
                .iter()
                .any(|(given, _)| given.eq_ignore_ascii_case(name))
        };
        let mut text = format!("{} {} HTTP/1.1\r\n", self.method, self.path);
        for (name, value) in &self.headers {
            text.push_str(&format!("{name}: {value}\r\n"));
        }
        if !names_header("Host") {
            text.push_str(&format!("Host: {authority}\r\n"));
        }
        if let Some(body) = &self.body
            && !names_header("Content-Length")
        {
            text.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        if !names_header("Connection") {
            text.push_str("Connection: close\r\n");
        }
        text.push_str("\r\n");
        let mut bytes = text.into_bytes();
        bytes.extend_from_slice(self.body.as_deref().unwrap_or_default());
        bytes
    }
}

/// A body that is not JSON is kept as a JSON string, and an empty one is `null`.
#[derive(Clone, Debug, PartialEq)]
pub struct ApiReply {
    pub status: u16,
    pub body: Value,
}

impl ApiReply {
    pub fn to_json_line(&self) -> String {
        format!("{{\"status\":{},\"body\":{}}}", self.status, self.body)
    }
}

pub(crate) fn exchange(authority: &str, bytes: &[u8]) -> Result<ApiReply, ClientError> {
    let mut stream = TcpStream::connect(authority).map_err(|source| ClientError::Connect {
        authority: authority.to_owned(),
        source,
    })?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    // The engine may answer, a 413 for example, before it has read the whole body. A client that
    // stopped at the failed write would never see the status, so the reply is read either way.
    let written = stream.write_all(bytes);
    let mut reply = Vec::new();
    let read = stream.read_to_end(&mut reply);
    parse_reply(&reply).map_err(|unparsable| match written.err().or(read.err()) {
        Some(io_error) => ClientError::Io(io_error),
        None => unparsable,
    })
}

fn parse_reply(bytes: &[u8]) -> Result<ApiReply, ClientError> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers);
    match (response.parse(bytes), response.code) {
        (Ok(httparse::Status::Complete(head_length)), Some(status)) => Ok(ApiReply {
            status,
            body: json_or_text(&bytes[head_length..]),
        }),
        (Err(error), _) => Err(ClientError::Reply(error.to_string())),
        _ => Err(ClientError::Reply(
            "the reply ended inside its headers".into(),
        )),
    }
}

fn json_or_text(body: &[u8]) -> Value {
    if body.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(body).into_owned()))
}
