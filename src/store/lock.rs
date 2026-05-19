use std::{
    ffi::OsString,
    fs::{File, OpenOptions},
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex, MutexGuard, TryLockError},
    time::{Duration, Instant},
};

use eyre::{eyre, Context};
use fs2::FileExt;

use super::StorePaths;

const REQUIRED_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const OPTIONAL_LOCK_TIMEOUT: Duration = Duration::from_millis(250);
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(20);

static CREDS_STORE_PROCESS_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Clone, Copy, Debug)]
pub(super) enum StoreLockMode {
    Required,
    Optional,
}

impl StoreLockMode {
    fn timeout(self) -> Duration {
        match self {
            Self::Required => REQUIRED_LOCK_TIMEOUT,
            Self::Optional => OPTIONAL_LOCK_TIMEOUT,
        }
    }
}

pub(super) struct StoreLock {
    _process_guard: MutexGuard<'static, ()>,
    file: File,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub(super) fn acquire_store_lock(
    paths: &StorePaths,
    mode: StoreLockMode,
) -> eyre::Result<Option<StoreLock>> {
    std::fs::create_dir_all(&paths.dir)
        .wrap_err_with(|| format!("failed to create creds store dir '{}'", paths.dir.display()))?;

    let timeout = mode.timeout();
    let Some(process_guard) = acquire_process_lock(&paths.path, mode, timeout)? else {
        return Ok(None);
    };

    let lock_path = lock_path(&paths.path);
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&lock_path)
        .wrap_err_with(|| format!("failed to open creds store lock '{}'", lock_path.display()))?;

    let start = Instant::now();
    loop {
        match lock_file.try_lock_exclusive() {
            Ok(()) => {
                return Ok(Some(StoreLock {
                    _process_guard: process_guard,
                    file: lock_file,
                }));
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                if start.elapsed() >= timeout {
                    return lock_timeout_result(&paths.path, mode);
                }
                std::thread::sleep(LOCK_RETRY_DELAY);
            }
            Err(err) => {
                return Err(err).wrap_err_with(|| {
                    format!("failed to lock creds store '{}'", lock_path.display())
                });
            }
        }
    }
}

fn acquire_process_lock(
    store_path: &Path,
    mode: StoreLockMode,
    timeout: Duration,
) -> eyre::Result<Option<MutexGuard<'static, ()>>> {
    let start = Instant::now();
    loop {
        match CREDS_STORE_PROCESS_MUTEX.try_lock() {
            Ok(guard) => return Ok(Some(guard)),
            Err(TryLockError::WouldBlock) => {
                if start.elapsed() >= timeout {
                    return lock_timeout_result(store_path, mode);
                }
                std::thread::sleep(LOCK_RETRY_DELAY);
            }
            Err(TryLockError::Poisoned(_)) => {
                return Err(eyre!("creds store process lock was poisoned"));
            }
        }
    }
}

fn lock_timeout_result<T>(store_path: &Path, mode: StoreLockMode) -> eyre::Result<Option<T>> {
    match mode {
        StoreLockMode::Optional => Ok(None),
        StoreLockMode::Required => Err(eyre!(
            "creds store is busy; another fj-ex process is updating '{}'. Retry shortly.",
            store_path.display()
        )),
    }
}

fn lock_path(store_path: &Path) -> PathBuf {
    let mut file_name = store_path
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("ui-creds.json"));
    file_name.push(".lock");
    store_path.with_file_name(file_name)
}
