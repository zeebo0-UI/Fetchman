//! Self-update support. Releases are downloaded from GitHub and verified before
//! the running executable is replaced.
use crate::error::{FetchError, Result};
use flate2::read::GzDecoder;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fs, io::Write, path::Path, process::Command, thread, time::Duration};

const API: &str = "https://api.github.com/repos/zeebo0-UI/Fetchman/releases/latest";

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    assets: Vec<Asset>,
}
#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

pub async fn run(check: bool) -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent(format!("Fetchman/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| FetchError::Internal(e.to_string()))?;
    let release: Release = client
        .get(API)
        .send()
        .await
        .map_err(crate::error::network_error)?
        .error_for_status()
        .map_err(crate::error::network_error)?
        .json()
        .await
        .map_err(crate::error::network_error)?;
    let tag = release.tag_name.trim_start_matches('v');
    let latest = Version::parse(tag)
        .map_err(|_| FetchError::Http("GitHub returned an invalid release version.".into()))?;
    let current = Version::parse(env!("CARGO_PKG_VERSION")).expect("package version is valid");
    if latest <= current {
        println!("Fetchman is already up to date (v{current}).");
        return Ok(());
    }
    println!("Fetchman v{current} → v{latest}");
    if check {
        println!("Update available: {}", release.html_url);
        return Ok(());
    }
    let asset = select_asset(&release.assets).ok_or_else(|| {
        FetchError::Http("There is no Fetchman build for this operating system yet.".into())
    })?;
    let checksum = release
        .assets
        .iter()
        .find(|a| a.name == format!("{}.sha256", asset.name))
        .ok_or_else(|| FetchError::Http("The release is missing its checksum file.".into()))?;
    let archive = client
        .get(&asset.browser_download_url)
        .send()
        .await
        .map_err(crate::error::network_error)?
        .error_for_status()
        .map_err(crate::error::network_error)?
        .bytes()
        .await
        .map_err(crate::error::network_error)?;
    let expected = client
        .get(&checksum.browser_download_url)
        .send()
        .await
        .map_err(crate::error::network_error)?
        .error_for_status()
        .map_err(crate::error::network_error)?
        .text()
        .await
        .map_err(crate::error::network_error)?
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let actual = format!("{:x}", Sha256::digest(&archive));
    if expected != actual {
        return Err(FetchError::Http(
            "The update checksum did not match. Nothing was installed.".into(),
        ));
    }
    let current_exe =
        std::env::current_exe().map_err(|e| FetchError::io("Could not locate Fetchman", e))?;
    let temp = current_exe.with_file_name(format!("fetchman.update.{}.tmp", std::process::id()));
    extract_binary(&archive, &asset.name, &temp)?;
    let child =
        std::env::current_exe().map_err(|e| FetchError::io("Could not locate Fetchman", e))?;
    Command::new(child)
        .arg("apply-update")
        .arg(&temp)
        .arg(&current_exe)
        .spawn()
        .map_err(|e| FetchError::io("Could not start the updater", e))?;
    println!("Update downloaded and verified. Fetchman will restart with v{latest}.");
    Ok(())
}

fn select_asset(assets: &[Asset]) -> Option<&Asset> {
    #[cfg(windows)]
    {
        assets
            .iter()
            .find(|a| a.name.contains("windows") && a.name.ends_with(".zip"))
    }
    #[cfg(target_os = "linux")]
    {
        assets
            .iter()
            .find(|a| a.name.contains("x86_64-unknown-linux-gnu") && a.name.ends_with(".tar.gz"))
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        assets
            .iter()
            .find(|a| a.name.contains("x86_64-apple-darwin") && a.name.ends_with(".tar.gz"))
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        assets
            .iter()
            .find(|a| a.name.contains("aarch64-apple-darwin") && a.name.ends_with(".tar.gz"))
    }
}

fn extract_binary(data: &[u8], name: &str, destination: &Path) -> Result<()> {
    let mut output = fs::File::create(destination)
        .map_err(|e| FetchError::io("Could not prepare the update", e))?;
    if name.ends_with(".zip") {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(data))
            .map_err(|_| FetchError::Http("The downloaded update archive is invalid.".into()))?;
        let mut found = false;
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(|_| {
                FetchError::Http("The downloaded update archive is invalid.".into())
            })?;
            let path = Path::new(entry.name());
            if path.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            }) {
                continue;
            }
            if path.file_name().is_some_and(|n| {
                n == if cfg!(windows) {
                    "fetchman.exe"
                } else {
                    "fetchman"
                }
            }) {
                std::io::copy(&mut entry, &mut output)
                    .map_err(|e| FetchError::io("Could not extract the update", e))?;
                found = true;
                break;
            }
        }
        if !found {
            return Err(FetchError::Http(
                "The update archive did not contain Fetchman.".into(),
            ));
        }
    } else {
        let decoder = GzDecoder::new(std::io::Cursor::new(data));
        let mut archive = tar::Archive::new(decoder);
        let mut found = false;
        for item in archive
            .entries()
            .map_err(|e| FetchError::io("Could not read the update archive", e))?
        {
            let mut entry =
                item.map_err(|e| FetchError::io("Could not read the update archive", e))?;
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let path = entry
                .path()
                .map_err(|e| FetchError::io("Could not read the update archive", e))?;
            if path.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            }) {
                continue;
            }
            if path.file_name().is_some_and(|n| n == "fetchman") {
                std::io::copy(&mut entry, &mut output)
                    .map_err(|e| FetchError::io("Could not extract the update", e))?;
                found = true;
                break;
            }
        }
        if !found {
            return Err(FetchError::Http(
                "The update archive did not contain Fetchman.".into(),
            ));
        }
    }
    output
        .flush()
        .map_err(|e| FetchError::io("Could not write the update", e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o755))
            .map_err(|e| FetchError::io("Could not make the update executable", e))?;
    }
    Ok(())
}

pub fn apply_update(source: &Path, destination: &Path) -> Result<()> {
    for _ in 0..100 {
        if let Ok(meta) = fs::metadata(source)
            && meta.len() > 0
        {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let backup = destination.with_extension("old");
    for _ in 0..100 {
        if destination.exists() {
            let _ = fs::remove_file(&backup);
            if fs::rename(destination, &backup).is_err() {
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        }
        match fs::rename(source, destination) {
            Ok(()) => {
                let _ = fs::remove_file(backup);
                return Ok(());
            }
            Err(_) => {
                let _ = fs::rename(&backup, destination);
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
    Err(FetchError::io(
        "Could not install the update",
        std::io::Error::other("the executable is still in use"),
    ))
}
