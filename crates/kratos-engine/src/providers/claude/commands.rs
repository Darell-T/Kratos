//! What handles ask of the actor. `on_command` hands each command to the method that serves it.

use super::actor::{Actor, Command};
use super::settings::check_settings;

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
                let _ = reply.send(check_settings(&settings));
            }
            Command::Access { thread_id, reply } => {
                let access = self
                    .load(&thread_id)
                    .await
                    .map(|()| self.threads[&thread_id].settings.access());
                let _ = reply.send(access);
            }
            Command::Shutdown { reply } => drop(reply.send(())),
        }
    }
}
