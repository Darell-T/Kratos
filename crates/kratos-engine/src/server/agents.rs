//! The routes that drive a thread's agent: its messages, its turn, its settings, and what it waits
//! on the user for. Codex is the only agent Kratos drives so far; a thread of another provider
//! keeps R2's plain message log.

use std::time::Duration;

use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use kratos_protocol::{
    AnswerQuestion, ApprovalId, ContinuationId, DecideApproval, MessageId, MessageOrigin,
    PostMessage, Provider, QuestionId, QueueFull, SendMessage, ThreadEventBody, ThreadId,
    ThreadSettings, ThreadSkills,
};
use uuid::Uuid;

use super::acting::Acting;
use super::json::read_json;
use super::{ApiError, ServerState, json_reply};
use crate::providers::codex::{CodexProvider, MAX_PENDING_MESSAGES, ProviderError, Sent};

/// How long `GET /api/usage` waits for Codex's fresh read before it answers with what it knows.
const USAGE_REFRESH_WAIT: Duration = Duration::from_secs(10);

const NO_SLASH_COMMANDS: &str =
    "Codex's app-server protocol does not list its slash commands, so Kratos cannot offer them";

type ThreadPath = Result<Path<ThreadId>, PathRejection>;
type MessagePath = Result<Path<(ThreadId, MessageId)>, PathRejection>;

fn path<T>(path: Result<Path<T>, PathRejection>) -> Result<T, ApiError> {
    path.map(|Path(value)| value)
        .map_err(|_| ApiError::bad_request("Malformed URL path"))
}

impl ServerState {
    pub(super) fn codex(&self) -> Result<&CodexProvider, ApiError> {
        self.codex
            .as_ref()
            .map_err(|missing| ApiError::unavailable(missing.clone()))
    }
}

impl From<ProviderError> for ApiError {
    fn from(error: ProviderError) -> ApiError {
        match error {
            ProviderError::UnknownThread(_)
            | ProviderError::UnknownMessage(_)
            | ProviderError::UnknownApproval(_)
            | ProviderError::UnknownQuestion(_)
            | ProviderError::UnknownContinuation(_) => ApiError::new(StatusCode::NOT_FOUND, error),
            ProviderError::Conflict(_) => ApiError::new(StatusCode::CONFLICT, error),
            ProviderError::Unsupported(_) => ApiError::bad_request(error.to_string()),
            ProviderError::Unavailable(_) | ProviderError::Stopped => {
                ApiError::unavailable(error.to_string())
            }
            ProviderError::Store(error) => error.into(),
        }
    }
}

/// `POST /api/threads/{id}/messages`. To a Codex thread: 201 with the queued message, 200 with the
/// message the client sent before under the same `messageId` (D85), 409 when that id holds another
/// message, or 429 when the queue is full, which a client may retry with the same `messageId`. To
/// any other thread: 201 with the appended event, as R2 answers.
///
/// The thread is checked before the body is read: a post to an unknown thread answers 404 without
/// reading or size-checking its body.
pub(super) async fn post_message(
    State(state): State<ServerState>,
    headers: HeaderMap,
    id: ThreadPath,
    body: Body,
) -> Result<Response, ApiError> {
    let thread_id = path(id)?;
    let lookup = thread_id.clone();
    let provider = state
        .store
        .call(move |store| store.provider_thread(&lookup))
        .await?
        .thread
        .provider;
    let acting = Acting::of(&state, &headers).await?;
    match (provider, acting) {
        (Provider::Codex, acting) => send_to_codex(&state, acting, thread_id, body).await,
        (_, None) => log_message(&state, thread_id, body).await,
        (_, Some(_)) => Err(ApiError::bad_request(
            "Kratos runs no agent in this thread yet, so it takes no messages from other threads",
        )),
    }
}

async fn send_to_codex(
    state: &ServerState,
    acting: Option<Acting>,
    thread_id: ThreadId,
    body: Body,
) -> Result<Response, ApiError> {
    let origin = match acting {
        None => MessageOrigin::User,
        Some(acting) => {
            acting.not_on_itself(&thread_id)?;
            acting.origin()
        }
    };
    let codex = state.codex()?;
    let request: SendMessage = read_json(body).await?;
    let (message_id, input) = request.into_parts().map_err(ApiError::bad_request)?;
    let message_id = message_id.unwrap_or_else(|| MessageId::new(Uuid::new_v4().to_string()));
    Ok(sent_reply(
        codex.send(thread_id, message_id, input, origin).await?,
    ))
}

/// R2's answer for a thread whose agent Kratos does not drive yet: the message only joins the log.
async fn log_message(
    state: &ServerState,
    thread_id: ThreadId,
    body: Body,
) -> Result<Response, ApiError> {
    let PostMessage { text } = read_json(body).await?;
    let body = ThreadEventBody::UserMessage {
        text: text.into_string(),
    };
    let event = state.hub.append(thread_id, body).await?;
    Ok(json_reply(StatusCode::CREATED, &event))
}

fn sent_reply(sent: Sent) -> Response {
    match sent {
        Sent::Queued(message) => json_reply(StatusCode::CREATED, &message),
        Sent::Existing(message) => json_reply(StatusCode::OK, &message),
        Sent::Full { pending } => json_reply(
            StatusCode::TOO_MANY_REQUESTS,
            &QueueFull {
                error: format!(
                    "The thread already holds {pending} messages; send again once one has run"
                ),
                queued: pending,
                limit: MAX_PENDING_MESSAGES,
            },
        ),
    }
}

/// `GET /api/threads/{id}`: 200 with the thread, its settings, its queue, and what waits on the
/// user.
pub(super) async fn thread_details(
    State(state): State<ServerState>,
    id: ThreadPath,
) -> Result<Response, ApiError> {
    let thread_id = path(id)?;
    details(&state, thread_id).await
}

async fn details(state: &ServerState, thread_id: ThreadId) -> Result<Response, ApiError> {
    let details = state
        .store
        .call(move |store| store.thread_details(&thread_id))
        .await?;
    Ok(json_reply(StatusCode::OK, &details))
}

/// `POST /api/threads/{id}/messages/{messageId}/steer`: 200 with the message, now being added to
/// the running turn.
pub(super) async fn steer(
    State(state): State<ServerState>,
    ids: MessagePath,
) -> Result<Response, ApiError> {
    let (thread_id, message_id) = path(ids)?;
    let message = state.codex()?.steer(thread_id, message_id).await?;
    Ok(json_reply(StatusCode::OK, &message))
}

/// `DELETE /api/threads/{id}/messages/{messageId}`: 200 with the removed message (D85).
pub(super) async fn remove_message(
    State(state): State<ServerState>,
    ids: MessagePath,
) -> Result<Response, ApiError> {
    let (thread_id, message_id) = path(ids)?;
    let message = state.codex()?.remove(thread_id, message_id).await?;
    Ok(json_reply(StatusCode::OK, &message))
}

/// `POST /api/threads/{id}/messages/{messageId}/retry`: 200 with the message, queued again.
pub(super) async fn retry_message(
    State(state): State<ServerState>,
    ids: MessagePath,
) -> Result<Response, ApiError> {
    let (thread_id, message_id) = path(ids)?;
    let message = state.codex()?.retry(thread_id, message_id).await?;
    Ok(json_reply(StatusCode::OK, &message))
}

/// `POST /api/threads/{id}/messages/{messageId}/discard`: 200 with the message, settled from
/// Codex's history and never sent again (D11).
pub(super) async fn discard_message(
    State(state): State<ServerState>,
    ids: MessagePath,
) -> Result<Response, ApiError> {
    let (thread_id, message_id) = path(ids)?;
    let message = state.codex()?.discard(thread_id, message_id).await?;
    Ok(json_reply(StatusCode::OK, &message))
}

/// `POST /api/threads/{id}/stop`: 200 with the thread's details once Codex accepted the stop. The
/// turn's end arrives as an event.
pub(super) async fn stop(
    State(state): State<ServerState>,
    headers: HeaderMap,
    id: ThreadPath,
) -> Result<Response, ApiError> {
    let thread_id = path(id)?;
    if let Some(acting) = Acting::of(&state, &headers).await? {
        acting.not_on_itself(&thread_id)?;
    }
    state.codex()?.stop(thread_id.clone()).await?;
    details(&state, thread_id).await
}

/// `POST /api/threads/{id}/resume`: 200 with the thread's details.
pub(super) async fn resume(
    State(state): State<ServerState>,
    id: ThreadPath,
) -> Result<Response, ApiError> {
    let thread_id = path(id)?;
    state.codex()?.resume(thread_id.clone()).await?;
    details(&state, thread_id).await
}

/// `PUT /api/threads/{id}/settings`: 200 with the thread's details. The settings apply from the
/// next turn (D87).
pub(super) async fn put_settings(
    State(state): State<ServerState>,
    id: ThreadPath,
    body: Body,
) -> Result<Response, ApiError> {
    let thread_id = path(id)?;
    let codex = state.codex()?;
    let settings: ThreadSettings = read_json(body).await?;
    codex.set_settings(thread_id.clone(), settings).await?;
    details(&state, thread_id).await
}

/// `GET /api/threads/{id}/skills`: 200 with the skills Codex reports for the thread's folder, and
/// why slash commands are missing (D97).
pub(super) async fn skills(
    State(state): State<ServerState>,
    id: ThreadPath,
) -> Result<Response, ApiError> {
    let thread_id = path(id)?;
    let skills = state.codex()?.skills(thread_id).await?;
    let reply = ThreadSkills {
        skills,
        slash_commands_gap: Some(NO_SLASH_COMMANDS.to_owned()),
    };
    Ok(json_reply(StatusCode::OK, &reply))
}

/// `POST /api/approvals/{id}`: 200 with the approval's outcome. The first decision is recorded and
/// reaches Codex; a later one gets that outcome back (D83).
pub(super) async fn decide(
    State(state): State<ServerState>,
    headers: HeaderMap,
    id: Result<Path<ApprovalId>, PathRejection>,
    body: Body,
) -> Result<Response, ApiError> {
    let approval_id = path(id)?;
    if let Some(acting) = Acting::of(&state, &headers).await? {
        acting.may_decide()?;
        let lookup = approval_id.clone();
        let stored = state
            .store
            .call(move |store| store.approval(&lookup))
            .await?;
        if let Some(stored) = stored {
            acting.not_on_itself(&stored.thread_id)?;
        }
    }
    let codex = state.codex()?;
    let DecideApproval { decision } = read_json(body).await?;
    let outcome = codex.decide(approval_id, decision).await?;
    Ok(json_reply(StatusCode::OK, &outcome))
}

/// `POST /api/questions/{id}`: 200 with the question's outcome. As with approvals, the first answer
/// wins.
pub(super) async fn answer(
    State(state): State<ServerState>,
    headers: HeaderMap,
    id: Result<Path<QuestionId>, PathRejection>,
    body: Body,
) -> Result<Response, ApiError> {
    let question_id = path(id)?;
    if let Some(acting) = Acting::of(&state, &headers).await? {
        let lookup = question_id.clone();
        let stored = state
            .store
            .call(move |store| store.question(&lookup))
            .await?;
        if let Some(stored) = stored {
            acting.not_on_itself(&stored.thread_id)?;
        }
    }
    let codex = state.codex()?;
    let AnswerQuestion { answers } = read_json(body).await?;
    let outcome = codex.answer(question_id, answers).await?;
    Ok(json_reply(StatusCode::OK, &outcome))
}

/// `POST /api/continuations/{id}/schedule`: 200 with the continuation, now resuming on its own at
/// the reset (D41).
pub(super) async fn schedule(
    State(state): State<ServerState>,
    id: Result<Path<ContinuationId>, PathRejection>,
) -> Result<Response, ApiError> {
    let continuation_id = path(id)?;
    let scheduler = state.scheduler.as_ref().ok_or_else(|| {
        ApiError::unavailable("Kratos cannot schedule continuations without Codex".to_owned())
    })?;
    let continuation = scheduler.schedule(continuation_id).await?;
    Ok(json_reply(StatusCode::OK, &continuation))
}

/// `GET /api/usage`: 200 with each account's usage, read fresh from Codex when it answers in time
/// (D38). Context use is a thread's, in its turn events, never this report (D76).
pub(super) async fn usage(State(state): State<ServerState>) -> Response {
    if let Ok(codex) = state.codex() {
        // A failed or slow read leaves the last snapshot, which says so in its allowance.
        let _ = tokio::time::timeout(USAGE_REFRESH_WAIT, codex.refresh_usage()).await;
    }
    json_reply(StatusCode::OK, &state.usage.report())
}
