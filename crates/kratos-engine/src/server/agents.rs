//! The routes that drive a thread's agent: its messages, its turn, its settings, and what it waits
//! on the user for. Each reaches the agent of the thread's own provider (see [`Agent`]); approvals
//! and questions reach the provider of their recorded thread.

use std::time::Duration;

use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use kratos_protocol::{
    AnswerQuestion, ApprovalId, ContinuationId, DecideApproval, MessageId, MessageOrigin,
    QuestionId, QueueFull, SendMessage, ThreadId, ThreadSettings,
};
use uuid::Uuid;

use super::acting::Acting;
use super::json::read_json;
use super::threads::details;
use super::{ApiError, MessagePath, ServerState, ThreadPath, json_reply, path};
use crate::providers::{Agent, MAX_PENDING_MESSAGES, Sent};

/// How long `GET /api/usage` waits for Codex's fresh read before it answers with what it knows.
const USAGE_REFRESH_WAIT: Duration = Duration::from_secs(10);

/// `POST /api/threads/{id}/messages`: 201 with the queued message, 200 with the message the
/// client sent before under the same `messageId` (D85), 409 when that id holds another message, or
/// 429 when the queue is full, which a client may retry with the same `messageId`.
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
    let agent = state.agent(&thread_id).await?;
    let acting = Acting::of(&state, &headers).await?;
    send_to_agent(&agent, acting, thread_id, body).await
}

async fn send_to_agent(
    agent: &Agent<'_>,
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
    let SendMessage { message_id, input } = read_json(body).await?;
    let message_id = message_id.unwrap_or_else(|| MessageId::new(Uuid::new_v4().to_string()));
    Ok(sent_reply(
        agent.send(thread_id, message_id, input, origin).await?,
    ))
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

/// `POST /api/threads/{id}/messages/{messageId}/steer`: 200 with the message, now being added to
/// the running turn.
pub(super) async fn steer(
    State(state): State<ServerState>,
    ids: MessagePath,
) -> Result<Response, ApiError> {
    let (thread_id, message_id) = path(ids)?;
    let message = state
        .agent(&thread_id)
        .await?
        .steer(thread_id, message_id)
        .await?;
    Ok(json_reply(StatusCode::OK, &message))
}

/// `DELETE /api/threads/{id}/messages/{messageId}`: 200 with the removed message (D85).
pub(super) async fn remove_message(
    State(state): State<ServerState>,
    ids: MessagePath,
) -> Result<Response, ApiError> {
    let (thread_id, message_id) = path(ids)?;
    let message = state
        .agent(&thread_id)
        .await?
        .remove(thread_id, message_id)
        .await?;
    Ok(json_reply(StatusCode::OK, &message))
}

/// `POST /api/threads/{id}/messages/{messageId}/retry`: 200 with the message, queued again.
pub(super) async fn retry_message(
    State(state): State<ServerState>,
    ids: MessagePath,
) -> Result<Response, ApiError> {
    let (thread_id, message_id) = path(ids)?;
    let message = state
        .agent(&thread_id)
        .await?
        .retry(thread_id, message_id)
        .await?;
    Ok(json_reply(StatusCode::OK, &message))
}

/// `POST /api/threads/{id}/messages/{messageId}/discard`: 200 with the message, settled from
/// Codex's history and never sent again (D11).
pub(super) async fn discard_message(
    State(state): State<ServerState>,
    ids: MessagePath,
) -> Result<Response, ApiError> {
    let (thread_id, message_id) = path(ids)?;
    let message = state
        .agent(&thread_id)
        .await?
        .discard(thread_id, message_id)
        .await?;
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
    state
        .agent(&thread_id)
        .await?
        .stop(thread_id.clone())
        .await?;
    details(&state, thread_id, None).await
}

/// `POST /api/threads/{id}/resume`: 200 with the thread's details.
pub(super) async fn resume(
    State(state): State<ServerState>,
    id: ThreadPath,
) -> Result<Response, ApiError> {
    let thread_id = path(id)?;
    state
        .agent(&thread_id)
        .await?
        .resume(thread_id.clone())
        .await?;
    details(&state, thread_id, None).await
}

/// `PUT /api/threads/{id}/settings`: 200 with the thread's details. The settings apply from the
/// next turn (D87).
pub(super) async fn put_settings(
    State(state): State<ServerState>,
    id: ThreadPath,
    body: Body,
) -> Result<Response, ApiError> {
    let thread_id = path(id)?;
    let agent = state.agent(&thread_id).await?;
    let settings: ThreadSettings = read_json(body).await?;
    agent.set_settings(thread_id.clone(), settings).await?;
    details(&state, thread_id, None).await
}

/// `GET /api/threads/{id}/skills`: 200 with what the thread's agent reports for its folder. Codex
/// lists skills and says why slash commands are missing. Claude lists the slash commands its
/// session reports, which include its skills (D97).
pub(super) async fn skills(
    State(state): State<ServerState>,
    id: ThreadPath,
) -> Result<Response, ApiError> {
    let thread_id = path(id)?;
    let skills = state.agent(&thread_id).await?.skills(thread_id).await?;
    Ok(json_reply(StatusCode::OK, &skills))
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
    let agent = state.approval_agent(&approval_id).await?;
    let DecideApproval { decision } = read_json(body).await?;
    let outcome = agent.decide(approval_id, decision).await?;
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
    let acting = Acting::of(&state, &headers).await?;
    if let Some(acting) = &acting {
        let lookup = question_id.clone();
        let stored = state
            .store
            .call(move |store| store.question(&lookup))
            .await?;
        if let Some(stored) = stored {
            acting.not_on_itself(&stored.thread_id)?;
        }
    }
    let agent = state.question_agent(&question_id).await?;
    let AnswerQuestion { answers } = read_json(body).await?;
    let outcome = agent
        .answer(question_id, answers, acting.map(|acting| acting.access))
        .await?;
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
