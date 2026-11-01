//! `kratos-engine` serves until Ctrl+C. `kratos-engine restore <backup-id>` makes a migration
//! backup the live database again; it is refused while an engine runs on the same data directory.
//!
//! Two threads. The main thread owns the store and runs its jobs (see `store_thread`). The server
//! and its tokio runtime run on a second thread. So a panic in the store ends the process, and the
//! database closes only after the server, and every socket its runtime ran, has stopped.

mod config;
mod hub;
mod server;
mod store_thread;

use std::io;
use std::net::{Ipv4Addr, TcpListener};
use std::process::ExitCode;

use kratos_store::{BackupId, InvalidBackupId, OpenError, RestoreError, Store};

use config::{Config, ConfigError};
use hub::Hub;
use server::ServerState;
use store_thread::StoreThread;

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
        [command, id] if command == "restore" => restore(&config, &id.parse()?),
        _ => Err(StartError::Usage),
    }
}

fn serve(config: &Config) -> Result<(), StartError> {
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
    // The listener already accepts into its backlog, so a client may connect as soon as it reads
    // this.
    println!(
        "Kratos engine listening on http://127.0.0.1:{port} (data in {})",
        config.home.display()
    );
    let (store_thread, store_jobs) = store_thread::channel();
    let server = std::thread::spawn(move || run_server(listener, store_thread));
    store_jobs.run(store);
    server.join().expect("the server thread does not panic")
}

fn run_server(listener: TcpListener, store: StoreThread) -> Result<(), StartError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let state = ServerState {
            hub: Hub::new(store.clone()),
            store,
        };
        server::serve(listener, state, shutdown_signal()).await?;
        Ok(())
    })
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("cannot listen for Ctrl+C: {error}");
        std::future::pending::<()>().await;
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
    #[error("usage: kratos-engine [restore <backup-id>]")]
    Usage,
}
