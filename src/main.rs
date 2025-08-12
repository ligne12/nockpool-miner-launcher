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
use tokio::sync::Mutex;
use tokio::time::{interval, Duration};
use zip::ZipArchive;

const UPDATE_URL: &str = "https://api.github.com/repos/SWPSCO/nockpool-miner/releases/latest";

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

        let base_dir = if os_name == "macos" {
            env::var("HOME").unwrap() + "/Library/Application Support/nockpool-miner"
        } else {
            "/opt/nockpool-miner".to_string()
        };
        let base_dir = PathBuf::from(base_dir);
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
            println!("New version {} is available. Downloading...", self.version);
            self.download_and_run().await?;
            self.update_symlink()?;
        } else {
            println!("You are on the latest version.");
        }
        Ok(())
    }

    async fn download_and_run(&self) -> Result<()> {
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

    pub fn run_miner(&self) -> Result<Child> {
        let bin_path = self.current_symlink.join(&self.bin_name);
        let child = Command::new(bin_path)
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
        child: Arc<Mutex<Child>>,
    ) {
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(15 * 60));
            loop {
                interval.tick().await;
                let mut pi = package_info.lock().await;
                let local_version = pi.get_local_version();
                pi.fetch_latest().await.unwrap();

                let needs_update = match local_version {
                    Some(lv) => lv != pi.version,
                    None => true,
                };

                if needs_update {
                    println!("Update found in background, restarting miner...");
                    let mut child_lock = child.lock().await;
                    pi.kill_miner(&mut child_lock).unwrap();
                    pi.ensure_latest_version().await.unwrap();
                    let new_child = pi.run_miner().unwrap();
                    *child_lock = new_child;
                }
            }
        });
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let package_info = PackageInfo::new()?;
    let package_info = Arc::new(Mutex::new(package_info));

    {
        let mut pi = package_info.lock().await;
        pi.ensure_latest_version().await?;
    }

    let mut child = {
        let pi = package_info.lock().await;
        pi.run_miner()?
    };

    let stdout = child
        .stdout
        .take()
        .expect("child stdout was not configured to a pipe");

    let stderr = child
        .stderr
        .take()
        .expect("child stderr was not configured to a pipe");

    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            println!("{}", line);
        }
    });

    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            eprintln!("{}", line);
        }
    });

    let child = Arc::new(Mutex::new(child));

    PackageInfo::start_update_watcher(package_info.clone(), child.clone());

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("Ctrl-C received, shutting down miner...");
            let mut child_lock = child.lock().await;
            let pi = package_info.lock().await;
            pi.kill_miner(&mut child_lock)?;
            println!("Miner shut down.");
        }
        res = async {
            let mut child_guard = child.lock().await;
            child_guard.wait().await
        } => {
            println!("Miner exited with status: {:?}", res);
        }
    }

    Ok(())
}
