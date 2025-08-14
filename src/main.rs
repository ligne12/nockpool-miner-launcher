mod tracer;

use anyhow::Result;
use reqwest::{header::USER_AGENT, Client};
use serde::Deserialize;
use sysinfo::System;
use std::env;
use std::fs;
use std::io::{Cursor, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify};
use tokio::time::{interval, Duration};
use zip::ZipArchive;
use tracing::info;
use directories::ProjectDirs;

const UPDATE_URL: &str = "https://nockpool.com/api/version";
const UPDATE_INTERVAL: u64 = 15 * 60;

#[derive(Debug, Deserialize)]
struct ReleaseInfo {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Clone)]
struct PackageInfo {
    os_name: String,
    arch: String,
    version: String,
    download_url: String,
    bin_name: String,
    package_name: String,
    versions_dir: PathBuf,
    current_symlink: PathBuf,
}

impl PackageInfo {
    pub fn new() -> Result<Self> {
        let (os_name, arch) = Self::get_device_info()?;
        let bin_name = "nockpool-miner".to_string();

        let base_dir = if let Some(proj_dirs) = ProjectDirs::from("com", "swps", "nockpool-miner") {
            proj_dirs.data_dir().to_path_buf()
        } else {
            return Err(anyhow::anyhow!("Could not determine application data directory"));
        };

        let versions_dir = base_dir.join("versions");
        let current_symlink = base_dir.join("current");

        Ok(PackageInfo {
            os_name,
            arch,
            version: String::new(),
            download_url: String::new(),
            bin_name,
            package_name: String::new(),
            versions_dir,
            current_symlink,
        })
    }

    fn get_device_info() -> Result<(String, String)> {
        let os_name = match System::name() {
            Some(os) => {
                if os.to_lowercase().contains("darwin") {
                    "macos".to_string()
                } else {
                    "linux".to_string()
                }
            }
            None => return Err(anyhow::anyhow!("Failed to get OS name")),
        };

        let arch = match System::cpu_arch() {
            Some(arch) => {
                if arch == "aarch64" || arch == "arm64" {
                    "aarch64".to_string()
                } else {
                    "x86_64".to_string()
                }
            }
            None => return Err(anyhow::anyhow!("Failed to get CPU architecture")),
        };

        Ok((os_name, arch))
    }

    pub async fn fetch_latest(&mut self) -> Result<()> {
        let client = Client::new();
        let res = client
            .get(UPDATE_URL)
            .header(USER_AGENT, "miner-launcher")
            .send()
            .await?;
        let release_info: ReleaseInfo = res.json().await?;

        self.package_name = if self.os_name == "macos" {
            format!("{}-{}-{}.zip", self.bin_name, self.os_name, self.arch)
        } else {
            format!("{}-{}-{}", self.bin_name, self.os_name, self.arch)
        };

        for asset in release_info.assets {
            if asset.name == self.package_name {
                self.download_url = asset.browser_download_url;
                self.version = release_info.tag_name.replace("v", "");
                return Ok(());
            }
        }

        Err(anyhow::anyhow!(
            "Could not find a compatible package for this platform"
        ))
    }

    pub fn get_local_version(&self) -> Option<String> {
        if self.current_symlink.exists() {
            let real_path = fs::read_link(&self.current_symlink).ok()?;
            let version = real_path.file_name()?.to_str()?.to_string();
            Some(version)
        } else {
            None
        }
    }

    pub async fn ensure_latest_version(&mut self) -> Result<()> {
        let local_version = self.get_local_version();
        self.fetch_latest().await?;

        let needs_update = match local_version {
            Some(lv) => lv != self.version,
            None => true,
        };

        if needs_update {
            info!("New version {} is available. Downloading...", self.version);
            self.download_and_install().await?;
            self.update_symlink()?;
        } else {
            info!("You are on the latest version.");
        }
        Ok(())
    }

    async fn download_and_install(&self) -> Result<()> {
        let response = reqwest::get(&self.download_url).await?;
        let bytes = response.bytes().await?;

        let version_dir = self.versions_dir.join(&self.version);
        fs::create_dir_all(&version_dir)?;

        let bin_path = version_dir.join(&self.bin_name);

        if self.os_name == "macos" {
            let mut archive = ZipArchive::new(Cursor::new(bytes))?;
            archive.extract(&version_dir)?;
        } else {
            let mut file = fs::File::create(&bin_path)?;
            file.write_all(&bytes)?;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bin_path, fs::Permissions::from_mode(0o755))?;
        }

        Ok(())
    }

    fn update_symlink(&self) -> Result<()> {
        let version_dir = self.versions_dir.join(&self.version);

        if self.current_symlink.exists() {
            fs::remove_file(&self.current_symlink)?;
        }

        #[cfg(unix)]
        std::os::unix::fs::symlink(version_dir, &self.current_symlink)?;

        Ok(())
    }

    pub fn run_miner(&self, args: &[String]) -> Result<Child> {
        let bin_path = self.current_symlink.join(&self.bin_name);
        let child = Command::new(bin_path)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        Ok(child)
    }

    pub fn kill_miner(&self, child: &mut Child) -> Result<()> {
        child.start_kill()?;
        Ok(())
    }

    pub fn start_update_watcher(
        package_info: Arc<Mutex<PackageInfo>>,
        update_notifier: Arc<Notify>,
    ) {
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(UPDATE_INTERVAL));
            loop {
                interval.tick().await;
                info!("Checking for updates...");

                let mut pi = package_info.lock().await;
                let local_version = pi.get_local_version();

                if let Err(e) = pi.fetch_latest().await {
                    info!("Failed to check for updates: {}", e);
                    continue;
                }

                let needs_update = match local_version {
                    Some(lv) => lv != pi.version,
                    None => true,
                };

                if needs_update {
                    info!("Update found in background, preparing update...");
                    if let Err(e) = pi.download_and_install().await {
                        info!("Failed to download update: {}", e);
                        continue;
                    }
                    if let Err(e) = pi.update_symlink() {
                        info!("Failed to update symlink: {}", e);
                        continue;
                    }
                    update_notifier.notify_one();
                } else {
                    info!("Already on the latest version.");
                }
            }
        });
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracer::init();

    let mut disable_update_loop = false;
    let mut no_update = false;
    let mut miner_args = Vec::new();

    for arg in env::args().skip(1) {
        match arg.as_str() {
            "--disable-update-loop" => disable_update_loop = true,
            "--no-update" => no_update = true,
            _ => miner_args.push(arg),
        }
    }

    let package_info = PackageInfo::new()?;
    let package_info = Arc::new(Mutex::new(package_info));

    if no_update {
        let pi = package_info.lock().await;
        if pi.get_local_version().is_none() {
            return Err(anyhow::anyhow!(
                "No current version installed. Please run without --no-update first."
            ));
        }
    } else {
        let mut pi = package_info.lock().await;
        pi.ensure_latest_version().await?;
    }

    let restart_notifier = Arc::new(Notify::new());
    let update_notifier = Arc::new(Notify::new());

    if !disable_update_loop {
        PackageInfo::start_update_watcher(package_info.clone(), update_notifier.clone());
    }

    loop {
        let mut child = {
            let pi = package_info.lock().await;
            pi.run_miner(&miner_args)?
        };

        let stdout = child
            .stdout
            .take()
            .expect("child stdout was not configured to a pipe");

        let stderr = child
            .stderr
            .take()
            .expect("child stderr was not configured to a pipe");

        let restart_notifier_stdout = restart_notifier.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if line.contains("restart-miner-now") {
                    info!("Restart signal received from stdout, restarting miner...");
                    restart_notifier_stdout.notify_one();
                    break;
                }
                eprintln!("{}", line);
            }
        });

        let restart_notifier_stderr = restart_notifier.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if line.contains("restart-miner-now") {
                    info!("Restart signal received from stderr, restarting miner...");
                    restart_notifier_stderr.notify_one();
                    break;
                }
                eprintln!("{}", line);
            }
        });

        let child = Arc::new(Mutex::new(child));

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl-C received, shutting down miner...");
                let mut child_lock = child.lock().await;
                let pi = package_info.lock().await;
                pi.kill_miner(&mut child_lock)?;
                info!("Miner shut down.");
                break;
            }
            _ = restart_notifier.notified() => {
                info!("Restarting miner due to output signal...");
                let mut child_lock = child.lock().await;
                let pi = package_info.lock().await;
                let _ = pi.kill_miner(&mut child_lock);
                continue;
            }
            _ = update_notifier.notified() => {
                info!("Restarting miner due to update...");
                let mut child_lock = child.lock().await;
                let pi = package_info.lock().await;
                let _ = pi.kill_miner(&mut child_lock);
                continue;
            }
            res = async {
                let mut child_guard = child.lock().await;
                child_guard.wait().await
            } => {
                info!("Miner exited with status: {:?}. Restarting...", res);
                continue;
            }
        }
    }

    Ok(())
}
