use std::future::Future;
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eyre::{eyre, Context, Result};

/// Short random-ish hex suffix, unique per process and call (no RNG dependency).
pub fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or_default();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // splitmix64 over time, pid and a counter.
    let mut x =
        nanos ^ (u64::from(std::process::id()) << 32) ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    format!("{:08x}", x as u32)
}

/// Ask the OS for a currently free TCP port on 127.0.0.1.
///
/// There is a small window before Docker binds it; callers retry on
/// "port is already allocated".
pub fn pick_free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").wrap_err("failed to bind an ephemeral port")?;
    Ok(listener.local_addr()?.port())
}

pub fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref().map(str::trim),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Marker for an error [`poll`] must not retry (e.g. the container exited).
#[derive(Debug)]
pub struct Fatal(pub String);

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Fatal {}

/// An error that makes [`poll`] stop at once instead of retrying.
pub fn fatal(msg: impl Into<String>) -> eyre::Report {
    eyre::Report::new(Fatal(msg.into()))
}

fn is_fatal(e: &eyre::Report) -> bool {
    e.chain().any(|c| c.is::<Fatal>())
}

/// Poll `check` until it returns `Ok(Some(_))` or `timeout` elapses.
///
/// `Ok(None)` means "not yet"; an `Err` is remembered and reported on timeout
/// (transient errors are expected while servers start up). An error made with
/// [`fatal`] is returned at once.
pub async fn poll<T, F, Fut>(
    what: &str,
    timeout: Duration,
    interval: Duration,
    mut check: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<T>>>,
{
    let started = Instant::now();
    let mut last_err: Option<eyre::Report> = None;
    loop {
        match check().await {
            Ok(Some(v)) => return Ok(v),
            Ok(None) => {}
            Err(e) if is_fatal(&e) => {
                return Err(e.wrap_err(format!("gave up waiting for {what}")));
            }
            Err(e) => last_err = Some(e),
        }
        if started.elapsed() >= timeout {
            return Err(match last_err {
                Some(e) => e.wrap_err(format!("timed out after {timeout:?} waiting for {what}")),
                None => eyre!("timed out after {timeout:?} waiting for {what}"),
            });
        }
        tokio::time::sleep(interval).await;
    }
}

/// Minimal standard base64 (used for an HTTP Basic header passed to git).
pub fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Last `max_lines` lines of `text`.
pub fn tail(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"e2e:secret"), "ZTJlOnNlY3JldA==");
    }

    #[tokio::test]
    async fn poll_returns_fatal_errors_at_once() {
        let mut calls = 0u32;
        let started = Instant::now();
        let res: Result<()> = poll(
            "a dead thing",
            Duration::from_secs(30),
            Duration::from_millis(10),
            || {
                calls += 1;
                async { Err(fatal("container exited")) }
            },
        )
        .await;
        let err = res.expect_err("fatal error must fail the poll");
        assert_eq!(calls, 1);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(format!("{err:?}").contains("container exited"));
    }

    #[tokio::test]
    async fn poll_retries_ordinary_errors_until_success() {
        let mut calls = 0u32;
        let v = poll(
            "a flaky thing",
            Duration::from_secs(30),
            Duration::from_millis(1),
            || {
                calls += 1;
                let n = calls;
                async move {
                    if n < 3 {
                        Err(eyre!("not yet"))
                    } else {
                        Ok(Some(n))
                    }
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(v, 3);
    }

    #[test]
    fn unique_suffixes_differ() {
        assert_ne!(unique_suffix(), unique_suffix());
    }
}
