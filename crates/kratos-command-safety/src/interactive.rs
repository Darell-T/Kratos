//! Kratos's own rule, not copied from Codex: which programs read commands from their input.
//!
//! Measured on Codex 0.159.1: text an agent types into a running process with `write_stdin` never
//! reaches the client as an approval request. A shell or interpreter started without a script
//! turns that text into commands Kratos never sees, so starting one is something Kratos cannot
//! check (D18).

use std::path::Path;

/// A family of programs that run commands from their input unless given code or a script.
struct Family {
    names: &'static [&'static str],
    /// Options that give the program its code inline, so it reads no commands from input.
    code: &'static [&'static str],
    /// Options that keep it reading input after any code it was given.
    stay: &'static [&'static str],
    /// The first operand names a database, not a script.
    database: bool,
}

/// Option names are lowercase: arguments are compared without case, for the Windows shells.
const FAMILIES: [Family; 9] = [
    Family {
        names: &["bash", "sh", "zsh", "dash", "ksh", "fish", "csh", "tcsh"],
        code: &["-c"],
        stay: &["-i", "-s"],
        database: false,
    },
    Family {
        names: &["pwsh", "powershell"],
        code: &[
            "-c",
            "-command",
            "-f",
            "-file",
            "-e",
            "-ec",
            "-encodedcommand",
        ],
        stay: &["-noexit"],
        database: false,
    },
    Family {
        names: &["cmd"],
        code: &["/c"],
        stay: &["/k"],
        database: false,
    },
    Family {
        names: &["python", "python2", "python3", "pypy", "pypy3"],
        code: &["-c", "-m"],
        stay: &["-i"],
        database: false,
    },
    Family {
        names: &["node", "deno", "bun"],
        code: &["-e", "--eval", "-p", "--print"],
        stay: &["-i", "--interactive"],
        database: false,
    },
    Family {
        names: &["ruby", "perl"],
        code: &["-e"],
        stay: &[],
        database: false,
    },
    Family {
        names: &["php"],
        code: &["-r", "-f"],
        stay: &["-a"],
        database: false,
    },
    Family {
        names: &["lua", "luajit"],
        code: &["-e"],
        stay: &["-i"],
        database: false,
    },
    Family {
        names: &["psql", "mysql", "sqlite3", "redis-cli", "mongosh"],
        code: &[
            "-c",
            "--command",
            "-f",
            "--file",
            "-e",
            "--execute",
            "--eval",
        ],
        stay: &[],
        database: true,
    },
];

/// Programs that read input whatever their arguments.
const ALWAYS_INTERACTIVE: [&str; 3] = ["irb", "ipython", "ghci"];

/// Whether `words`, one command's program and arguments, starts a program that reads commands
/// from its input.
pub(crate) fn reads_input(words: &[String]) -> bool {
    let Some((program, arguments)) = words.split_first() else {
        return false;
    };
    let name = program_name(program);
    if ALWAYS_INTERACTIVE.contains(&name.as_str()) {
        return true;
    }
    let Some(family) = FAMILIES
        .iter()
        .find(|family| family.names.contains(&name.as_str()))
    else {
        return false;
    };
    let arguments: Vec<String> = arguments
        .iter()
        .map(|argument| argument.to_lowercase())
        .collect();
    if arguments
        .iter()
        .any(|argument| family.stay.contains(&argument.as_str()))
    {
        return true;
    }
    let is_posix_shell = std::ptr::eq(family, &FAMILIES[0]);
    let gives_code = arguments.iter().any(|argument| {
        family.code.contains(&argument.as_str())
            || (is_posix_shell && is_short_cluster_with_c(argument))
    });
    let has_script = !family.database
        && arguments
            .iter()
            .any(|argument| !argument.starts_with(['-', '/']));
    !gives_code && !has_script
}

/// The program's name without folders, case, or a Windows extension: `/usr/bin/Python3.EXE` is
/// `python3`.
fn program_name(program: &str) -> String {
    let file = Path::new(program).file_name().map_or_else(
        || program.to_owned(),
        |file| file.to_string_lossy().into_owned(),
    );
    // A Windows path keeps its backslashes on other systems.
    let file = file.rsplit('\\').next().unwrap_or(&file).to_lowercase();
    match file.rsplit_once('.') {
        Some((stem, "exe" | "cmd" | "bat")) => stem.to_owned(),
        _ => file,
    }
}

/// Clustered short options that include `-c`, such as `-lc` or `-ec`.
fn is_short_cluster_with_c(argument: &str) -> bool {
    argument.len() > 2
        && argument.starts_with('-')
        && !argument.starts_with("--")
        && argument[1..].contains('c')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(command: &[&str]) -> Vec<String> {
        command.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn a_bare_shell_or_interpreter_reads_input_and_one_given_code_does_not() {
        // Catches the measured bypass: a bare `bash` started under Approve for me took `rm -rf`
        // through write_stdin without any approval.
        for interactive in [
            &["bash"][..],
            &["/bin/bash", "-l"],
            &["sh", "-s"],
            &["python3"],
            &["C:\\Python\\python.exe", "-i", "script.py"],
            &["node"],
            &["pwsh", "-NoLogo"],
            &["powershell.exe", "-NoExit", "-Command", "ls"],
            &["cmd.exe", "/K", "dir"],
            &["psql", "app"],
            &["php", "-a"],
            &["irb"],
        ] {
            assert!(reads_input(&words(interactive)), "{interactive:?}");
        }
        for one_shot in [
            &["bash", "-lc", "cargo test"][..],
            &["bash", "build.sh"],
            &["python3", "-c", "print(1)"],
            &["python3", "-m", "pytest"],
            &["node", "server.js"],
            &["pwsh", "-NoProfile", "-Command", "Get-ChildItem"],
            &["cmd", "/c", "dir"],
            &["psql", "app", "-c", "select 1"],
            &["perl", "-i", "-pe", "s/a/b/", "file.txt"],
            &["cargo", "test"],
            &[],
        ] {
            assert!(!reads_input(&words(one_shot)), "{one_shot:?}");
        }
    }
}
