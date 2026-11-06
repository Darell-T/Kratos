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
async fn cancelling_wait_preserves_drop_cleanup() {
    // Catches a cancelled wait discarding ownership before Drop can end the child's whole tree.
    let mut owner = OwnedChild::spawn(parent_of_a_sleeper()).unwrap();
    let grandchild = grandchild_id(&mut owner).await;

    tokio::select! {
        biased;
        status = owner.wait() => panic!("the sleeper exited before cancellation: {status:?}"),
        () = tokio::task::yield_now() => {},
    }
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

#[cfg(windows)]
#[tokio::test]
async fn a_suspended_child_ends_with_its_process_inside_an_existing_job() {
    // Catches a child orphaned between its suspended creation and assignment to its own job.
    use std::io::Write;
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    const HELPER: &str = "KRATOS_TEST_SUSPENDED_CHILD";
    let executable = std::env::current_exe().unwrap();
    if std::env::var_os(HELPER).is_some() {
        kratos_process::contain_current_process().unwrap();
        let child = std::process::Command::new(executable)
            .arg("--list")
            .creation_flags(0x0800_0004) // CREATE_NO_WINDOW | CREATE_SUSPENDED.
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        println!("suspended child: {}", child.id());
        std::io::stdout().flush().unwrap();
        std::io::stdin().read_line(&mut String::new()).unwrap();
        std::process::exit(0);
    }

    let mut command = Command::new(executable);
    command
        .args([
            "--exact",
            "a_suspended_child_ends_with_its_process_inside_an_existing_job",
            "--nocapture",
        ])
        .env(HELPER, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    // The outer owner models a runner's existing job and cleans up even when the assertion fails.
    let mut owner = OwnedChild::spawn(command).unwrap();
    let stdout = owner.take_stdout().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let child = tokio::time::timeout(GONE_WITHIN, async {
        loop {
            let line = lines
                .next_line()
                .await
                .unwrap()
                .expect("the helper prints its child id");
            if let Some(id) = line.strip_prefix("suspended child: ") {
                break id.parse::<u32>().unwrap();
            }
        }
    })
    .await
    .expect("the helper starts its suspended child");
    assert!(process_is_running(child));

    owner.take_stdin().unwrap().write_all(b"\n").await.unwrap();
    let status = tokio::time::timeout(GONE_WITHIN, owner.wait())
        .await
        .expect("the helper exits without Rust cleanup")
        .unwrap();
    assert!(status.success());

    assert_ends(child).await;
}
