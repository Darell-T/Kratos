//! A shell or interpreter an agent starts without a script turns later typed input into commands
//! Kratos never sees, so the check finds it inside the command text Codex sends for approval.

use kratos_command_safety::{reads_commands_from_input, split_command_text};

fn reads_input(approval_text: &str) -> bool {
    let command = split_command_text(approval_text).expect("the text splits");
    reads_commands_from_input(&command)
}

#[test]
fn finds_a_bare_shell_or_interpreter_inside_the_command_codex_wraps_in_a_login_shell() {
    // Catches a check that only looks at the outer `bash -lc`, which always has a script, so the
    // measured `exec_command {"cmd": "bash", "tty": true}` would pass unasked.
    for interactive in [
        "/bin/bash -lc bash",
        "/bin/bash -lc 'cd /work && python3'",
        "/bin/zsh -lc 'node'",
        "python3",
        "pwsh -NoProfile -Command pwsh",
    ] {
        assert!(reads_input(interactive), "{interactive}");
    }
    for one_shot in [
        "/bin/bash -lc 'cargo test 2>&1 | tail -n 20'",
        "/bin/bash -lc 'python3 -m pytest -q'",
        "/bin/bash -lc 'echo hi > inside.txt'",
        "pwsh -NoProfile -Command 'Get-ChildItem'",
    ] {
        assert!(!reads_input(one_shot), "{one_shot}");
    }
}
