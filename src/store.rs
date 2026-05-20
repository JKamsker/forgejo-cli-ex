use std::path::PathBuf;

use eyre::eyre;
use time::OffsetDateTime;

mod creds;
mod file;
mod keys;
mod lock;
mod repair;

#[cfg(test)]
mod creds_tests;

pub use creds::{
    clear_cookie_jar, delete_store_entry, get_store_entry, get_ui_creds, read_creds_store,
    save_cookie_jar, set_ui_creds, CookieJar, CookieRecord, StoreEntry,
};
pub use keys::get_fj_api_token_for_base_url;

#[derive(Clone, Debug)]
pub struct StorePaths {
    pub dir: PathBuf,
    pub path: PathBuf,
}

fn app_data_base_dir() -> eyre::Result<PathBuf> {
    std::env::var_os("APPDATA")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|d| d.data_dir().to_path_buf()))
        .ok_or_else(|| eyre!("unable to locate AppData directory"))
}

pub fn ui_creds_store_paths() -> eyre::Result<StorePaths> {
    let app_data = app_data_base_dir()?;

    let dir = app_data.join("Cyborus").join("forgejo-cli").join("data");
    let path = dir.join("ui-creds.json");
    Ok(StorePaths { dir, path })
}

pub fn keys_store_paths() -> eyre::Result<StorePaths> {
    let app_data = app_data_base_dir()?;

    let dir = app_data.join("Cyborus").join("forgejo-cli").join("data");
    let path = dir.join("keys.json");
    Ok(StorePaths { dir, path })
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| OffsetDateTime::now_utc().unix_timestamp().to_string())
}
