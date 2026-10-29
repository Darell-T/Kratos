use axum::body::Body;
use futures_util::StreamExt;
use serde::de::DeserializeOwned;
use serde_json::error::Category;

use super::{ApiError, MAX_BODY_BYTES};

/// Reads a request body of at most [`MAX_BODY_BYTES`] and parses it into `T`. Answers 413 "Request
/// body is too large", 400 "Request body must be valid JSON" for a syntax error or an empty body,
/// and 400 naming the field for valid JSON of the wrong shape. `Content-Type` is ignored.
pub async fn read_json<T: DeserializeOwned>(body: Body) -> Result<T, ApiError> {
    let bytes = read_capped(body).await?;
    parse_json(&bytes).map_err(|error| match error {
        JsonError::Invalid => ApiError::bad_request("Request body must be valid JSON"),
        JsonError::Shape(message) => ApiError::bad_request(message),
    })
}

/// Reads `body` to its end and throws it away, for a request refused before its body matters. See
/// [`read_capped`] for why the body is read at all.
pub async fn discard(body: Body) {
    let mut chunks = body.into_data_stream();
    while let Some(Ok(_)) = chunks.next().await {}
}

/// An oversized body is read to its end and thrown away before the 413 goes out. Answering at once
/// would let the connection close while the client is still writing, and on Windows that resets the
/// connection, so the client never sees the status. Memory stays constant; the cost is time spent
/// on bytes that are discarded.
async fn read_capped(body: Body) -> Result<Vec<u8>, ApiError> {
    let mut chunks = body.into_data_stream();
    let mut kept = Vec::new();
    let mut total = 0usize;
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| ApiError::bad_request("Request body could not be read"))?;
        total += chunk.len();
        if total <= MAX_BODY_BYTES {
            kept.extend_from_slice(&chunk);
        }
    }
    if total > MAX_BODY_BYTES {
        return Err(ApiError::body_too_large());
    }
    Ok(kept)
}

#[derive(Debug, PartialEq, Eq)]
pub enum JsonError {
    /// Not JSON at all, or JSON with something after the value.
    Invalid,
    /// JSON of the wrong shape. The text names the field, for example `text: must not be blank`.
    Shape(String),
}

pub fn parse_json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, JsonError> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = serde_path_to_error::deserialize(&mut deserializer).map_err(json_error)?;
    deserializer.end().map_err(|_| JsonError::Invalid)?;
    Ok(value)
}

fn json_error(error: serde_path_to_error::Error<serde_json::Error>) -> JsonError {
    match error.inner().classify() {
        Category::Data => JsonError::Shape(describe(&error)),
        Category::Io | Category::Syntax | Category::Eof => JsonError::Invalid,
    }
}

/// serde_json ends a message with " at line L column C", which says nothing a client can use
/// about a request this small.
fn describe(error: &serde_path_to_error::Error<serde_json::Error>) -> String {
    let message = error.inner().to_string();
    let message = message
        .rsplit_once(" at line ")
        .map_or(message.as_str(), |(text, _position)| text);
    let path = error.path().to_string();
    if path == "." {
        message.to_owned()
    } else {
        format!("{path}: {message}")
    }
}
