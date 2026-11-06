//! A child's whole process tree ends when its owner drops it or ends it.

use std::time::{Duration, Instant};

use fake_agents::process_is_running;
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
    let stdout = owner.take_stdout().expect("stdout is piped");
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .await
        .expect("the parent prints its grandchild's id");
    line.trim()
        .parse()
        .unwrap_or_else(|_| panic!("not a process id: {line:?}"))
}

async fn assert_ends(id: u32) {
    let started = Instant::now();
    while process_is_running(id) {
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
    assert!(process_is_running(grandchild));

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

#[cfg(unix)]
#[tokio::test]
async fn waiting_for_the_leader_also_ends_its_surviving_grandchild() {
    // Catches reaping a naturally exited leader before safely ending its remaining group.
    let mut command = Command::new("sh");
    command
        .args([
            "-c",
            "sleep 300 </dev/null >/dev/null 2>&1 & echo $!; exit 0",
        ])
        .stdout(std::process::Stdio::piped());
    let mut owner = OwnedChild::spawn(command).unwrap();
    let grandchild = grandchild_id(&mut owner).await;

    let status = tokio::time::timeout(GONE_WITHIN, owner.wait())
        .await
        .expect("the leader has exited")
        .unwrap();

    assert!(status.success());
    assert_ends(grandchild).await;
}
