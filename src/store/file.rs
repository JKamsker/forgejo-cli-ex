use std::{
    ffi::OsString,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use eyre::Context;
use time::OffsetDateTime;

use super::{
    creds::{remove_entries_without_complete_creds, CredsStore},
    lock::{acquire_store_lock, StoreLockMode},
    repair::repair_creds_store_from_raw,
    StorePaths,
};

struct StoreLoad {
    store: CredsStore,
    repaired: bool,
}

pub(super) fn update_creds_store<T>(
    paths: &StorePaths,
    mode: StoreLockMode,
    update: impl FnOnce(&mut CredsStore) -> eyre::Result<(T, bool)>,
) -> eyre::Result<Option<T>> {
    let Some(_lock) = acquire_store_lock(paths, mode)? else {
        return Ok(None);
    };

    let mut load = read_creds_store_unlocked(&paths.path, true)?;
    let (value, changed) = update(&mut load.store)?;
    let removed_invalid_entries = remove_entries_without_complete_creds(&mut load.store);

    if changed || load.repaired || removed_invalid_entries > 0 {
        write_creds_store_atomic_unlocked(paths, &load.store)?;
    }

    Ok(Some(value))
}

pub(super) fn read_creds_store_with_paths(paths: &StorePaths) -> eyre::Result<CredsStore> {
    match read_creds_store_unlocked(&paths.path, false) {
        Ok(mut load) => {
            if remove_entries_without_complete_creds(&mut load.store) > 0 {
                return read_cleaned_creds_store_with_lock(paths);
            }
            Ok(load.store)
        }
        Err(_) => read_cleaned_creds_store_with_lock(paths),
    }
}

fn read_cleaned_creds_store_with_lock(paths: &StorePaths) -> eyre::Result<CredsStore> {
    let Some(_lock) = acquire_store_lock(paths, StoreLockMode::Required)? else {
        unreachable!("required creds store lock cannot be skipped");
    };

    let mut load = read_creds_store_unlocked(&paths.path, true)?;
    let removed_invalid_entries = remove_entries_without_complete_creds(&mut load.store);
    if load.repaired || removed_invalid_entries > 0 {
        write_creds_store_atomic_unlocked(paths, &load.store)?;
    }
    Ok(load.store)
}

fn read_creds_store_unlocked(store_path: &Path, repair_invalid: bool) -> eyre::Result<StoreLoad> {
    if !store_path.is_file() {
        return Ok(StoreLoad {
            store: CredsStore::default(),
            repaired: false,
        });
    }

    let raw = std::fs::read_to_string(store_path)
        .wrap_err_with(|| format!("failed to read creds store at '{}'", store_path.display()))?;
    if raw.trim().is_empty() {
        return Ok(StoreLoad {
            store: CredsStore::default(),
            repaired: false,
        });
    }

    match serde_json::from_str::<CredsStore>(&raw) {
        Ok(store) => Ok(StoreLoad {
            store,
            repaired: false,
        }),
        Err(err) if repair_invalid => repair_invalid_store(store_path, &raw, err),
        Err(err) => Err(err)
            .wrap_err_with(|| format!("invalid creds store JSON at '{}'", store_path.display())),
    }
}

fn repair_invalid_store(
    store_path: &Path,
    raw: &str,
    err: serde_json::Error,
) -> eyre::Result<StoreLoad> {
    let backup = backup_path(store_path)?;
    let _ = std::fs::copy(store_path, &backup);

    let repaired = repair_creds_store_from_raw(raw)?;
    if !repaired.is_empty() {
        eprintln!(
            "warn: ui-creds.json was invalid JSON; backed up to '{}' and repaired by dropping cookie jars.",
            backup.display()
        );
        return Ok(StoreLoad {
            store: repaired,
            repaired: true,
        });
    }

    Err(err).wrap_err_with(|| {
        format!(
            "invalid creds store JSON at '{}'. Backed up to '{}'. Re-run `fj-ex auth login` (or legacy `fj-ex login`) to recreate.",
            store_path.display(),
            backup.display()
        )
    })
}

fn backup_path(original: &Path) -> eyre::Result<PathBuf> {
    let stamp = OffsetDateTime::now_utc()
        .format(&time::format_description::parse(
            "[year][month][day]T[hour][minute][second]Z",
        )?)
        .wrap_err("failed to format timestamp")?;
    Ok(PathBuf::from(format!(
        "{}.bad.{stamp}.json",
        original.display()
    )))
}

fn write_creds_store_atomic_unlocked(paths: &StorePaths, store: &CredsStore) -> eyre::Result<()> {
    std::fs::create_dir_all(&paths.dir)
        .wrap_err_with(|| format!("failed to create creds store dir '{}'", paths.dir.display()))?;

    let mut store = store.clone();
    remove_entries_without_complete_creds(&mut store);

    let json = serde_json::to_vec_pretty(&store).wrap_err("failed to serialize creds store")?;
    let temp_path = temp_store_path(&paths.path);

    let write_result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(&json)?;
        file.sync_all()?;
        drop(file);

        replace_file(&temp_path, &paths.path)?;
        sync_parent_dir(&paths.dir);
        Ok(())
    })();

    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err).wrap_err_with(|| {
            format!(
                "failed to atomically write creds store '{}'",
                paths.path.display()
            )
        });
    }

    Ok(())
}

fn temp_store_path(store_path: &Path) -> PathBuf {
    let mut file_name = store_path
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("ui-creds.json"));
    file_name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    store_path.with_file_name(file_name)
}

#[cfg(not(windows))]
fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(source, target)
}

#[cfg(windows)]
fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();

    let flags = MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH;
    let ok = unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), flags) };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn sync_parent_dir(dir: &Path) {
    if let Ok(file) = File::open(dir) {
        let _ = file.sync_all();
    }
}
