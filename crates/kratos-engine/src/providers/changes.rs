//! What a turn changed, counted from its finished items the same way for every provider.

use kratos_protocol::{AgentItem, FileChangeKind, ItemStatus, TurnChanges};

/// What a turn changed, recorded from its finished items as they arrive.
#[derive(Debug, Default)]
pub struct ChangesRecorder {
    files: Vec<String>,
    edited_since_command: bool,
    ran_command: bool,
}

impl ChangesRecorder {
    pub fn record(&mut self, item: &AgentItem) {
        match item {
            AgentItem::FileChanges {
                status: ItemStatus::Completed,
                changes,
                ..
            } => {
                for change in changes {
                    self.edited(&change.path);
                    if let FileChangeKind::Update {
                        move_path: Some(path),
                    } = &change.kind
                    {
                        self.edited(path);
                    }
                }
            }
            AgentItem::Command {
                status: ItemStatus::Completed | ItemStatus::Failed,
                ..
            } => {
                self.ran_command = true;
                self.edited_since_command = false;
            }
            _ => {}
        }
    }

    fn edited(&mut self, path: &str) {
        if !self.files.iter().any(|file| file == path) {
            self.files.push(path.to_owned());
        }
        self.edited_since_command = true;
    }

    /// What the turn changed, leaving the recorder empty for the next turn.
    pub fn take(&mut self) -> TurnChanges {
        let taken = std::mem::take(self);
        TurnChanges {
            files: taken.files,
            edited_since_command: taken.edited_since_command,
            ran_command: taken.ran_command,
        }
    }
}

#[cfg(test)]
mod tests {
    use kratos_protocol::{CappedText, FileChange};

    use super::*;

    fn patch(path: &str, move_path: Option<&str>) -> AgentItem {
        AgentItem::FileChanges {
            id: "p".into(),
            status: ItemStatus::Completed,
            changes: vec![FileChange {
                path: path.into(),
                kind: FileChangeKind::Update {
                    move_path: move_path.map(str::to_owned),
                },
                diff: CappedText::new("", 0),
            }],
        }
    }

    #[test]
    fn a_turn_reports_each_edited_file_once_including_a_move_destination() {
        // Catches a recorder that misses a rename's destination, repeats a file edited twice, or
        // keeps reporting an edit as unchecked after a later command ran.
        let mut changes = ChangesRecorder::default();
        changes.record(&patch("a.rs", Some("b.rs")));
        changes.record(&patch("a.rs", None));
        changes.record(&AgentItem::Command {
            id: "c".into(),
            command: "cargo test".into(),
            cwd: Some("/w".into()),
            status: ItemStatus::Failed,
            exit_code: None,
            output: CappedText::new("", 0),
        });

        let taken = changes.take();
        assert_eq!(taken.files, ["a.rs", "b.rs"]);
        assert!(!taken.edited_since_command);
        assert!(taken.ran_command);
        assert_eq!(
            changes.take().files,
            Vec::<String>::new(),
            "taking empties the recorder"
        );
    }
}
