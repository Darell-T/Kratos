//! Child processes whose whole process tree ends with their owner.
//!
//! An agent CLI starts tools of its own, and killing only the CLI leaves those running: the Claude
//! prototype left four. [`OwnedChild::spawn`] starts the child so that dropping the [`OwnedChild`]
//! ends every process the child started too.
//!
//! - Windows: the child starts suspended, joins a new job object marked kill-on-close, and only
//!   then
//!   runs, so even its first grandchild is in the job. The job forbids breaking away. Closing the
//!   job's handle ends every process in it, and Windows closes a process's handles when it dies, so
//!   the tree also ends when the engine crashes.
//! - Unix: the child leads a new process group, and dropping kills the group. The group is
//!   signalled at most once, and only while its leader is unreaped, because a reaped leader's group
//!   id can pass to an unrelated program. A process that leaves the group, or an engine killed with
//!   SIGKILL, leaves processes behind. Proving macOS and Linux isolation is later work (D17).

use std::io;
use std::process::ExitStatus;

use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix::Tree;
#[cfg(windows)]
use windows::Tree;

#[cfg(windows)]
pub use windows::contain_current_process;

/// A child process and its process tree. Dropping it ends the tree.
#[derive(Debug)]
pub struct OwnedChild {
    // Declared first so it drops first: the tree ends before tokio stops tracking the child.
    tree: Tree,
    child: Child,
}

impl OwnedChild {
    /// Starts `command`. On Windows this sets the command's creation flags to start it suspended
    /// and without a console window, replacing any flags the caller set.
    pub fn spawn(mut command: Command) -> io::Result<OwnedChild> {
        let (tree, child) = Tree::spawn(&mut command)?;
        Ok(OwnedChild { tree, child })
    }

    /// Takes the child's stdin pipe, if stdin was piped.
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    /// Takes the child's stdout pipe, if stdout was piped.
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    /// Takes the child's stderr pipe, if stderr was piped.
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    /// Ends every process in the tree now. The child's exit status stays available to
    /// [`OwnedChild::wait`]. Ending a tree that already ended is not an error.
    ///
    /// On Unix, the group is signalled at most once and never after its leader has been reaped,
    /// so a later call does nothing.
    pub fn end_tree(&mut self) -> io::Result<()> {
        self.tree.end()
    }

    /// Waits for the child to exit and returns its status. On Unix, once the child has exited
    /// this ends the rest of its group before reaping the child, since the group id is only safe
    /// to signal until then.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.tree.wait(&mut self.child).await
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        // Nothing can report an error from a drop, and a tree that already ended is the common
        // case.
        let _ = self.end_tree();
    }
}

/// Pins that the future [`OwnedChild::wait`] returns is `Send`, since callers spawn it on the
/// multi-threaded runtime. A non-`Send` value held across an await in any platform's `Tree::wait`
/// fails this build.
#[allow(dead_code, reason = "exists only to be type-checked")]
fn wait_future_is_send(child: &mut OwnedChild) {
    fn assert_send<T: Send>(_: T) {}
    assert_send(child.wait());
}
