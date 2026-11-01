use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

/// The engine's exclusive hold on its data directory, released when this value drops.
///
/// It belongs to the open handle on `kratos.lock`, so the system releases it when the handle
/// closes, including when the process dies. The file itself stays and is never deleted, because
/// deleting a lock file races with the next engine opening it.
pub(crate) struct DataDirectory {
    home: PathBuf,
    _lock: File,
}

impl DataDirectory {
    /// Creates `home` if needed and locks it. `Err(InUse)` while another holder exists, in this
    /// process or another.
    pub(crate) fn lock(home: &Path) -> Result<DataDirectory, DirectoryError> {
        fs::create_dir_all(home).map_err(io_error(home))?;
        let lock_path = home.join("kratos.lock");
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(io_error(&lock_path))?;
        match file.try_lock() {
            Ok(()) => Ok(DataDirectory {
                home: home.to_path_buf(),
                _lock: file,
            }),
            Err(TryLockError::WouldBlock) => Err(DirectoryError::InUse(home.to_path_buf())),
            Err(TryLockError::Error(source)) => Err(io_error(&lock_path)(source)),
        }
    }

    pub(crate) fn database_path(&self) -> PathBuf {
        self.home.join("kratos.db")
    }

    pub(crate) fn backups_path(&self) -> PathBuf {
        self.home.join("backups")
    }
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> DirectoryError {
    let path = path.to_path_buf();
    move |source| DirectoryError::Io { path, source }
}

#[derive(Debug, thiserror::Error)]
pub enum DirectoryError {
    #[error("another Kratos engine is using the data directory {}", .0.display())]
    InUse(PathBuf),
    #[error("cannot use the data directory {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}
