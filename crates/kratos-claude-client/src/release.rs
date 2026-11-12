//! Which Claude Code release is installed, the newest one its updater would install, and that
//! updater. None of it reads the account: `--version`, `doctor`, and `update` need no login.
//!
//! The newest release comes from the feed `claude update` itself reads for a native install
//! (measured on 2.1.303: `{feed}/latest` answers `2.1.322`). It is fetched with the system's
//! `curl`, which Windows 10 and later, macOS, and the Linux CI images ship, so the engine needs no
//! TLS stack of its own. Other installs (npm, Homebrew, WinGet) are updated by their own manager,
//! so Kratos reports their newest release as unknown.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::str::FromStr;
use std::time::Duration;

use kratos_process::OwnedChild;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::launch::set_environment;

/// How long `--version`, `doctor`, or the feed may take to answer.
const READ_TIMEOUT: Duration = Duration::from_secs(20);
/// The native updater downloads a whole release without printing progress, and stopping it
/// midway can leave a broken CLI, so it gets as long as a slow link needs.
const UPDATE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How much of a failed command's output an error keeps.
const OUTPUT_KEPT: usize = 2_000;

/// A Claude Code release number, such as `2.1.303`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version([u64; 3]);

impl Version {
    pub const fn new(major: u64, minor: u64, patch: u64) -> Version {
        Version([major, minor, patch])
    }
}

impl FromStr for Version {
    type Err = ParseVersionError;

    /// Reads `major.minor.patch`. A prerelease suffix such as `-beta` is not a release Kratos
    /// compares, so it fails.
    fn from_str(text: &str) -> Result<Version, ParseVersionError> {
        let mut parts = text.split('.').map(str::parse::<u64>);
        match ([parts.next(), parts.next(), parts.next()], parts.next()) {
            ([Some(Ok(major)), Some(Ok(minor)), Some(Ok(patch))], None) => {
                Ok(Version([major, minor, patch]))
            }
            _ => Err(ParseVersionError(text.to_owned())),
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [major, minor, patch] = self.0;
        write!(formatter, "{major}.{minor}.{patch}")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a Claude Code release number: {0:?}")]
pub struct ParseVersionError(String);

/// The newest release the installed copy's own updater would install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Latest {
    Release(Version),
    /// Kratos cannot tell, and the text says why.
    Unknown(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    #[error("cannot run {}: {source}", .program.display())]
    Run {
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{what} did not finish within {} seconds", .after.as_secs())]
    TimedOut { what: String, after: Duration },
    #[error("{what} failed ({status}): {output}")]
    Failed {
        what: String,
        status: ExitStatus,
        output: String,
    },
    #[error("{what} printed no release number: {output}")]
    Unreadable { what: String, output: String },
}

/// The release `program --version` reports, as `2.1.303 (Claude Code)`. The CLI gets the
/// environment a session gets.
pub async fn installed_version(
    program: &Path,
    env_set: &[(OsString, OsString)],
) -> Result<Version, ReleaseError> {
    let output = run(program, &["--version"], env_set, READ_TIMEOUT).await?;
    output
        .split_whitespace()
        .next()
        .and_then(|first| first.parse().ok())
        .ok_or_else(|| ReleaseError::Unreadable {
            what: "claude --version".to_owned(),
            output: kept(&output),
        })
}

/// The newest release on the update channel `claude doctor` reports, read from `feed` for a
/// native install.
pub async fn latest_version(
    program: &Path,
    env_set: &[(OsString, OsString)],
    feed: &str,
) -> Result<Latest, ReleaseError> {
    let doctor = run(program, &["doctor"], env_set, READ_TIMEOUT).await?;
    let method = doctor_line(&doctor, "Config install method:");
    if method != Some("native") {
        return Ok(Latest::Unknown(format!(
            "Claude Code was installed by {}, which updates it; Kratos reads the newest release \
             only for the native install",
            method.unwrap_or("an unknown method")
        )));
    }
    let channel = doctor_line(&doctor, "Auto-update channel:").unwrap_or("latest");
    if !channel.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Ok(Latest::Unknown(format!(
            "Claude Code follows the update channel {channel:?}, which Kratos does not know"
        )));
    }
    let url = format!("{feed}/{channel}");
    let body = run(
        Path::new("curl"),
        &["--fail", "--silent", "--show-error", "--location", &url],
        &[],
        READ_TIMEOUT,
    )
    .await?;
    body.trim()
        .parse()
        .map(Latest::Release)
        .map_err(|_| ReleaseError::Unreadable {
            what: url,
            output: kept(&body),
        })
}

/// Runs `claude update`, which installs the newest release on the CLI's channel, and returns
/// the release installed afterward. Sessions already running keep their release until they end.
pub async fn update(
    program: &Path,
    env_set: &[(OsString, OsString)],
) -> Result<Version, ReleaseError> {
    run(program, &["update"], env_set, UPDATE_TIMEOUT).await?;
    installed_version(program, env_set).await
}

/// `claude doctor`'s value after `label`, such as `native` after `Config install method:`.
fn doctor_line<'a>(doctor: &'a str, label: &str) -> Option<&'a str> {
    doctor
        .lines()
        .find_map(|line| line.trim().strip_prefix(label))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Runs `program` with no input and returns what it printed, stdout then stderr. The whole
/// process tree ends when it fails or runs out of time.
async fn run(
    program: &Path,
    args: &[&str],
    env_set: &[(OsString, OsString)],
    timeout: Duration,
) -> Result<String, ReleaseError> {
    let what = std::iter::once(program.display().to_string())
        .chain(args.iter().map(|arg| (*arg).to_owned()))
        .collect::<Vec<_>>()
        .join(" ");
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    set_environment(&mut command, env_set);
    let mut child = OwnedChild::spawn(command).map_err(|source| ReleaseError::Run {
        program: program.to_owned(),
        source,
    })?;
    let mut stdout = child.take_stdout().expect("stdout was piped");
    let mut stderr = child.take_stderr().expect("stderr was piped");
    let finished = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (_, _, status) = tokio::join!(
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err),
            child.wait()
        );
        (status, out, err)
    };
    let (status, out, err) =
        tokio::time::timeout(timeout, finished)
            .await
            .map_err(|_| ReleaseError::TimedOut {
                what: what.clone(),
                after: timeout,
            })?;
    let status = status.map_err(|source| ReleaseError::Run {
        program: program.to_owned(),
        source,
    })?;
    let mut output = String::from_utf8_lossy(&out).into_owned();
    output.push_str(&String::from_utf8_lossy(&err));
    if status.success() {
        Ok(output)
    } else {
        Err(ReleaseError::Failed {
            what,
            status,
            output: kept(&output),
        })
    }
}

/// The last [`OUTPUT_KEPT`] bytes of `output`, where an updater says why it failed.
fn kept(output: &str) -> String {
    let output = output.trim();
    let mut start = output.len().saturating_sub(OUTPUT_KEPT);
    while !output.is_char_boundary(start) {
        start += 1;
    }
    output[start..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_orders_release_numbers_by_each_part() {
        let parsed: Version = "2.1.303".parse().unwrap();
        assert_eq!(parsed, Version::new(2, 1, 303));
        assert_eq!(parsed.to_string(), "2.1.303");
        assert!(Version::new(2, 1, 303) < Version::new(2, 1, 322));
        assert!(Version::new(2, 1, 999) < Version::new(2, 2, 0));
        for text in ["2.1", "2.1.3.4", "2.1.x", "2.1.322-beta", ""] {
            assert!(text.parse::<Version>().is_err(), "{text}");
        }
    }

    #[test]
    fn reads_the_install_method_and_channel_from_doctor() {
        let doctor = "Claude Code doctor\n\n  Running: native (2.1.303)\n  Config install method: \
                      native\n  Auto-update channel: latest\n";
        assert_eq!(
            doctor_line(doctor, "Config install method:"),
            Some("native")
        );
        assert_eq!(doctor_line(doctor, "Auto-update channel:"), Some("latest"));
        assert_eq!(doctor_line(doctor, "Missing:"), None);
    }
}
