//! The worktrees of a project's repository, which a new thread may work in.

use std::path::{Path, PathBuf};

use super::{PlaceError, Worktrees, git, resolved, text};

/// A worktree git lists, with its top folder's links resolved.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Listed {
    pub path: String,
    /// `None` when no branch is checked out.
    pub branch: Option<String>,
}

impl Worktrees {
    /// Every worktree of the repository whose top folder is `project`, other than the project
    /// folder itself. A worktree whose folder is gone is left out, since no thread can work there.
    pub(crate) async fn list(&self, project: &Path) -> Result<Vec<Listed>, PlaceError> {
        let repository = self.repository_of(project).await?;
        let args = ["worktree", "list", "--porcelain", "-z"];
        let listed = self.git.run(&repository.top, &args, git::QUICK).await?;
        let mut worktrees = Vec::new();
        for (path, branch) in entries(&listed) {
            let Ok(path) = resolved(&path).await else {
                continue;
            };
            if path != repository.top {
                worktrees.push(Listed {
                    path: text(&path)?,
                    branch,
                });
            }
        }
        Ok(worktrees)
    }
}

/// The folder and branch of each worktree in `git worktree list --porcelain -z` output, leaving
/// out a bare repository and a worktree git marks prunable, whose folder is gone. Each field ends
/// with a NUL, and an empty field ends a worktree.
fn entries(listed: &str) -> Vec<(PathBuf, Option<String>)> {
    let mut entries = Vec::new();
    let mut current: Option<(PathBuf, Option<String>)> = None;
    for field in listed.split('\0') {
        if let Some(path) = field.strip_prefix("worktree ") {
            current = Some((PathBuf::from(path), None));
        } else if let Some(branch) = field.strip_prefix("branch refs/heads/") {
            if let Some((_, current_branch)) = &mut current {
                *current_branch = Some(branch.to_owned());
            }
        } else if field == "bare" || field.starts_with("prunable") {
            current = None;
        } else if field.is_empty() {
            entries.extend(current.take());
        }
    }
    entries.extend(current);
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_worktree_git_lists_gives_its_folder_and_branch() {
        let listed = "worktree C:/repo\0HEAD 1111\0branch refs/heads/main\0\0\
                      worktree C:/repo/feature\0HEAD 2222\0branch refs/heads/feature/login\0\0\
                      worktree C:/detached\0HEAD 3333\0detached\0\0\
                      worktree C:/gone\0HEAD 4444\0branch refs/heads/gone\0prunable gitdir file points to non-existent location\0\0";

        assert_eq!(
            entries(listed),
            [
                (PathBuf::from("C:/repo"), Some("main".to_owned())),
                (
                    PathBuf::from("C:/repo/feature"),
                    Some("feature/login".to_owned())
                ),
                (PathBuf::from("C:/detached"), None),
            ]
        );
    }

    #[test]
    fn a_bare_repository_is_not_a_worktree() {
        let listed = "worktree C:/repo.git\0bare\0\0worktree C:/work\0HEAD 1111\0detached\0\0";

        assert_eq!(entries(listed), [(PathBuf::from("C:/work"), None)]);
    }
}
