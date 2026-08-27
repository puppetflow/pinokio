use std::process::Stdio;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde::Deserialize;
use tempfile::TempDir;
use tokio::process::{Child, Command};
use tracing::{debug, warn};

use crate::config::Config;
use crate::errors::GatewayError;

const DEVTOOLS_PORT_FILE: &str = "DevToolsActivePort";
const PORT_FILE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const SIGTERM_GRACE: Duration = Duration::from_secs(3);

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchOptions {
    pub proxy_server: Option<String>,
    pub proxy_bypass_list: Option<String>,
    pub disable_web_security: Option<bool>,
}

impl LaunchOptions {
    pub fn validate(self) -> Result<Self, GatewayError> {
        if let Some(proxy_server) = &self.proxy_server
            && (proxy_server.len() > 2048
                || proxy_server.chars().any(char::is_control)
                || !["http://", "https://", "socks4://", "socks5://"]
                    .iter()
                    .any(|scheme| proxy_server.starts_with(scheme)))
        {
            return Err(GatewayError::InvalidLaunchOptions(
                "proxyServer must be a valid HTTP, HTTPS, SOCKS4, or SOCKS5 URL".into(),
            ));
        }
        if let Some(proxy_bypass_list) = &self.proxy_bypass_list
            && (proxy_bypass_list.len() > 2048 || proxy_bypass_list.chars().any(char::is_control))
        {
            return Err(GatewayError::InvalidLaunchOptions(
                "proxyBypassList contains invalid characters".into(),
            ));
        }

        Ok(self)
    }
}

/// A single Chromium process bound to one session.
pub struct Chromium {
    child: Child,
    pgid: Pid,
    /// Kept for the lifetime of the process; removed on shutdown.
    user_data_dir: Option<TempDir>,
    /// Browser-level CDP WebSocket URL, always on 127.0.0.1.
    pub ws_url: String,
}

/// Launches an isolated Chromium and waits until it publishes its CDP
/// endpoint through the DevToolsActivePort file in its user data dir.
pub async fn launch(
    config: &Config,
    launch_options: &LaunchOptions,
) -> Result<Chromium, GatewayError> {
    let user_data_dir = TempDir::with_prefix("pinokio-")
        .map_err(|e| GatewayError::ChromiumUnavailable(format!("temp dir creation failed: {e}")))?;

    let mut args: Vec<String> = Vec::new();
    if config.chrome_headless {
        args.push("--headless=new".into());
    }
    args.push("--remote-debugging-port=0".into());
    args.push(format!(
        "--user-data-dir={}",
        user_data_dir.path().display()
    ));
    args.extend(
        [
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-sync",
            "--metrics-recording-only",
            "--disable-default-apps",
        ]
        .map(String::from),
    );
    if config.chrome_no_sandbox {
        args.push("--no-sandbox".into());
    }
    if config.chrome_disable_dev_shm_usage {
        args.push("--disable-dev-shm-usage".into());
    }
    if let Some(language) = &config.language {
        // LANGUAGE may hold a list like "fr-FR:fr"; Chromium wants one tag.
        if let Some(tag) = language.split(':').next() {
            args.push(format!("--lang={tag}"));
        }
    }
    if let Some(proxy_server) = &launch_options.proxy_server {
        args.push(format!("--proxy-server={proxy_server}"));
    }
    if let Some(proxy_bypass_list) = &launch_options.proxy_bypass_list {
        args.push(format!("--proxy-bypass-list={proxy_bypass_list}"));
    }
    if launch_options.disable_web_security == Some(true) {
        args.push("--disable-web-security".into());
    }
    args.extend(config.chrome_extra_args.iter().cloned());

    let mut command = Command::new(&config.chrome_path);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    // Run Chromium in its own session/process group so the whole tree can
    // be signaled at once without touching unrelated processes.
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid().map_err(std::io::Error::from)?;
            Ok(())
        });
    }

    let mut child = command
        .spawn()
        .map_err(|e| GatewayError::ChromiumUnavailable(format!("spawn failed: {e}")))?;

    let pid = child
        .id()
        .ok_or_else(|| GatewayError::ChromiumUnavailable("spawned process has no pid".into()))?;
    let pgid = Pid::from_raw(pid as i32);

    match wait_for_devtools_endpoint(&mut child, &user_data_dir, config.chrome_startup_timeout)
        .await
    {
        Ok(ws_url) => {
            debug!(pid, "chromium ready");
            Ok(Chromium {
                child,
                pgid,
                user_data_dir: Some(user_data_dir),
                ws_url,
            })
        }
        Err(err) => {
            // Startup failed: kill the process tree and remove the temp dir
            // before surfacing the error.
            let mut failed = Chromium {
                child,
                pgid,
                user_data_dir: Some(user_data_dir),
                ws_url: String::new(),
            };
            failed.shutdown().await;
            Err(err)
        }
    }
}

/// Polls the DevToolsActivePort file until Chromium writes its CDP port and
/// browser target path, or the startup timeout expires, or the process dies.
async fn wait_for_devtools_endpoint(
    child: &mut Child,
    user_data_dir: &TempDir,
    timeout: Duration,
) -> Result<String, GatewayError> {
    let port_file = user_data_dir.path().join(DEVTOOLS_PORT_FILE);
    let deadline = Instant::now() + timeout;

    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| GatewayError::ChromiumUnavailable(format!("wait failed: {e}")))?
        {
            return Err(GatewayError::ChromiumUnavailable(format!(
                "chromium exited during startup with {status}"
            )));
        }

        if let Ok(contents) = tokio::fs::read_to_string(&port_file).await {
            let mut lines = contents.lines();
            let port = lines.next().and_then(|l| l.trim().parse::<u16>().ok());
            let path = lines.next().map(str::trim);
            if let (Some(port), Some(path)) = (port, path)
                && port > 0
                && path.starts_with('/')
            {
                return Ok(format!("ws://127.0.0.1:{port}{path}"));
            }
        }

        if Instant::now() >= deadline {
            return Err(GatewayError::ChromiumStartupTimeout);
        }
        tokio::time::sleep(PORT_FILE_POLL_INTERVAL).await;
    }
}

impl Chromium {
    /// Terminates the Chromium process group and removes the temp dir.
    ///
    /// SIGTERM first, then SIGKILL after a short grace period. The child is
    /// always reaped through `wait()` so it never becomes a zombie. Signals
    /// only target this session's process group, never other sessions.
    pub async fn shutdown(&mut self) {
        if self.user_data_dir.is_none() {
            // Already shut down.
            return;
        }

        if self.child.try_wait().ok().flatten().is_none() {
            if let Err(e) = killpg(self.pgid, Signal::SIGTERM) {
                debug!(pid = self.pgid.as_raw(), "SIGTERM failed: {e}");
            }
            let terminated = tokio::time::timeout(SIGTERM_GRACE, self.child.wait())
                .await
                .is_ok();
            if !terminated {
                if let Err(e) = killpg(self.pgid, Signal::SIGKILL) {
                    warn!(pid = self.pgid.as_raw(), "SIGKILL failed: {e}");
                }
                if let Err(e) = self.child.wait().await {
                    warn!(pid = self.pgid.as_raw(), "reaping chromium failed: {e}");
                }
            }
        }

        // Best-effort sweep for stragglers left in the process group after
        // the main process was reaped (SIGKILL to an empty group is a no-op).
        let _ = killpg(self.pgid, Signal::SIGKILL);

        if let Some(dir) = self.user_data_dir.take()
            && let Err(e) = dir.close()
        {
            warn!("failed to remove chromium temp dir: {e}");
        }
    }
}
