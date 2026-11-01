//! A child's whole process tree ends when its owner drops it or ends it.

use std::time::{Duration, Instant};

use kratos_process::OwnedChild;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

const GONE_WITHIN: Duration = Duration::from_secs(20);

/// A shell that starts a grandchild sleeping for five minutes, prints the grandchild's process id,
/// and waits for it.
fn parent_of_a_sleeper() -> Command {
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 300 & echo $!; wait"]);
        command
    };
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$p = Start-Process -PassThru -WindowStyle Hidden -FilePath powershell.exe \
             -ArgumentList '-NoProfile','-Command','Start-Sleep 300'; $p.Id; $p.WaitForExit()",
        ]);
        command
    };
    command.stdout(std::process::Stdio::piped());
    command
}

async fn grandchild_id(owner: &mut OwnedChild) -> u32 {
    let stdout = owner.child().stdout.take().expect("stdout is piped");
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .await
        .expect("the parent prints its grandchild's id");
    line.trim()
        .parse()
        .unwrap_or_else(|_| panic!("not a process id: {line:?}"))
}

/// A zombie, which a container's init may never reap, counts as ended.
#[cfg(unix)]
fn is_running(id: u32) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &id.to_string()])
        .output()
        .expect("ps runs");
    let state = String::from_utf8_lossy(&output.stdout);
    let state = state.trim();
    !state.is_empty() && !state.starts_with('Z')
}

#[cfg(windows)]
fn is_running(id: u32) -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {id}"), "/NH", "/FO", "CSV"])
        .output()
        .expect("tasklist runs");
    String::from_utf8_lossy(&output.stdout).contains(&format!("\"{id}\""))
}

async fn assert_ends(id: u32) {
    let started = Instant::now();
    while is_running(id) {
        assert!(
            started.elapsed() < GONE_WITHIN,
            "process {id} still runs {GONE_WITHIN:?} after its tree was ended"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn dropping_the_owner_ends_the_grandchild_too() {
    let mut owner = OwnedChild::spawn(parent_of_a_sleeper()).unwrap();
    let grandchild = grandchild_id(&mut owner).await;
    assert!(is_running(grandchild));

    drop(owner);

    assert_ends(grandchild).await;
}

#[tokio::test]
async fn ending_the_tree_still_reports_the_child_exit() {
    let mut owner = OwnedChild::spawn(parent_of_a_sleeper()).unwrap();
    let grandchild = grandchild_id(&mut owner).await;

    owner.end_tree().unwrap();

    let status = tokio::time::timeout(GONE_WITHIN, owner.wait())
        .await
        .expect("the child exits once its tree is ended")
        .unwrap();
    assert!(!status.success());
    assert_ends(grandchild).await;
    owner.end_tree().unwrap();
}
