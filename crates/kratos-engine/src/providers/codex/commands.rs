//! What handles ask of the actor.

use std::time::Duration;

use kratos_codex_client::{Codex, RequestError, methods, types};
use kratos_protocol::{CommandResult, Message, MessageId, MessageStatus, Skill, ThreadId};
use serde_json::json;

use super::actor::{Actor, Command, Deferred, Done, Reply};
use super::profiles::Profile;
use super::threads::Turn;
use super::turns::{Delivery, Stopper};
use super::{ProviderError, wire};

impl Actor {
    // #lizard forgives: one arm per command, each a call.
    pub(super) async fn on_command(&mut self, command: Command) {
        match command {
            Command::Send {
                thread_id,
                message_id,
                input,
                origin,
                reply,
            } => {
                let sent = self.send(&thread_id, message_id, input, origin).await;
                let _ = reply.send(sent);
                self.dispatch(&thread_id).await;
            }
            Command::Steer {
                thread_id,
                message_id,
                reply,
            } => {
                let steered = self.steer(&thread_id, message_id).await;
                let _ = reply.send(steered);
            }
            Command::Remove {
                thread_id,
                message_id,
                reply,
            } => {
                let removed = self.remove(&thread_id, message_id).await;
                let _ = reply.send(removed);
                self.dispatch(&thread_id).await;
            }
            Command::Retry {
                thread_id,
                message_id,
                reply,
            } => {
                let retried = self.retry(&thread_id, message_id).await;
                let _ = reply.send(retried);
                self.dispatch(&thread_id).await;
            }
            Command::Discard {
                thread_id,
                message_id,
                reply,
            } => self.discard(thread_id, message_id, reply).await,
            Command::Stop { thread_id, reply } => self.stop(&thread_id, reply).await,
            Command::Resume { thread_id, reply } => {
                let resumed = self.resume(&thread_id).await;
                let _ = reply.send(resumed);
                self.dispatch(&thread_id).await;
            }
            Command::SetSettings {
                thread_id,
                settings,
                reply,
            } => {
                let set = self.set_settings(&thread_id, settings).await;
                let _ = reply.send(set);
            }
            Command::CheckSettings { settings, reply } => {
                let _ = reply.send(self.check_settings(&settings, None));
            }
            Command::Decide {
                approval_id,
                decision,
                reply,
            } => {
                let decided = self.decide(approval_id, decision).await;
                let _ = reply.send(decided);
            }
            Command::Answer {
                question_id,
                answers,
                reply,
            } => {
                let answered = self.answer(question_id, answers).await;
                let _ = reply.send(answered);
            }
            Command::Skills { thread_id, reply } => self.skills(thread_id, reply).await,
            Command::Run {
                thread_id,
                command,
                timeout,
                reply,
            } => self.run_command(thread_id, command, timeout, reply).await,
            Command::RefreshUsage { reply } => match self.connection() {
                Ok(Some(codex)) => self.read_usage(codex, Some(reply)),
                Ok(None) => self.deferred.push(Deferred::RefreshUsage { reply }),
                Err(error) => drop(reply.send(Err(error))),
            },
            Command::Continue {
                thread_id,
                continuation_id,
                reply,
            } => {
                let continued = self.continue_thread(&thread_id, continuation_id).await;
                let _ = reply.send(continued);
                self.dispatch(&thread_id).await;
            }
            Command::Shutdown { reply } => drop(reply.send(())),
        }
    }

    /// Adds a queued message to the running turn. Answers once Codex has the request; the outcome
    /// arrives as the message's status.
    async fn steer(
        &mut self,
        thread_id: &ThreadId,
        message_id: MessageId,
    ) -> Result<Message, ProviderError> {
        self.load(thread_id).await?;
        let thread = &self.threads[thread_id];
        let (
            Turn::Running {
                turn_id,
                stopping: false,
                ..
            },
            Some(native_id),
        ) = (&thread.turn, thread.native_id())
        else {
            return Err(ProviderError::Conflict(
                "Only a running turn can be steered".into(),
            ));
        };
        let (turn_id, native_id) = (turn_id.clone(), native_id.to_owned());
        let message = self.message(thread_id, &message_id).await?;
        if message.status != MessageStatus::Queued {
            return Err(ProviderError::Conflict(format!(
                "Only a queued message can steer the turn; message {message_id} is not queued"
            )));
        }
        if message.origin.limit().is_some() {
            return Err(ProviderError::Conflict(
                "A message from another thread runs in a turn of its own, with no more than that thread's permission".into(),
            ));
        }
        let codex = self
            .connection()?
            .ok_or_else(|| ProviderError::Unavailable("Codex is starting".into()))?;
        let submitting = self
            .write(thread_id, {
                let message_id = message_id.clone();
                move |write| write.set_message_status(&message_id, MessageStatus::Submitting)
            })
            .await?;
        self.threads
            .get_mut(thread_id)
            .expect("loaded above")
            .steering
            .insert(message_id.clone());
        let params = types::TurnSteerParams {
            additional_context: None,
            client_user_message_id: Some(message_id.as_str().to_owned()),
            expected_turn_id: turn_id.as_str().to_owned(),
            input: wire::user_input(&message),
            responsesapi_client_metadata: None,
            thread_id: native_id,
        };
        let thread_id = thread_id.clone();
        self.spawn(async move {
            let result = codex
                .request::<methods::TurnSteer>(&params)
                .await
                .map(|_| ());
            Done::Steered {
                thread_id,
                message_id,
                result,
            }
        });
        Ok(submitting)
    }

    pub(super) async fn on_steered(
        &mut self,
        thread_id: ThreadId,
        message_id: MessageId,
        result: Result<(), RequestError>,
    ) {
        let turn_id = self
            .threads
            .get(&thread_id)
            .and_then(|thread| thread.turn.running_id().cloned());
        match (result, turn_id) {
            (Ok(()), Some(turn_id)) => {
                let _ = self
                    .write(&thread_id, {
                        let message_id = message_id.clone();
                        move |write| {
                            write.set_message_status(
                                &message_id,
                                MessageStatus::Accepted { turn_id },
                            )
                        }
                    })
                    .await;
            }
            (Err(RequestError::Rpc { code, .. }), _)
                if kratos_codex_client::rejected_before_execution(code) =>
            {
                // Refused before it ran: the message goes back to the queue.
                let _ = self
                    .write(&thread_id, {
                        let message_id = message_id.clone();
                        move |write| write.set_message_status(&message_id, MessageStatus::Queued)
                    })
                    .await;
            }
            (result, _) => {
                let error = result.err().map_or_else(
                    || "The turn ended before Codex confirmed the steer".to_owned(),
                    |error| error.to_string(),
                );
                // The message stays in `steering` until the history check settles it.
                self.check_delivery(&thread_id, &message_id, Delivery::Steer, error);
                return;
            }
        }
        if let Some(thread) = self.threads.get_mut(&thread_id) {
            thread.steering.remove(&message_id);
        }
        self.dispatch(&thread_id).await;
    }

    async fn stop(&mut self, thread_id: &ThreadId, reply: Reply<()>) {
        if let Err(error) = self.load(thread_id).await {
            let _ = reply.send(Err(error));
            return;
        }
        let thread = self.threads.get_mut(thread_id).expect("loaded above");
        match &mut thread.turn {
            Turn::Idle => drop(reply.send(Ok(()))),
            // Stopped as soon as its id is known.
            Turn::Starting { stop_waiters, .. } => stop_waiters.push(reply),
            Turn::Running { .. } => self.interrupt(thread_id, Stopper::Users(vec![reply])),
        }
    }

    async fn skills(&mut self, thread_id: ThreadId, reply: Reply<Vec<Skill>>) {
        if let Err(error) = self.load(&thread_id).await {
            let _ = reply.send(Err(error));
            return;
        }
        let codex = match self.connection() {
            Ok(Some(codex)) => codex,
            Ok(None) => {
                self.deferred.push(Deferred::Skills { thread_id, reply });
                return;
            }
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let folder = self.threads[&thread_id].folder.clone();
        tokio::spawn(async move {
            let _ = reply.send(list_skills(&codex, folder).await);
        });
    }

    async fn run_command(
        &mut self,
        thread_id: ThreadId,
        command: Vec<String>,
        timeout: Duration,
        reply: Reply<CommandResult>,
    ) {
        if let Err(error) = self.load(&thread_id).await {
            let _ = reply.send(Err(error));
            return;
        }
        let codex = match self.connection() {
            Ok(Some(codex)) => codex,
            Ok(None) => {
                self.deferred.push(Deferred::Run {
                    thread_id,
                    command,
                    timeout,
                    reply,
                });
                return;
            }
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let thread = &self.threads[&thread_id];
        let profile = Profile::for_settings(&thread.settings).id().to_owned();
        let folder = thread.folder.clone();
        tokio::spawn(async move {
            let _ = reply.send(Ok(exec(&codex, command, folder, profile, timeout).await));
        });
    }
}

async fn list_skills(codex: &Codex, folder: String) -> Result<Vec<Skill>, ProviderError> {
    let params = types::SkillsListParams {
        cwds: vec![folder],
        force_reload: Some(false),
    };
    let listed = codex
        .request::<methods::SkillsList>(&params)
        .await
        .map_err(|error| {
            ProviderError::Unavailable(format!("Codex did not list skills: {error}"))
        })?;
    Ok(listed
        .data
        .into_iter()
        .flat_map(|entry| entry.skills)
        .map(|skill| Skill {
            name: skill.name,
            description: skill.description,
            path: skill.path.0,
            enabled: skill.enabled,
        })
        .collect())
}

/// Runs `command` through Codex's command runner, in `folder`, inside `profile`'s sandbox.
async fn exec(
    codex: &Codex,
    command: Vec<String>,
    folder: String,
    profile: String,
    timeout: Duration,
) -> CommandResult {
    let mut params: types::CommandExecParams = wire::required(json!({ "command": command }));
    params.cwd = Some(folder);
    params.permission_profile = Some(profile);
    params.timeout_ms = Some(i64::try_from(timeout.as_millis()).unwrap_or(i64::MAX));
    // The runner's own deadline answers first; this one only bounds a Codex that never answers.
    let deadline = timeout + Duration::from_secs(10);
    let result = codex
        .request_within::<methods::CommandExec>(&params, deadline)
        .await;
    wire::command_outcome(result)
}
