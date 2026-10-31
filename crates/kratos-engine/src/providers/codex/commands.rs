//! What handles ask of the actor. `on_command` hands each command to the method that serves it,
//! and the two commands that only pass a request through to Codex, listing skills and running a
//! command, are served here.

use std::time::Duration;

use kratos_codex_client::{Codex, methods, types};
use kratos_protocol::{CommandResult, Skill, ThreadId};
use serde_json::json;

use super::actor::{Actor, Command, Reply};
use super::profiles::Profile;
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
            Command::RefreshUsage { reply } => self.refresh_usage(reply),
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

    async fn skills(&mut self, thread_id: ThreadId, reply: Reply<Vec<Skill>>) {
        if let Err(error) = self.load(&thread_id).await {
            let _ = reply.send(Err(error));
            return;
        }
        let codex = match self.connection() {
            Ok(Some(codex)) => codex,
            Ok(None) => {
                self.deferred.push(Command::Skills { thread_id, reply });
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
                self.deferred.push(Command::Run {
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
