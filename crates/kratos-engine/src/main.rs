//! `kratos-engine` serves until Ctrl+C. `kratos-engine restore <backup-id>` makes a migration
//! backup the live database again; it is refused while an engine runs on the same data directory.
//! `kratos-engine mcp` is the MCP server Codex starts for each thread (see `kratos_engine::mcp`).
//! On Windows, the serving engine joins its own kill-on-close job, so every child ends with it.
//!
//! Two threads. The main thread owns the store and runs its jobs (see `store_thread`). The server
//! and its tokio runtime run on a second thread. So a panic in the store ends the process, and the
//! database closes only after the server, and every socket its runtime ran, has stopped.
//!
//! Codex is found through `CODEX_EXE`, else `PATH` (D2), and Claude through `CLAUDE_EXE`, else
//! `PATH` (L8). Without one the engine still serves, and a request that needs it answers 503
//! saying why.

use std::ffi::OsString;
use std::io;
use std::net::{Ipv4Addr, TcpListener};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use kratos_api_client::{Client, ClientError};
use kratos_claude_client::locate_claude;
use kratos_codex_client::{LocateError, locate_codex};
use kratos_command_safety::DangerousCommandPlatform;
use kratos_store::{BackupId, InvalidBackupId, OpenError, RestoreError, Store, StoreError};

use kratos_engine::config::{Config, ConfigError};
use kratos_engine::hub::Hub;
use kratos_engine::mcp::{self, McpServer};
use kratos_engine::providers::claude::{self, ClaudeProvider, ClaudeSettings};
use kratos_engine::providers::codex::{self, CodexProvider, CodexSettings, ProtectedFolders};
use kratos_engine::scheduler::{self, Providers, SystemClock};
use kratos_engine::server::{self, ClientToken, ServerState};
use kratos_engine::store_thread::{self, StoreThread};
use kratos_engine::usage::{LimitStop, UsageTracker};

/// The TypeScript provider's deadlines: a turn may run ten minutes without waiting on the user,
/// and a request to Codex may take thirty seconds.
const TURN_TIMEOUT: Duration = Duration::from_secs(600);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a turn Codex agreed to stop may take to end before Kratos closes Codex.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// The wait after a Claude process's first failure, doubled by each failure in a row (L4), and the
/// failures after which only the user starts it again.
const RESTART_WAIT: Duration = Duration::from_secs(1);
const MAX_RESTARTS: u32 = 5;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), StartError> {
    let config = Config::from_env()?;
    let arguments: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    match &arguments[..] {
        [] => serve(&config),
        [command] if command == mcp::SUBCOMMAND => mcp(&config),
        [command, id] if command == "restore" => restore(&config, &id.parse()?),
        _ => Err(StartError::Usage),
    }
}

fn mcp(config: &Config) -> Result<(), StartError> {
    let thread_id = config.mcp_thread.clone().ok_or(StartError::McpThread)?;
    let client = Client::new(&format!("http://127.0.0.1:{}", config.port))
        .expect("a loopback address is valid")
        .with_token_from(&config.home);
    let caller = mcp::Caller::new(client, thread_id)?;
    mcp::serve(&caller, io::stdin().lock(), io::stdout())?;
    Ok(())
}

fn serve(config: &Config) -> Result<(), StartError> {
    #[cfg(windows)]
    kratos_process::contain_current_process().map_err(StartError::Contain)?;
    let store = Store::open(&config.home)?;
    if let Some(backup) = store.backup_taken() {
        println!(
            "Backed up the version {} database to {} before migrating it",
            backup.source_version,
            backup.path.display()
        );
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, config.port)).map_err(|source| {
        StartError::Listen {
            port: config.port,
            source,
        }
    })?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let token = ClientToken::issue(&config.home).map_err(StartError::Token)?;
    // The listener already accepts into its backlog, so a client may connect as soon as it reads
    // this.
    println!(
        "Kratos engine listening on http://127.0.0.1:{port} (data in {})",
        config.home.display()
    );
    let (store_thread, store_jobs) = store_thread::channel();
    let server_config = config.clone();
    let server =
        std::thread::spawn(move || run_server(listener, store_thread, &server_config, token));
    store_jobs.run(store);
    server.join().expect("the server thread does not panic")
}

fn run_server(
    listener: TcpListener,
    store: StoreThread,
    config: &Config,
    token: ClientToken,
) -> Result<(), StartError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let hub = Hub::new(store.clone());
        let usage = UsageTracker::default();
        let port = listener.local_addr()?.port();
        let (limit_stops, stops) = mpsc::unbounded_channel();
        let codex = start_codex(config, port, &hub, &store, &usage, limit_stops.clone()).await?;
        let claude = start_claude(config, &hub, &store, &codex, &usage, limit_stops).await?;
        let providers = Providers {
            codex: codex.as_ref().ok().cloned(),
            claude: claude.as_ref().ok().cloned(),
        };
        let scheduler = if providers.is_empty() {
            None
        } else {
            let (hub, store, usage) = (hub.clone(), store.clone(), usage.clone());
            let clock = Arc::new(SystemClock);
            Some(scheduler::start(hub, store, usage, providers, clock, stops).await?)
        };
        let state = ServerState {
            hub,
            store,
            codex,
            claude,
            scheduler,
            usage,
            token,
        };
        server::serve(listener, state.clone(), shutdown_signal()).await?;
        // The agents end before the store closes, so nothing they report goes unrecorded (L2).
        if let Ok(claude) = &state.claude {
            claude.shutdown().await;
        }
        if let Ok(codex) = &state.codex {
            codex.shutdown().await;
        }
        Ok(())
    })
}

/// Codex, or why it cannot run, which the engine reports and serves on without.
async fn start_codex(
    config: &Config,
    port: u16,
    hub: &Hub,
    store: &StoreThread,
    usage: &UsageTracker,
    limit_stops: mpsc::UnboundedSender<LimitStop>,
) -> Result<Result<CodexProvider, String>, StoreError> {
    match codex_settings(config, port) {
        Ok(settings) => {
            let (hub, store, usage) = (hub.clone(), store.clone(), usage.clone());
            Ok(Ok(
                codex::start(settings, hub, store, usage, limit_stops).await?
            ))
        }
        Err(missing) => {
            eprintln!("Codex threads are unavailable: {missing}");
            Ok(Err(missing.to_string()))
        }
    }
}

/// Claude, or why it cannot run, which the engine reports and serves on without.
async fn start_claude(
    config: &Config,
    hub: &Hub,
    store: &StoreThread,
    codex: &Result<CodexProvider, String>,
    usage: &UsageTracker,
    limit_stops: mpsc::UnboundedSender<LimitStop>,
) -> Result<Result<ClaudeProvider, String>, StoreError> {
    match claude_settings(config) {
        Ok(mut settings) => {
            settings.tools = codex.as_ref().ok().map(|codex| claude::ClaudeTools {
                runner: codex.clone(),
                protected: ProtectedFolders::new(
                    config.home.clone(),
                    config.codex_home.clone(),
                    config.user_home.clone(),
                ),
            });
            let (hub, store, usage) = (hub.clone(), store.clone(), usage.clone());
            Ok(Ok(
                claude::start(settings, hub, store, usage, limit_stops).await?
            ))
        }
        Err(missing) => {
            eprintln!("Claude threads are unavailable: {missing}");
            Ok(Err(missing.to_string()))
        }
    }
}

fn claude_settings(config: &Config) -> Result<ClaudeSettings, ClaudeMissing> {
    let program = locate_claude(config.claude_exe.as_deref(), config.path.as_deref())?;
    let claude_home = config
        .user_home
        .clone()
        .ok_or(ClaudeMissing::NoHomeFolder)?;
    Ok(ClaudeSettings {
        tools: None,
        program,
        claude_home,
        env_set: Vec::new(),
        turn_timeout: TURN_TIMEOUT,
        stop_timeout: STOP_TIMEOUT,
        control_timeout: REQUEST_TIMEOUT,
        restart_wait: RESTART_WAIT,
        max_restarts: MAX_RESTARTS,
    })
}

fn codex_settings(config: &Config, port: u16) -> Result<CodexSettings, CodexMissing> {
    let program = locate_codex(config.codex_exe.as_deref(), config.path.as_deref())?;
    let server_folder = config.home.join("codex-server");
    std::fs::create_dir_all(&server_folder).map_err(CodexMissing::ServerFolder)?;
    Ok(CodexSettings {
        program,
        extra_args: Vec::<OsString>::new(),
        env_set: Vec::new(),
        server_folder,
        protected: ProtectedFolders::new(
            config.home.clone(),
            config.codex_home.clone(),
            config.user_home.clone(),
        ),
        turn_timeout: TURN_TIMEOUT,
        stop_timeout: STOP_TIMEOUT,
        request_timeout: REQUEST_TIMEOUT,
        platform: DangerousCommandPlatform::host(),
        mcp: Some(McpServer {
            program: std::env::current_exe().map_err(CodexMissing::Engine)?,
            home: config.home.clone(),
            port,
        }),
    })
}

#[derive(Debug, thiserror::Error)]
enum ClaudeMissing {
    #[error(transparent)]
    Locate(#[from] kratos_claude_client::LocateError),
    #[error("cannot find the user's home folder, where Claude keeps its history")]
    NoHomeFolder,
}

#[derive(Debug, thiserror::Error)]
enum CodexMissing {
    #[error(transparent)]
    Locate(#[from] LocateError),
    #[error("cannot create Codex's working folder: {0}")]
    ServerFolder(#[source] io::Error),
    #[error("cannot find the engine's own binary for the MCP server: {0}")]
    Engine(#[source] io::Error),
}

#[cfg(not(windows))]
async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("cannot listen for Ctrl+C: {error}");
        std::future::pending::<()>().await;
    }
}

// A launcher that starts the engine in a new process group can stop it only with Ctrl+Break.
#[cfg(windows)]
async fn shutdown_signal() {
    let signals = (
        tokio::signal::windows::ctrl_c(),
        tokio::signal::windows::ctrl_break(),
    );
    let (mut ctrl_c, mut ctrl_break) = match signals {
        (Ok(ctrl_c), Ok(ctrl_break)) => (ctrl_c, ctrl_break),
        (Err(error), _) | (_, Err(error)) => {
            eprintln!("cannot listen for console shutdown signals: {error}");
            return std::future::pending().await;
        }
    };
    tokio::select! {
        _ = ctrl_c.recv() => {},
        _ = ctrl_break.recv() => {},
    }
}

fn restore(config: &Config, id: &BackupId) -> Result<(), StartError> {
    let restored = kratos_store::restore(&config.home, id)?;
    println!(
        "Restored backup {} (version {}) as the database in {}",
        restored.restored.id,
        restored.restored.source_version,
        config.home.display()
    );
    if let Some(replaced) = restored.replaced {
        println!(
            "The database it replaced is saved as backup {} (version {})",
            replaced.id, replaced.source_version
        );
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum StartError {
    #[cfg(windows)]
    #[error(
        "cannot put the engine in a Windows job object; refusing to start because agent processes could outlive it: {0}"
    )]
    Contain(#[source] io::Error),
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Open(#[from] OpenError),
    #[error(transparent)]
    Restore(#[from] RestoreError),
    #[error(transparent)]
    BackupId(#[from] InvalidBackupId),
    #[error("cannot listen on 127.0.0.1:{port}: {source}")]
    Listen {
        port: u16,
        #[source]
        source: io::Error,
    },
    #[error("cannot run the server: {0}")]
    Server(#[from] io::Error),
    #[error("cannot write the client token: {0}")]
    Token(#[source] io::Error),
    #[error("cannot read what the last engine left unfinished: {0}")]
    Records(#[from] StoreError),
    #[error("usage: kratos-engine [restore <backup-id> | mcp]")]
    Usage,
    #[error("kratos-engine mcp needs KRATOS_THREAD_ID, the thread whose agent it serves")]
    McpThread,
}
