//! Child processes whose whole process tree ends with their owner.
//!
//! An agent CLI starts tools of its own, and killing only the CLI leaves those running: the Claude
//! prototype left four. [`OwnedChild::spawn`] starts the child so that dropping the [`OwnedChild`]
//! ends every process the child started too.
//!
//! - Windows: the child starts suspended, joins a new job object marked kill-on-close, and only then
//!   runs, so even its first grandchild is in the job. The job forbids breaking away. Closing the
//!   job's handle ends every process in it, and Windows closes a process's handles when it dies, so
//!   the tree also ends when the engine crashes.
//! - Unix: the child leads a new process group, and dropping kills the group. A process that leaves
//!   the group, or an engine killed with SIGKILL, leaves processes behind. Proving macOS and Linux
//!   isolation is later work (D17).

use std::io;
use std::process::ExitStatus;

use tokio::process::{Child, Command};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix::Tree;
#[cfg(windows)]
use windows::Tree;

/// A child process and its process tree. Dropping it ends the tree.
#[derive(Debug)]
pub struct OwnedChild {
    // Declared first so it drops first: the tree ends before tokio stops tracking the child.
    tree: Tree,
    child: Child,
}

impl OwnedChild {
    /// Starts `command`. On Windows this sets the command's creation flags to start it suspended and
    /// without a console window, replacing any flags the caller set.
    pub fn spawn(mut command: Command) -> io::Result<OwnedChild> {
        let (tree, child) = Tree::spawn(&mut command)?;
        Ok(OwnedChild { tree, child })
    }

    /// The child process, for its pipes and its exit status. Killing it through this handle ends
    /// only the child; [`OwnedChild::end_tree`] ends the tree.
    pub fn child(&mut self) -> &mut Child {
        &mut self.child
    }

    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// Ends every process in the tree now. The child's exit status stays available to
    /// [`OwnedChild::wait`]. Ending a tree that already ended is not an error.
    pub fn end_tree(&mut self) -> io::Result<()> {
        self.tree.end()
    }

    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        // Nothing can report an error from a drop, and a tree that already ended is the common case.
        let _ = self.tree.end();
    }
}
