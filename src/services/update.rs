use std::{
    env,
    fs::File,
    io::{BufReader, copy},
};

use colored::Colorize;
use http::header::USER_AGENT;
use serde::Deserialize;
use snafu::ResultExt;
use tracing::info;
use wreq::Client;
use zip::ZipArchive;

use crate::{
    config::CLEWDR_CONFIG,
    error::{ClewdrError, WreqSnafu},
    services::version::{is_newer_release, parse_clewdr_version},
};

const DEFAULT_REPO_OWNER: &str = "waylon256yhw";
const DEFAULT_REPO_NAME: &str = "clewdr-hub";

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Clone)]
pub struct UpdateStatus {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
}

/// Updater for the ClewdR application
/// Handles checking for updates and updating the application
pub struct ClewdrUpdater {
    client: Client,
    user_agent: String,
    repo_owner: String,
    repo_name: String,
}

impl ClewdrUpdater {
    /// Creates a new ClewdrUpdater instance
    ///
    /// # Returns
    /// * `Result<Self, ClewdrError>` - A new updater instance or an error
    pub fn new() -> Result<Self, ClewdrError> {
        let (repo_owner, repo_name) = package_github_repo().unwrap_or_else(|| {
            (
                DEFAULT_REPO_OWNER.to_string(),
                DEFAULT_REPO_NAME.to_string(),
            )
        });
        let policy = wreq::redirect::Policy::default();
        let client = wreq::Client::builder()
            .redirect(policy)
            .build()
            .context(WreqSnafu {
                msg: "Failed to create HTTP client",
            })?;

        let user_agent = format!(
            "clewdr/{} (+https://github.com/{}/{})",
            env!("CARGO_PKG_VERSION"),
            repo_owner,
            repo_name
        );

        Ok(Self {
            client,
            user_agent,
            repo_owner,
            repo_name,
        })
    }

    /// Fetch the latest GitHub release and compare it with the running
    /// binary. This is a read-only status check; it never performs a
    /// self-update and is used by presentation layers such as `clewdr menu`.
    pub async fn check_update_status(&self) -> Result<UpdateStatus, ClewdrError> {
        let release = self.fetch_latest_release().await?;
        let latest_version = release.tag_name.trim_start_matches('v').to_string();
        let current_version = env!("CARGO_PKG_VERSION").to_string();

        let current_v = parse_clewdr_version(&current_version)?;
        let latest_v = parse_clewdr_version(&latest_version)?;
        let update_available = is_newer_release(&current_v, &latest_v);

        Ok(UpdateStatus {
            current_version,
            latest_version,
            update_available,
        })
    }

    /// Download and atomically replace the current binary with the latest
    /// release when it is newer than the running version. This exits the
    /// process after a successful replacement, matching the existing
    /// `clewdr update` behavior.
    pub async fn update_to_latest(&self) -> Result<bool, ClewdrError> {
        let release = self.fetch_latest_release().await?;
        let latest_version = release.tag_name.trim_start_matches('v');
        let current_version = env!("CARGO_PKG_VERSION");

        let current_v = parse_clewdr_version(current_version)?;
        let latest_v = parse_clewdr_version(latest_version)?;
        if !is_newer_release(&current_v, &latest_v) {
            return Ok(false);
        }

        self.perform_update(&release).await?;
        Ok(true)
    }

    /// Checks for updates by comparing the current version to the latest release on GitHub
    /// Performs automatic update if `force` is true or auto_update is enabled in config.
    ///
    /// # Arguments
    /// * `force` — when `true`, the check ignores `check_update=false` and always
    ///   performs the update if a newer version is available. Set by callers
    ///   that originate from `clewdr --update` or `clewdr update`.
    ///
    /// # Returns
    /// * `Result<bool, ClewdrError>` - True if update available, false otherwise
    pub async fn check_for_updates(&self, force: bool) -> Result<bool, ClewdrError> {
        if CLEWDR_CONFIG.load().no_fs {
            // If no_fs feature is enabled, skip update check
            info!("Update check skipped due to no_fs feature");
            return Ok(false);
        }

        if !force && !CLEWDR_CONFIG.load().check_update {
            return Ok(false);
        }

        info!("Checking for updates...");
        // info!("User-Agent: {}", self.user_agent);

        let release = self.fetch_latest_release().await?;
        let latest_version = release.tag_name.trim_start_matches('v');
        let current_version = env!("CARGO_PKG_VERSION");

        // SemVer 2.0 precedence: prereleases don't outrank matching
        // stables, *and* build metadata is ignored. Default `Ord` on
        // semver::Version compares build metadata too, which would make
        // a release like `v1.2.4+build.5` look "newer" than a running
        // `1.2.4` and self-replace with the same binary on every check.
        let current_v = parse_clewdr_version(current_version)?;
        let latest_v = parse_clewdr_version(latest_version)?;
        let update_available = is_newer_release(&current_v, &latest_v);

        if !update_available {
            info!("Already at the latest version {}", current_version.green());
            return Ok(false);
        }
        info!(
            "New version {} available (current: {})",
            latest_version.green().italic(),
            current_version.yellow()
        );
        // Auto update if forced or enabled in config
        if force || CLEWDR_CONFIG.load().auto_update {
            self.perform_update(&release).await?;
        }

        Ok(true)
    }

    async fn fetch_latest_release(&self) -> Result<GitHubRelease, ClewdrError> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/releases/latest",
            self.repo_owner, self.repo_name
        );

        let response = self
            .client
            .get(&url)
            .header(USER_AGENT, &self.user_agent)
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to fetch latest release from GitHub",
            })?
            .error_for_status()
            .context(WreqSnafu {
                msg: "Fetch latest release from GitHub returned an error",
            })?;

        response.json().await.context(WreqSnafu {
            msg: "Failed to parse GitHub release response",
        })
    }

    /// Performs the update process
    /// Downloads the appropriate release asset, extracts it, and replaces the current binary
    ///
    /// # Arguments
    /// * `release` - GitHub release information containing assets to download
    ///
    /// # Returns
    /// * `Result<(), ClewdrError>` - Success or error during update process
    async fn perform_update(&self, release: &GitHubRelease) -> Result<(), ClewdrError> {
        let latest_version = release.tag_name.trim_start_matches('v');

        // Find appropriate asset for this platform
        let asset = self.find_appropriate_asset(release)?;

        info!("Downloading update from {}", asset.browser_download_url);

        // Create a temporary directory
        let temp_dir = tempfile::tempdir()?;
        let zip_path = temp_dir.path().join("update.zip");

        // Download the asset
        let response = self
            .client
            .get(&asset.browser_download_url)
            .header(USER_AGENT, &self.user_agent)
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to download update asset",
            })?
            .error_for_status()
            .context(WreqSnafu {
                msg: "Download update asset returned an error",
            })?;

        // Save the downloaded file
        let content = response.bytes().await.context(WreqSnafu {
            msg: "Failed to read response bytes from update asset",
        })?;
        let mut file = File::create(&zip_path)?;
        copy(&mut content.as_ref(), &mut file)?;

        // Extract the zip
        let extract_dir = temp_dir.path().join("extracted");
        std::fs::create_dir_all(&extract_dir)?;

        let file = File::open(&zip_path)?;
        let reader = BufReader::new(file);
        let mut archive = ZipArchive::new(reader)?;

        // Extract all files
        archive.extract(&extract_dir)?;

        let binary_name = if cfg!(windows) {
            "clewdr.exe"
        } else {
            "clewdr"
        };
        let binary_path = extract_dir.join(binary_name);

        if !binary_path.exists() {
            return Err(ClewdrError::AssetError {
                msg: format!("Binary not found in the update package: {binary_name}"),
            });
        }

        // Make the binary executable on Unix systems
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&binary_path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&binary_path, perms)?;
        }

        #[cfg(target_os = "android")]
        {
            use tracing::warn;
            let so_path = extract_dir.join("libc++_shared.so");
            if so_path.exists() {
                let current_dir = env::current_exe()?
                    .parent()
                    .ok_or(ClewdrError::AssetError {
                        msg: "Failed to get current directory".to_string(),
                    })?
                    .to_path_buf();
                let target_so_path = current_dir.join("libc++_shared.so");
                std::fs::copy(&so_path, &target_so_path)?;
                info!("Copied libc++_shared.so to the application directory");
            } else {
                warn!("libc++_shared.so not found in the update package");
            }
        }

        // Replace the current binary
        self_replace::self_replace(&binary_path)?;

        println!("Successfully updated to version {}", latest_version.green());
        println!("{}", "Update complete, closing...".green());
        std::process::exit(0);
    }

    /// Finds the appropriate asset for the current platform and architecture
    ///
    /// # Arguments
    /// * `release` - GitHub release information containing available assets
    ///
    /// # Returns
    /// * `Result<&'a GitHubAsset, ClewdrError>` - Appropriate asset or error if none found
    fn find_appropriate_asset<'a>(
        &self,
        release: &'a GitHubRelease,
    ) -> Result<&'a GitHubAsset, ClewdrError> {
        // Determine platform and architecture
        let os = env::consts::OS;
        let arch = env::consts::ARCH;

        let target = match (os, arch) {
            ("windows", "x86_64") => "windows-x86_64",
            ("linux", "x86_64") => {
                if cfg!(target_env = "musl") {
                    "musllinux-x86_64"
                } else {
                    "linux-x86_64"
                }
            }
            ("linux", "aarch64") => {
                if cfg!(target_env = "musl") {
                    "musllinux-aarch64"
                } else {
                    "linux-aarch64"
                }
            }
            ("macos", "x86_64") => "macos-x86_64",
            ("macos", "aarch64") => "macos-aarch64",
            ("android", "aarch64") => "android-aarch64",
            _ => {
                return Err(ClewdrError::AssetError {
                    msg: format!("Unsupported platform: {os}-{arch}"),
                });
            }
        };
        info!("Detected platform: {}", target);
        release
            .assets
            .iter()
            .find(|asset| asset.name.contains(target) && asset.name.ends_with(".zip"))
            .ok_or(ClewdrError::AssetError {
                msg: format!("No suitable asset found for platform: {target}"),
            })
    }
}

fn package_github_repo() -> Option<(String, String)> {
    let repo = option_env!("CARGO_PKG_REPOSITORY")?.trim();
    let repo = repo
        .trim_end_matches(".git")
        .trim_end_matches('/')
        .trim_start_matches("git+");
    let suffix = repo
        .strip_prefix("https://github.com/")
        .or_else(|| repo.strip_prefix("http://github.com/"))
        .or_else(|| repo.strip_prefix("git@github.com:"))?;
    let mut parts = suffix.split('/');
    let owner = parts.next()?.trim();
    let name = parts.next()?.trim();
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some((owner.to_string(), name.to_string()))
}
