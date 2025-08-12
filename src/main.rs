use reqwest::Client;
use anyhow::Result;
use reqwest::header::USER_AGENT;
use serde::Deserialize;
use sysinfo::{System};
use directories::UserDirs;
use std::path::PathBuf;

const UPDATE_URL: &str = "https://api.github.com/repos/SWPSCO/nockpool-miner/releases/latest";

#[derive(Debug, Deserialize)]
struct ReleaseInfo {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    digest: String,
    browser_download_url: String,
}

#[derive(Debug)]
struct VersionInfo {
    major: u32,
    minor: u32,
    patch: u32,
}

impl VersionInfo {
    pub fn new(version: &str) -> Self {
        let clean_version = version.replace("v", "");
        let parts: Vec<&str> = clean_version.split('.').collect();
        Self {
            major: parts[0].parse().unwrap(),
            minor: parts[1].parse().unwrap(),
            patch: parts[2].parse().unwrap(),
        }
    }

    pub fn to_string(&self) -> String {
        format!("{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug)]
struct PackageInfo {
    version: VersionInfo,
    download_url: String,
    digest: String,
}


#[tokio::main]
async fn main() {
    // get os information
    let (os_name, arch) = match get_device_info() {
        Ok((os_name, arch)) => (os_name, arch),
        Err(e) => {
            eprintln!("Error getting device info: {}", e);
            return;
        }
    };

    // fetch update information
    let package_info = match fetch_latest(os_name, arch).await {
        Ok(package_info) => package_info,
        Err(e) => {
            eprintln!("Error fetching latest package info: {}", e);
            return;
        }
    };
    println!("package_info: {:#?}", package_info);

    // check if current exists in ~/Library/Application Support/nockpool-miner/current/symlinked-file
    let home_dir = UserDirs::new()?.home_dir().to_path_buf();
    let versions = home_dir.join("Library").join("Application Support").join("nockpool-miner/versions")
    let current_path = versions.join("current/nockpool-miner"); // nockpool miner is a symlink to the current version
    if !current_path.exists() {
        match download_and_run(package_info).await {
            Ok(_) => (),
            Err(e) => {
                eprintln!("Error downloading and running: {}", e);
                return;
            }
        }
    }
    // if not, download from latest release
    // if ex
}

async fn download_and_run(package_info: PackageInfo) -> Result<()> {
    let client = Client::new();
    let res = client.get(package_info.download_url).send().await?;
    let body = res.text().await?;
    println!("body: {}", body);
    Ok(())
}

async fn fetch_latest(os_name: String, arch: String) -> Result<PackageInfo> {
    let client = Client::new();

    let res = client.get(UPDATE_URL).header(USER_AGENT, "miner-launcher").send().await?;

    let release_info: ReleaseInfo = res.json().await?;

    let package_name = format!(
        "nockpool-miner-{}-{}{}",
        os_name,
        arch,
        if os_name == "macos" { ".pkg" } else { "" },
    );

    let mut download_url = String::new();
    let mut digest = String::new();

    for asset in release_info.assets {
        if asset.name == package_name {
            download_url = asset.browser_download_url;
            digest = asset.digest.split(':').collect::<Vec<&str>>()[1].to_string();
            break;
        }
    }

    let package_info = PackageInfo {
        version: VersionInfo::new(&release_info.tag_name),
        download_url,
        digest,
    };

    Ok(package_info)
}

fn get_device_info() -> Result<(String, String)> {
    let os_name = match System::name() {
        Some(os) => if os == "Darwin" { "macos" } else { "linux" },
        None => return Err(anyhow::anyhow!("Failed to get OS name")),
    };

    let arch = match System::cpu_arch() {
        Some(arch) => if arch == "arm64" { "aarch64" } else { "x86_64" },
        None => return Err(anyhow::anyhow!("Failed to get CPU architecture")),
    };

    Ok((os_name.to_string(), arch.to_string()))
}