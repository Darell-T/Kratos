use std::io;
use std::process::ExitStatus;

use tokio::process::{Child, Command};
use tokio::signal::unix::{SignalKind, signal};

/// The child's process group. Its id is the child's process id, because the child leads it.
#[derive(Debug)]
pub struct Tree {
    group: Option<libc::pid_t>,
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
        Ok((Tree { group: Some(group) }, child))
    }

    /// Attempts to kill the group once, only while its leader has not been reaped.
    pub fn end(&mut self, child: &Child) -> io::Result<()> {
        let Some(group) = self.group.take() else {
            return Ok(());
        };
        if child.id() != u32::try_from(group).ok() {
            return Ok(());
        }
        #[cfg(test)]
        tests::SIGNALLED.with_borrow_mut(|groups| groups.push(group));
        #[allow(unsafe_code, reason = "killpg has no safe wrapper in std")]
        // SAFETY: killpg only reads its two integer arguments; it touches no memory of this
        // process.
        let result = unsafe { libc::killpg(group, libc::SIGKILL) };
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

    pub async fn wait(&mut self, child: &mut Child) -> io::Result<ExitStatus> {
        drop(child.stdin.take());
        let Some(group) = self
            .group
            .filter(|group| child.id() == u32::try_from(*group).ok())
        else {
            self.group = None;
            return child.wait().await;
        };
        let leader = libc::id_t::try_from(group)
            .map_err(|_| io::Error::other(format!("process group {group} does not fit an id_t")))?;
        // Subscribe before checking exit so an intervening SIGCHLD cannot be missed.
        let mut exited = signal(SignalKind::child())?;
        loop {
            let done = leader_exited(leader).inspect_err(|error| {
                if error.raw_os_error() == Some(libc::ECHILD) {
                    self.group = None;
                }
            })?;
            if done {
                break;
            }
            exited
                .recv()
                .await
                .ok_or_else(|| io::Error::other("the SIGCHLD listener closed"))?;
        }
        self.end(child)?;
        child.wait().await
    }
}

/// Reports whether the leader has exited, without reaping it. This is a plain function so the
/// `siginfo_t` it holds, which contains raw pointers, never lives in an async state machine and
/// makes the wait future non-`Send`.
fn leader_exited(leader: libc::id_t) -> io::Result<bool> {
    #[allow(unsafe_code, reason = "waitid requires a siginfo_t output buffer")]
    // SAFETY: zero is valid for siginfo_t and keeps si_pid zero when WNOHANG finds no status.
    let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
    loop {
        #[allow(unsafe_code, reason = "waitid has no safe wrapper in std")]
        // SAFETY: info is writable for the entire call. WNOWAIT keeps the leader unreaped.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                leader,
                &raw mut info,
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        };
        if result == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    #[allow(
        unsafe_code,
        reason = "siginfo_t exposes si_pid through an unsafe accessor"
    )]
    // SAFETY: info was zeroed and waitid succeeded, so si_pid is initialized even without a status.
    Ok(unsafe { info.si_pid() } != 0
        && matches!(
            info.si_code,
            libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
        ))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::process::Stdio;

    use tokio::process::Command;

    use crate::OwnedChild;

    thread_local! {
        pub(super) static SIGNALLED: RefCell<Vec<libc::pid_t>> = const { RefCell::new(Vec::new()) };
    }

    #[tokio::test]
    async fn ending_then_reaping_and_dropping_signals_the_group_once() {
        // Catches a retained group id being signalled again after end_tree and reaping.
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 300"]);
        let mut owner = OwnedChild::spawn(command).unwrap();
        let id = owner.child().id().unwrap();

        owner.end_tree().unwrap();
        owner.wait().await.unwrap();
        owner.end_tree().unwrap();
        drop(owner);

        SIGNALLED.with_borrow(|groups| assert_eq!(groups.as_slice(), &[id as libc::pid_t]));
    }

    #[tokio::test]
    async fn a_stopped_leader_does_not_end_its_live_group() {
        // Catches a stopped-child waitid report being treated as an exit and killing the group.
        use tokio::io::{AsyncBufReadExt, BufReader};

        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 300 & echo $!; wait"])
            .stdout(Stdio::piped());
        let mut owner = OwnedChild::spawn(command).unwrap();
        let leader = owner.child().id().unwrap();
        let stdout = owner.child().stdout.take().unwrap();
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).await.unwrap();
        let grandchild: u32 = line.trim().parse().unwrap();
        assert!(
            Command::new("kill")
                .args(["-STOP", &leader.to_string()])
                .status()
                .await
                .unwrap()
                .success()
        );

        let waited =
            tokio::time::timeout(std::time::Duration::from_millis(300), owner.wait()).await;

        let alive = fake_agents::process_is_running(grandchild);
        owner.end_tree().unwrap();
        owner.wait().await.unwrap();
        assert!(
            waited.is_err(),
            "a stopped leader ended its group: {waited:?}"
        );
        assert!(
            alive,
            "the stopped leader's grandchild {grandchild} was killed"
        );
    }

    #[tokio::test]
    async fn raw_wait_retires_the_group_without_signalling_it() {
        // Catches end_tree or Drop signalling a group after the raw child handle reaped its leader.
        let mut command = Command::new("sh");
        command.args(["-c", "exit 0"]);
        let mut owner = OwnedChild::spawn(command).unwrap();

        assert!(owner.child().wait().await.unwrap().success());
        owner.end_tree().unwrap();
        drop(owner);

        SIGNALLED
            .with_borrow(|groups| assert!(groups.is_empty(), "unexpected signals: {groups:?}"));
    }

    #[tokio::test]
    async fn waiting_closes_stdin_before_observing_exit() {
        // Catches a pre-reap wait keeping stdin open while the child waits for EOF.
        let mut command = Command::new("sh");
        command.args(["-c", "cat >/dev/null"]).stdin(Stdio::piped());
        let mut owner = OwnedChild::spawn(command).unwrap();

        let status = tokio::time::timeout(std::time::Duration::from_secs(20), owner.wait())
            .await
            .expect("waiting closes stdin so cat can exit")
            .unwrap();
        assert!(status.success());
        drop(owner);

        SIGNALLED.with_borrow(|groups| assert_eq!(groups.len(), 1));
    }

    #[tokio::test]
    async fn cancelling_wait_preserves_drop_cleanup() {
        // Catches a cancelled pre-reap wait discarding the ability to end the group.
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 300"]);
        let mut owner = OwnedChild::spawn(command).unwrap();
        let id = owner.child().id().unwrap();

        tokio::select! {
            biased;
            status = owner.wait() => panic!("sleep exited before cancellation: {status:?}"),
            () = tokio::task::yield_now() => {},
        }
        drop(owner);

        SIGNALLED.with_borrow(|groups| assert_eq!(groups.as_slice(), &[id as libc::pid_t]));
    }
}
