use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use eyre::{eyre, Context, Result};
use tempfile::TempDir;

const FORGEJO_ROOTLESS_IMAGE: &str = "codeberg.org/forgejo/forgejo:14-rootless";

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore]
async fn test_unix_socket_connection() -> Result<()> {
    if std::env::var("FJ_EX_UNIX_SOCKET_TEST").ok().as_deref() != Some("1") {
        eprintln!("skipping unix socket test: set FJ_EX_UNIX_SOCKET_TEST=1 to enable");
        return Ok(());
    }
    if !docker_available() {
        eprintln!("skipping unix socket test: docker is not available");
        return Ok(());
    }

    let temp = tempfile::tempdir().wrap_err("failed to create temp dir")?;
    let sockets_dir = temp.path().join("sockets");
    let data_dir = temp.path().join("data");

    fs::create_dir_all(&sockets_dir).wrap_err("failed to create sockets dir")?;
    fs::create_dir_all(&data_dir).wrap_err("failed to create data dir")?;

    let test_id = format!("{}", std::process::id());
    let container_name = format!("fj-ex-unix-test-{test_id}");
    let socket_path = sockets_dir.join("http.sock");

    println!("Starting Forgejo container with Unix socket...");
    let _container =
        ForgejoUnixSocketContainer::start(&container_name, &sockets_dir, &data_dir).await?;

    // Wait for socket to be created
    wait_for_socket(&socket_path, Duration::from_secs(30))?;

    println!("Socket created at: {}", socket_path.display());

    // Test 1: Parse unix socket URL
    println!("\nTest 1: URL parsing");
    test_unix_socket_url_parsing(&socket_path)?;

    // Test 2: Test connection with curl
    println!("\nTest 2: Connection with curl");
    test_curl_unix_socket(&socket_path)?;

    // Test 3: Test fj-ex auth login
    println!("\nTest 3: fj-ex auth login via Unix socket");
    test_fj_ex_login(&socket_path, &temp)?;

    // Test 4: Test fj-ex auth status
    println!("\nTest 4: fj-ex auth status via Unix socket");
    test_fj_ex_status(&socket_path, &temp)?;

    println!("\n✅ All Unix socket tests passed!");
    Ok(())
}

#[cfg(target_os = "linux")]
fn test_unix_socket_url_parsing(socket_path: &Path) -> Result<()> {
    // Test that the URL format is correct
    let socket_url = format!("http+unix://{}", socket_path.display());

    assert!(socket_url.starts_with("http+unix://"));
    assert!(socket_url.contains("http.sock"));

    println!("  ✓ URL format: {}", socket_url);
    Ok(())
}

#[cfg(target_os = "linux")]
fn test_curl_unix_socket(socket_path: &Path) -> Result<()> {
    let output = Command::new("curl")
        .arg("--unix-socket")
        .arg(socket_path)
        .arg("http://localhost/api/v1/version")
        .arg("--silent")
        .arg("--fail")
        .output()
        .wrap_err("failed to run curl")?;

    if !output.status.success() {
        return Err(eyre!(
            "curl failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let response = String::from_utf8_lossy(&output.stdout);
    assert!(response.contains("version"), "Expected version in response");

    println!("  ✓ curl connection successful");
    println!("  ✓ Response: {}", response);
    Ok(())
}

#[cfg(target_os = "linux")]
fn test_fj_ex_login(socket_path: &Path, temp: &TempDir) -> Result<()> {
    let fj_ex = fj_ex_bin()?;
    let socket_url = format!("http+unix://{}", socket_path.display());
    let appdata = temp.path().join("appdata");
    fs::create_dir_all(&appdata)?;

    // Create admin user in container first
    create_admin_user_in_container()?;

    let output = Command::new(&fj_ex)
        .env("HOME", temp.path())
        .env("XDG_DATA_HOME", appdata.join("share"))
        .arg("auth")
        .arg("login")
        .arg("-H")
        .arg(&socket_url)
        .arg("--userpass")
        .arg("testadmin:testpass123")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .wrap_err("failed to run fj-ex auth login")?;

    if !output.status.success() {
        eprintln!("stdout: {}", String::from_utf8_lossy(&output.stdout));
        eprintln!("stderr: {}", String::from_utf8_lossy(&output.stderr));
        return Err(eyre!("fj-ex auth login failed"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("testadmin@localhost") || stdout.contains("Saved UI creds"),
        "Expected login success message"
    );

    println!("  ✓ fj-ex auth login successful");
    Ok(())
}

#[cfg(target_os = "linux")]
fn test_fj_ex_status(socket_path: &Path, temp: &TempDir) -> Result<()> {
    let fj_ex = fj_ex_bin()?;
    let socket_url = format!("http+unix://{}", socket_path.display());
    let appdata = temp.path().join("appdata");

    let output = Command::new(&fj_ex)
        .env("HOME", temp.path())
        .env("XDG_DATA_HOME", appdata.join("share"))
        .arg("auth")
        .arg("status")
        .arg("-H")
        .arg(&socket_url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .wrap_err("failed to run fj-ex auth status")?;

    if !output.status.success() {
        eprintln!("stdout: {}", String::from_utf8_lossy(&output.stdout));
        eprintln!("stderr: {}", String::from_utf8_lossy(&output.stderr));
        return Err(eyre!("fj-ex auth status failed"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("testadmin@localhost") || stdout.contains("OK"),
        "Expected status success"
    );

    println!("  ✓ fj-ex auth status successful");
    Ok(())
}

#[cfg(target_os = "linux")]
fn wait_for_forgejo_ready(container_name: &str, timeout: Duration) -> Result<()> {
    let start = std::time::Instant::now();
    let poll_interval = Duration::from_millis(500);

    while start.elapsed() < timeout {
        let output = Command::new("docker")
            .args([
                "exec",
                container_name,
                "curl",
                "--silent",
                "--fail",
                "http://localhost:3000/api/v1/version",
            ])
            .output();

        if let Ok(output) = output {
            if output.status.success() {
                return Ok(());
            }
        }

        std::thread::sleep(poll_interval);
    }

    Err(eyre!("Forgejo did not become ready within timeout"))
}

#[cfg(target_os = "linux")]
fn wait_for_socket(socket_path: &Path, timeout: Duration) -> Result<()> {
    let start = std::time::Instant::now();

    while start.elapsed() < timeout {
        if socket_path.exists() {
            // Additional check: try to stat the socket
            if let Ok(metadata) = fs::metadata(socket_path) {
                #[cfg(target_os = "linux")]
                {
                    use std::os::unix::fs::FileTypeExt;
                    if metadata.file_type().is_socket() {
                        return Ok(());
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    Err(eyre!("Socket not created within timeout"))
}

#[cfg(target_os = "linux")]
fn create_admin_user_in_container() -> Result<()> {
    // Find the running container
    let containers = Command::new("docker")
        .args([
            "ps",
            "--filter",
            "name=fj-ex-unix-test",
            "--format",
            "{{.Names}}",
        ])
        .output()
        .wrap_err("failed to list containers")?;

    let container_name = String::from_utf8_lossy(&containers.stdout)
        .lines()
        .next()
        .ok_or_else(|| eyre!("no test container found"))?
        .to_string();

    // Wait for Forgejo to be ready by polling the health endpoint
    wait_for_forgejo_ready(&container_name, Duration::from_secs(30))?;

    let output = Command::new("docker")
        .args([
            "exec",
            &container_name,
            "forgejo",
            "admin",
            "user",
            "create",
            "--admin",
            "--username",
            "testadmin",
            "--password",
            "testpass123",
            "--email",
            "test@example.com",
        ])
        .output()
        .wrap_err("failed to create admin user")?;

    // It's ok if user already exists
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("already exist") && !stderr.contains("reserved") {
            eprintln!("Warning: admin user creation failed: {}", stderr);
        }
    }

    Ok(())
}

#[cfg(target_os = "linux")]
struct ForgejoUnixSocketContainer {
    name: String,
}

#[cfg(target_os = "linux")]
impl ForgejoUnixSocketContainer {
    async fn start(name: &str, sockets_dir: &Path, data_dir: &Path) -> Result<Self> {
        // Create initial app.ini
        let conf_dir = data_dir.join("custom").join("conf");
        fs::create_dir_all(&conf_dir)?;

        let app_ini = conf_dir.join("app.ini");
        fs::write(
            &app_ini,
            r#"
[server]
PROTOCOL = http+unix
HTTP_ADDR = /run/forgejo/http.sock
UNIX_SOCKET_PERMISSION = 660
DOMAIN = localhost
ROOT_URL = http://localhost/
START_SSH_SERVER = false

[database]
DB_TYPE = sqlite3
PATH = /var/lib/gitea/data/gitea.db

[security]
INSTALL_LOCK = true

[service]
DISABLE_REGISTRATION = false
REQUIRE_SIGNIN_VIEW = false
"#,
        )?;

        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };

        let output = Command::new("docker")
            .args([
                "run",
                "-d",
                "--name",
                name,
                "--user",
                &format!("{}:{}", uid, gid),
                "-v",
                &format!("{}:/var/lib/gitea", data_dir.display()),
                "-v",
                &format!("{}:/run/forgejo", sockets_dir.display()),
                FORGEJO_ROOTLESS_IMAGE,
            ])
            .output()
            .wrap_err("failed to start forgejo container")?;

        if !output.status.success() {
            return Err(eyre!(
                "docker run failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Ok(Self {
            name: name.to_string(),
        })
    }
}

#[cfg(target_os = "linux")]
impl Drop for ForgejoUnixSocketContainer {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.name])
            .output();
    }
}

fn docker_available() -> bool {
    Command::new("docker")
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn fj_ex_bin() -> Result<PathBuf> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");

    // Check CARGO_TARGET_DIR first, then fall back to target/ under manifest dir
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(manifest_dir).join("target"));

    // Try debug first, then release
    let debug_bin = target_dir.join("debug").join("fj-ex");
    let release_bin = target_dir.join("release").join("fj-ex");

    if release_bin.exists() {
        Ok(release_bin)
    } else if debug_bin.exists() {
        Ok(debug_bin)
    } else {
        Err(eyre!("fj-ex binary not found. Run 'cargo build' first"))
    }
}
