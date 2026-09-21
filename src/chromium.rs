use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::process::{Child, Command};
use tracing::{debug, warn};

use crate::config::{BrowserEngine, Config};
use crate::errors::GatewayError;

const DEVTOOLS_PORT_FILE: &str = "DevToolsActivePort";
const PORT_FILE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const SIGTERM_GRACE: Duration = Duration::from_secs(3);
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// Identity of the browser binary Pinokio launches. Computed once at startup
/// and exposed on `/status` so clients can tell which build served a run:
/// `--version` alone cannot distinguish a stock Chromium from a patched build
/// of the same release (patched builds, Chrome for Testing...), the hash can.
#[derive(Debug, Clone, Serialize)]
pub struct BrowserInfo {
    /// Bundled engine in use, or `custom` for CHROME_PATH and /opt/browser.
    pub engine: BrowserEngine,
    /// Product name from `--version`, e.g. "Chromium" or "Google Chrome".
    pub name: Option<String>,
    /// Version number from `--version`, e.g. "146.0.7680.177".
    pub version: Option<String>,
    /// Lowercase hex SHA-256 of the executable file.
    pub sha256: Option<String>,
    pub path: String,
}

/// Resolves the browser identity: `--version` and the executable hash run
/// concurrently. Hashing a 400 MB binary takes a couple of seconds, so it
/// runs on the blocking pool; a failure only yields `None`, never an error.
pub async fn identify(config: &Config) -> BrowserInfo {
    let hash_path = config.chrome_path.clone();
    let (version_output, sha256) = tokio::join!(
        version(config),
        tokio::task::spawn_blocking(move || sha256_file(&hash_path))
    );
    let sha256 = match sha256 {
        Ok(Ok(hash)) => Some(hash),
        Ok(Err(e)) => {
            warn!("browser binary hash failed: {e}");
            None
        }
        Err(e) => {
            warn!("browser binary hash task failed: {e}");
            None
        }
    };
    let (name, version) = match version_output.as_deref() {
        Some(output) => split_version_output(output),
        None => (None, None),
    };
    BrowserInfo {
        engine: config.browser_engine,
        name,
        version,
        sha256,
        path: config.chrome_path.display().to_string(),
    }
}

/// Splits `--version` output into (name, version): "Google Chrome 146.0.7680.177"
/// gives ("Google Chrome", "146.0.7680.177"). The version is the first
/// whitespace-separated token that looks like a dotted number; the name is
/// everything before it and anything after it is dropped (Debian appends
/// "built on Debian GNU/Linux 13 (trixie)"). Without such a token the whole
/// output is kept as the name.
fn split_version_output(output: &str) -> (Option<String>, Option<String>) {
    let tokens: Vec<&str> = output.split_whitespace().collect();
    let is_version = |token: &str| {
        token.contains('.')
            && token.chars().all(|c| c.is_ascii_digit() || c == '.')
            && token.chars().next().is_some_and(|c| c.is_ascii_digit())
    };
    match tokens.iter().position(|token| is_version(token)) {
        Some(index) if index > 0 => (
            Some(tokens[..index].join(" ")),
            Some(tokens[index].to_string()),
        ),
        Some(index) => (None, Some(tokens[index].to_string())),
        None => ((!tokens.is_empty()).then(|| tokens.join(" ")), None),
    }
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Runs `<binary> --version` and returns its trimmed output, or `None` when
/// the binary does not answer. Used for startup diagnostics only, so operators
/// can confirm which browser (stock Chromium, Chrome, a patched build...) is in use.
pub async fn version(config: &Config) -> Option<String> {
    let mut command = Command::new(&config.chrome_path);
    if config.chrome_no_sandbox {
        command.arg("--no-sandbox");
    }
    command
        .arg("--version")
        .stdin(Stdio::null())
        .kill_on_drop(true);

    let output = tokio::time::timeout(VERSION_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchOptions {
    pub proxy_server: Option<String>,
    pub proxy_bypass_list: Option<String>,
    pub disable_web_security: Option<bool>,
    /// Comma-separated BCP 47 tags, e.g. "fr-FR,fr". Overrides the LANGUAGE
    /// environment variable for this session.
    pub accept_language: Option<String>,
    /// User-Agent applied browser-wide with `--user-agent`, so pages, dedicated,
    /// shared and service workers all report the same string. Defaults to the
    /// binary's own UA without the "Headless" marker.
    pub user_agent: Option<String>,
    /// Page viewport the client will use. The window is sized so the inner
    /// viewport matches it, and the emulated screen is a standard resolution
    /// that fits the window, as on a desktop.
    pub viewport: Option<Viewport>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

/// Height of the tab strip and toolbar on a Linux Chrome window: outerHeight
/// exceeds innerHeight by this much on a real desktop.
const WINDOW_CHROME_HEIGHT: u32 = 87;
const DEFAULT_VIEWPORT: Viewport = Viewport {
    width: 1280,
    height: 720,
};
/// Common desktop resolutions, smallest first; the first one that fits the
/// window is reported as the screen.
const SCREEN_SIZES: [(u32, u32); 3] = [(1920, 1080), (2560, 1440), (3840, 2160)];

impl Viewport {
    fn window_size(self) -> (u32, u32) {
        (self.width, self.height + WINDOW_CHROME_HEIGHT)
    }

    fn screen_size(self) -> (u32, u32) {
        let (width, height) = self.window_size();
        SCREEN_SIZES
            .into_iter()
            .find(|(screen_width, screen_height)| width <= *screen_width && height <= *screen_height)
            .unwrap_or((width, height))
    }
}

/// Headed-desktop equivalent of the binary's headless UA: Chromium on Linux
/// reports "X11; Linux x86_64" whatever the CPU, and the reduced UA keeps only
/// the major version.
pub fn default_user_agent(browser_version: Option<&str>) -> String {
    let major = browser_version
        .and_then(|version| version.split('.').next())
        .filter(|major| !major.is_empty() && major.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or("0");
    format!(
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36"
    )
}

/// Loose BCP 47 check: subtags of 1 to 8 alphanumerics separated by "-".
fn is_language_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 35
        && tag.split('-').all(|subtag| {
            !subtag.is_empty()
                && subtag.len() <= 8
                && subtag.chars().all(|c| c.is_ascii_alphanumeric())
        })
}

/// Turns "fr-FR:fr" or "fr-FR,fr" into ("fr-FR", "fr-FR,fr"), appending the
/// bare language when only a regional tag was given so Accept-Language keeps
/// a fallback. Returns None when no valid tag is present.
fn language_args(raw: &str) -> Option<(String, String)> {
    let tags: Vec<&str> = raw
        .split([':', ','])
        .map(str::trim)
        .filter(|tag| is_language_tag(tag))
        .collect();
    let primary = *tags.first()?;
    let mut accept: Vec<String> = tags.iter().map(|tag| tag.to_string()).collect();
    if let Some(base) = primary.split('-').next()
        && base != primary
        && !accept
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(base))
    {
        accept.push(base.to_string());
    }
    Some((primary.to_string(), accept.join(",")))
}

impl LaunchOptions {
    pub fn validate(self) -> Result<Self, GatewayError> {
        if let Some(accept_language) = &self.accept_language
            && (accept_language.len() > 256 || language_args(accept_language).is_none())
        {
            return Err(GatewayError::InvalidLaunchOptions(
                "acceptLanguage must be a comma-separated list of BCP 47 language tags".into(),
            ));
        }
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
        if let Some(user_agent) = &self.user_agent
            && (user_agent.trim().is_empty()
                || user_agent.len() > 512
                || !user_agent
                    .chars()
                    .all(|c| c.is_ascii_graphic() || c == ' '))
        {
            return Err(GatewayError::InvalidLaunchOptions(
                "userAgent must be printable ASCII of at most 512 characters".into(),
            ));
        }
        if let Some(viewport) = &self.viewport
            && !((100..=7680).contains(&viewport.width) && (100..=4320).contains(&viewport.height))
        {
            return Err(GatewayError::InvalidLaunchOptions(
                "viewport width must be 100-7680 and height 100-4320".into(),
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
    browser_version: Option<&str>,
) -> Result<Chromium, GatewayError> {
    let user_data_dir = TempDir::with_prefix("pinokio-")
        .map_err(|e| GatewayError::ChromiumUnavailable(format!("temp dir creation failed: {e}")))?;

    let mut args: Vec<String> = Vec::new();
    if config.chrome_headless {
        args.push("--headless=new".into());
        // New headless differs from a desktop Chrome in ways every bot check
        // looks at: navigator.webdriver is true, the screen is 800x600 whatever
        // the window, media queries report no pointer and no hover device, and
        // there is not a single media device. Bring those back to desktop values.
        args.push("--disable-blink-features=AutomationControlled".into());
        args.push(
            "--blink-settings=primaryPointerType=4,availablePointerTypes=4,primaryHoverType=2,availableHoverTypes=2"
                .into(),
        );
        args.push("--use-fake-device-for-media-stream".into());
        let viewport = launch_options.viewport.unwrap_or(DEFAULT_VIEWPORT);
        let (window_width, window_height) = viewport.window_size();
        let (screen_width, screen_height) = viewport.screen_size();
        args.push(format!("--window-size={window_width},{window_height}"));
        args.push(format!("--screen-info={{{screen_width}x{screen_height}}}"));
    }
    // --user-agent is the only override that reaches shared and service
    // workers, which a CDP Emulation override on the page never does.
    let user_agent = launch_options
        .user_agent
        .clone()
        .unwrap_or_else(|| default_user_agent(browser_version));
    args.push(format!("--user-agent={user_agent}"));
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
    // Per-session acceptLanguage wins over the server-wide LANGUAGE variable.
    // On Linux, Chromium picks its application locale (UI strings and the
    // default JavaScript Intl locale) from the LANGUAGE environment variable
    // and ignores --lang for that, so the primary tag is passed as LANGUAGE to
    // the process, otherwise every session would inherit the server-wide
    // value. --accept-lang controls navigator.languages and Accept-Language.
    let language = launch_options
        .accept_language
        .as_deref()
        .or(config.language.as_deref());
    let language_env = language.and_then(language_args).map(|(primary, accept)| {
        args.push(format!("--lang={primary}"));
        args.push(format!("--accept-lang={accept}"));
        primary
    });
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
    if let Some(primary) = language_env {
        command.env("LANGUAGE", primary);
    }

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
