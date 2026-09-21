//! On-demand installation of a browser archive chosen by the operator.
//!
//! The Pinokio image ships only the stock Chromium. With BROWSER_ARCHIVE_URL
//! set, Pinokio downloads that archive (`.tar.gz` or `.zip`, detected from the
//! file content) on first start onto the operator's own volume and launches
//! the `chrome` executable it contains. The archive's
//! SHA-256 is always computed and logged; when BROWSER_ARCHIVE_SHA256 is set
//! too, a mismatch aborts the install. Pinokio has no opinion about which
//! browser this is: the operator picks the build and accepts its publisher's
//! license. Nothing third-party transits through Puppetflow.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tracing::info;

/// Volume where archives are installed, one directory per archive URL.
pub const INSTALL_ROOT: &str = "/opt/browsers";

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Error)]
pub enum InstallError {
    #[error("download failed: {0}")]
    Download(String),
    #[error("archive checksum mismatch: expected {expected}, got {actual}")]
    Checksum { expected: String, actual: String },
    #[error("archive extraction failed: {0}")]
    Archive(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Install directory name: a prefix of the URL's SHA-256, long enough to be
/// unique in practice and short enough to read in logs. Changing the URL
/// installs the new archive next to the previous one; re-publishing under the
/// same URL requires clearing the volume.
fn install_key(url: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(url.as_bytes()));
    digest[..16].to_string()
}

pub fn binary_path(root: &Path, url: &str) -> PathBuf {
    root.join(install_key(url)).join("chrome")
}

/// Returns the path of the `chrome` executable for the archive, downloading
/// it first when it is not installed yet under `root`. `expected_sha256`,
/// when given, must match the downloaded file.
pub async fn ensure_installed(
    root: &Path,
    url: &str,
    expected_sha256: Option<&str>,
) -> Result<PathBuf, InstallError> {
    let binary = binary_path(root, url);
    if binary.is_file() {
        return Ok(binary);
    }
    info!(%url, root = %root.display(), "browser_archive_install_start");

    tokio::fs::create_dir_all(root).await?;
    let client = reqwest::Client::builder()
        .user_agent(concat!("pinokio/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| InstallError::Download(e.to_string()))?;

    let tag = format!("{}-{}", install_key(url), std::process::id());
    let archive_path = root.join(format!(".download-{tag}.archive"));
    let staging_dir = root.join(format!(".staging-{tag}"));
    let result = install(
        &client,
        url,
        expected_sha256,
        &archive_path,
        &staging_dir,
        &binary,
    )
    .await;

    // Best-effort cleanup of temporaries whatever the outcome.
    let _ = tokio::fs::remove_file(&archive_path).await;
    let _ = tokio::fs::remove_dir_all(&staging_dir).await;

    result.map(|()| binary)
}

async fn install(
    client: &reqwest::Client,
    url: &str,
    expected_hash: Option<&str>,
    archive_path: &Path,
    staging_dir: &Path,
    binary: &Path,
) -> Result<(), InstallError> {
    let actual = download_to_file(client, url, archive_path).await?;
    match expected_hash {
        Some(expected) if expected != actual => {
            return Err(InstallError::Checksum {
                expected: expected.to_string(),
                actual,
            });
        }
        Some(_) => info!(sha256 = %actual, "browser_archive_verified"),
        // Logged so the operator can compare with the publisher's checksums.
        None => info!(sha256 = %actual, "browser_archive_downloaded"),
    }

    let extracted = {
        let archive_path = archive_path.to_path_buf();
        let staging_dir = staging_dir.to_path_buf();
        tokio::task::spawn_blocking(move || extract_archive(&archive_path, &staging_dir))
            .await
            .map_err(|e| InstallError::Archive(format!("extraction task failed: {e}")))??
    };

    let install_dir = binary
        .parent()
        .expect("binary path always has a parent directory");
    match tokio::fs::rename(&extracted, install_dir).await {
        Ok(()) => {}
        // Another replica sharing the volume may have finished first.
        Err(_) if binary.is_file() => {}
        Err(e) => return Err(e.into()),
    }
    info!(path = %binary.display(), "browser_archive_install_done");
    Ok(())
}

/// Streams `url` into `dest`, hashing on the fly. Returns the hex SHA-256.
async fn download_to_file(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
) -> Result<String, InstallError> {
    let response = client
        .get(url)
        .timeout(DOWNLOAD_TIMEOUT)
        .send()
        .await
        .map_err(|e| InstallError::Download(e.to_string()))?;
    if !response.status().is_success() {
        return Err(InstallError::Download(format!(
            "HTTP {}",
            response.status()
        )));
    }
    let total = response.content_length().unwrap_or(0);

    let mut file = tokio::fs::File::create(dest).await?;
    let mut hasher = Sha256::new();
    let mut downloaded: u64 = 0;
    let mut next_report = 10;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| InstallError::Download(e.to_string()))?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        downloaded += chunk.len() as u64;
        if let Some(percent) = (downloaded * 100).checked_div(total)
            && percent >= next_report
        {
            // One line per 10% step: readable in `docker logs`, which has no
            // terminal to redraw a live bar on.
            let filled = (percent / 5).min(20) as usize;
            info!(
                progress = format!(
                    "[{}{}] {percent:>3}% {}/{} MB",
                    "#".repeat(filled),
                    "-".repeat(20 - filled),
                    downloaded / (1024 * 1024),
                    total / (1024 * 1024)
                ),
                "browser_archive_download_progress"
            );
            next_report = percent - percent % 10 + 10;
        }
    }
    file.flush().await?;
    Ok(format!("{:x}", hasher.finalize()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveFormat {
    TarGz,
    Zip,
}

/// Sniffs the archive format from its magic bytes rather than the URL, since
/// download links do not always end with the file extension.
fn detect_format(archive_path: &Path) -> Result<ArchiveFormat, InstallError> {
    use std::io::Read;

    let mut magic = [0u8; 4];
    let read = std::fs::File::open(archive_path)?.read(&mut magic)?;
    match &magic[..read] {
        [0x1f, 0x8b, ..] => Ok(ArchiveFormat::TarGz),
        [b'P', b'K', 0x03, 0x04] | [b'P', b'K', 0x05, 0x06] => Ok(ArchiveFormat::Zip),
        _ => Err(InstallError::Archive(
            "unrecognized archive format, expected a .tar.gz or .zip file".into(),
        )),
    }
}

/// Unpacks the archive (gzip tarball or zip) into `staging_dir` and returns the
/// directory that holds `chrome` (the archive may wrap everything in one
/// top-level folder). Files are made world-readable, directories and
/// executables world-executable, since Pinokio runs as an unprivileged user.
fn extract_archive(archive_path: &Path, staging_dir: &Path) -> Result<PathBuf, InstallError> {
    if staging_dir.exists() {
        std::fs::remove_dir_all(staging_dir)?;
    }
    std::fs::create_dir_all(staging_dir)?;

    match detect_format(archive_path)? {
        ArchiveFormat::TarGz => {
            let file = std::fs::File::open(archive_path)?;
            let decoder = flate2::read::GzDecoder::new(file);
            let mut archive = tar::Archive::new(decoder);
            archive.set_overwrite(true);
            // `unpack` refuses entries that would escape the destination directory.
            archive
                .unpack(staging_dir)
                .map_err(|e| InstallError::Archive(e.to_string()))?;
        }
        ArchiveFormat::Zip => {
            // Chrome for Testing and Google's other builds ship as zip. `extract`
            // sanitizes entry paths, restores unix modes and keeps symlinks inside
            // the destination.
            let file = std::fs::File::open(archive_path)?;
            let mut archive =
                zip::ZipArchive::new(file).map_err(|e| InstallError::Archive(e.to_string()))?;
            archive
                .extract(staging_dir)
                .map_err(|e| InstallError::Archive(e.to_string()))?;
        }
    }

    let root = if staging_dir.join("chrome").is_file() {
        staging_dir.to_path_buf()
    } else {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(staging_dir)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        match entries.as_mut_slice() {
            [single] if single.is_dir() && single.join("chrome").is_file() => single.clone(),
            _ => {
                return Err(InstallError::Archive(
                    "no chrome executable found at the archive root or in its single top-level directory".into(),
                ));
            }
        }
    };

    make_world_readable(&root)?;
    let mut chrome_perms = std::fs::metadata(root.join("chrome"))?.permissions();
    chrome_perms.set_mode(0o755);
    std::fs::set_permissions(root.join("chrome"), chrome_perms)?;
    Ok(root)
}

fn make_world_readable(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        let mut mode = metadata.permissions().mode() | 0o444;
        if metadata.is_dir() || mode & 0o111 != 0 {
            mode |= 0o111;
        }
        std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(mode))?;
        if metadata.is_dir() {
            make_world_readable(&entry.path())?;
        }
    }
    Ok(())
}
