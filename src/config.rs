use std::env;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid value for {name}: {reason}")]
    Invalid { name: &'static str, reason: String },
}

fn invalid(name: &'static str, reason: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        name,
        reason: reason.into(),
    }
}

/// Server configuration, loaded from environment variables and validated at startup.
#[derive(Debug, Clone)]
pub struct Config {
    pub host: IpAddr,
    pub port: u16,
    /// Empty token disables authentication.
    pub token: Option<String>,
    pub max_concurrent_sessions: usize,
    pub max_queue_length: usize,
    pub connection_timeout: Duration,
    pub queue_timeout: Duration,
    pub chrome_startup_timeout: Duration,
    pub shutdown_grace_period: Duration,
    pub chrome_path: PathBuf,
    pub chrome_headless: bool,
    pub chrome_no_sandbox: bool,
    pub chrome_disable_dev_shm_usage: bool,
    pub chrome_extra_args: Vec<String>,
    /// Used for the Chromium --lang flag when set.
    pub language: Option<String>,
}

fn env_str(name: &'static str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

fn env_parse<T: std::str::FromStr>(name: &'static str, default: T) -> Result<T, ConfigError>
where
    T::Err: std::fmt::Display,
{
    match env::var(name) {
        Ok(raw) if !raw.trim().is_empty() => raw
            .trim()
            .parse::<T>()
            .map_err(|e| invalid(name, e.to_string())),
        _ => Ok(default),
    }
}

fn env_bool(name: &'static str, default: bool) -> Result<bool, ConfigError> {
    match env::var(name) {
        Ok(raw) if !raw.trim().is_empty() => match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Ok(true),
            "false" | "0" | "no" => Ok(false),
            other => Err(invalid(name, format!("expected a boolean, got {other:?}"))),
        },
        _ => Ok(default),
    }
}

fn env_duration_ms(name: &'static str, default_ms: u64) -> Result<Duration, ConfigError> {
    let ms: u64 = env_parse(name, default_ms)?;
    if ms == 0 {
        return Err(invalid(name, "must be greater than 0"));
    }
    Ok(Duration::from_millis(ms))
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let host: IpAddr = env_str("HOST", "0.0.0.0")
            .parse()
            .map_err(|_| invalid("HOST", "expected an IP address"))?;

        let port: u16 = env_parse("PORT", 3000u16)?;
        if port == 0 {
            return Err(invalid("PORT", "must be between 1 and 65535"));
        }

        let token = match env::var("TOKEN") {
            Ok(raw) if !raw.trim().is_empty() => Some(raw.trim().to_string()),
            _ => None,
        };

        let max_concurrent_sessions: usize = env_parse("MAX_CONCURRENT_SESSIONS", 10usize)?;
        if max_concurrent_sessions == 0 {
            return Err(invalid("MAX_CONCURRENT_SESSIONS", "must be at least 1"));
        }

        let max_queue_length: usize = env_parse("MAX_QUEUE_LENGTH", 20usize)?;

        let chrome_path = PathBuf::from(env_str("CHROME_PATH", "/usr/bin/chromium"));
        if !chrome_path.is_file() {
            return Err(invalid(
                "CHROME_PATH",
                format!("{} is not an executable file", chrome_path.display()),
            ));
        }

        let chrome_extra_args = match env::var("CHROME_EXTRA_ARGS") {
            Ok(raw) => raw
                .split_whitespace()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        };

        let language = match env::var("LANGUAGE") {
            Ok(raw) if !raw.trim().is_empty() => Some(raw.trim().to_string()),
            _ => None,
        };

        Ok(Self {
            host,
            port,
            token,
            max_concurrent_sessions,
            max_queue_length,
            connection_timeout: env_duration_ms("CONNECTION_TIMEOUT_MS", 600_000)?,
            queue_timeout: env_duration_ms("QUEUE_TIMEOUT_MS", 600_000)?,
            chrome_startup_timeout: env_duration_ms("CHROME_STARTUP_TIMEOUT_MS", 15_000)?,
            shutdown_grace_period: env_duration_ms("SHUTDOWN_GRACE_PERIOD_MS", 10_000)?,
            chrome_path,
            chrome_headless: env_bool("CHROME_HEADLESS", true)?,
            chrome_no_sandbox: env_bool("CHROME_NO_SANDBOX", true)?,
            chrome_disable_dev_shm_usage: env_bool("CHROME_DISABLE_DEV_SHM_USAGE", true)?,
            chrome_extra_args,
            language,
        })
    }
}
