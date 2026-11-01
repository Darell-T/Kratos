use std::io;

use tokio::process::{Child, Command};

/// The child's process group. Its id is the child's process id, because the child leads it.
#[derive(Debug)]
pub struct Tree {
    group: libc::pid_t,
}

impl Tree {
    pub fn spawn(command: &mut Command) -> io::Result<(Tree, Child)> {
        command.process_group(0);
        let child = command.spawn()?;
        let id = child
            .id()
            .ok_or_else(|| io::Error::other("the child exited before its process id was read"))?;
        let group = libc::pid_t::try_from(id)
            .map_err(|_| io::Error::other(format!("process id {id} does not fit a pid_t")))?;
        Ok((Tree { group }, child))
    }

    /// Kills the group. ESRCH means nothing in it is left.
    ///
    /// A group id stays reserved while any process in the group lives, so the signal reaches this
    /// child's tree. Once all of them have exited the id can in principle be reused for an
    /// unrelated group, which is why the owner ends the tree as soon as it stops needing it.
    pub fn end(&mut self) -> io::Result<()> {
        #[allow(unsafe_code, reason = "killpg has no safe wrapper in std")]
        // SAFETY: killpg only reads its two integer arguments; it touches no memory of this process.
        let result = unsafe { libc::killpg(self.group, libc::SIGKILL) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }
}
