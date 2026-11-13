use std::{
    collections::HashMap,
    ffi::OsString,
    fs::File,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex, MutexGuard, TryLockError},
    time::{Duration, Instant},
};

use eyre::{eyre, Context};
use fs2::FileExt;

use super::StorePaths;

const REQUIRED_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
// Cookie persistence is best-effort; polling commands should skip it if the store is busy.
const OPTIONAL_LOCK_TIMEOUT: Duration = Duration::from_millis(0);
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(20);

// One in-process mutex per store file. Production uses a single store; keying by
// path keeps independent stores (e.g. parallel tests) from contending, which
// would make best-effort (optional) saves skip spuriously.
static CREDS_STORE_PROCESS_MUTEXES: LazyLock<Mutex<HashMap<PathBuf, &'static Mutex<()>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn process_mutex_for(store_path: &Path) -> &'static Mutex<()> {
    let mut mutexes = CREDS_STORE_PROCESS_MUTEXES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    mutexes
        .entry(store_path.to_path_buf())
        .or_insert_with(|| Box::leak(Box::new(Mutex::new(()))))
}

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
    super::file::create_private_dir_all(&paths.dir)?;

    let timeout = mode.timeout();
    let Some(process_guard) = acquire_process_lock(&paths.path, mode, timeout)? else {
        return Ok(None);
    };

    let lock_path = lock_path(&paths.path);
    let lock_file = super::file::private_open_options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
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
    let mutex = process_mutex_for(store_path);
    let start = Instant::now();
    loop {
        match mutex.try_lock() {
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
