// checks GitHub Releases for a newer ofm, downloads the build for this platform,
// verifies it against the release's SHA256SUMS.txt and swaps it in for the
// running binary. it never asks anything itself: `ofm update` shows what
// check() found and only calls install() after the user types y.

use anyhow::{bail, Context, Result};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;

const LATEST_RELEASE: &str = "https://api.github.com/repos/Londopy/ofm/releases/latest";
const SUMS: &str = "SHA256SUMS.txt";

#[derive(Debug)]
pub struct Update {
    pub version: Version,
    // the release page, for the notes
    pub page: String,
    binary: Asset,
    sums: Asset,
}

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

fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("ofm/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        // the blocking client applies this to the headers and then to each read,
        // so a slow download still finishes as long as it keeps moving
        .timeout(Duration::from_secs(30))
        .build()
        .context("setting up the HTTP client")
}

// Ok(None) when this build is already the latest release
pub fn check() -> Result<Option<Update>> {
    let suffix = asset_suffix(std::env::consts::OS, std::env::consts::ARCH)
        .context("ofm releases have no build for this platform; build it from source")?;
    let release: Release = client()?
        .get(LATEST_RELEASE)
        .send()
        .and_then(|r| r.error_for_status())
        .context("asking GitHub for the latest ofm release")?
        .json()
        .context("reading the release info from GitHub")?;
    let current = Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo versions are semver");
    pick(release, &current, suffix)
}

// end of the release asset each platform updates from; release.yml makes these
fn asset_suffix(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("windows", "x86_64") => Some("-x64-portable.exe"),
        ("windows", "x86") => Some("-x86-portable.exe"),
        ("windows", "aarch64") => Some("-arm64-portable.exe"),
        ("linux", "x86_64") => Some("-linux-x64"),
        _ => None,
    }
}

fn pick(release: Release, current: &Version, suffix: &str) -> Result<Option<Update>> {
    let tag = &release.tag_name;
    let version = Version::parse(tag.trim_start_matches('v'))
        .with_context(|| format!("the latest release has an unexpected tag: {tag}"))?;
    if version <= *current {
        return Ok(None);
    }
    let (mut binary, mut sums) = (None, None);
    for asset in release.assets {
        if asset.name == SUMS {
            sums = Some(asset);
        } else if asset.name.ends_with(suffix) {
            binary = Some(asset);
        }
    }
    let Some(binary) = binary else {
        bail!(
            "ofm {version} has no *{suffix} download; get it from {}",
            release.html_url
        );
    };
    let Some(sums) = sums else {
        bail!(
            "ofm {version} has no {SUMS} to verify it with; get it from {}",
            release.html_url
        );
    };
    Ok(Some(Update {
        version,
        page: release.html_url,
        binary,
        sums,
    }))
}

// the new version runs the next time ofm starts
pub fn install(update: &Update) -> Result<()> {
    let exe = std::env::current_exe().context("finding the running ofm binary")?;
    let dir = exe
        .parent()
        .context("the ofm binary has no parent folder")?;
    if !dir_is_writable(dir) {
        bail!(
            "ofm can't replace itself in {} (an all-users install?). Run `ofm update` as \
             administrator or with sudo, or install {} from {}",
            dir.display(),
            update.version,
            update.page
        );
    }

    let client = client()?;
    let sums = client
        .get(&update.sums.browser_download_url)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.text())
        .context("downloading SHA256SUMS.txt")?;
    let expected = hash_from_sums(&sums, &update.binary.name)
        .with_context(|| format!("{SUMS} has no entry for {}", update.binary.name))?;

    let mut bytes = Vec::new();
    client
        .get(&update.binary.browser_download_url)
        .send()
        .and_then(|r| r.error_for_status())
        // copy_to rather than bytes(): bytes() must finish within a single timeout
        .and_then(|mut r| r.copy_to(&mut bytes))
        .with_context(|| format!("downloading {}", update.binary.name))?;
    let actual = hex::encode(Sha256::digest(&bytes));
    if actual != expected {
        bail!(
            "{} failed its SHA-256 check (expected {expected}, got {actual}); nothing was changed",
            update.binary.name
        );
    }

    // next to the binary rather than in a shared temp folder another user could plant in
    let new_exe = dir.join(format!(".{}.new", update.binary.name));
    std::fs::write(&new_exe, &bytes).with_context(|| format!("writing {}", new_exe.display()))?;
    let replaced = self_replace::self_replace(&new_exe).context("replacing the ofm binary");
    let _ = std::fs::remove_file(&new_exe);
    replaced
}

// actually tries a write; permission bits don't tell the truth on windows
fn dir_is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".ofm-write-test-{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

// sha256sum format: "<hex>  <name>", with a '*' before the name in binary mode
fn hash_from_sums(sums: &str, file: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        (name == file).then(|| hash.to_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // everything release.yml uploads for v1.2.0
    const V120: &[&str] = &[
        "ofm-1.2.0-setup-x64.exe",
        "ofm-1.2.0-setup-x64.exe.torrent",
        "ofm-1.2.0-x64.msi",
        "ofm-1.2.0-x86.msi",
        "ofm-1.2.0-arm64.msi",
        "ofm-v1.2.0-x64-portable.exe",
        "ofm-v1.2.0-x64-portable.exe.torrent",
        "ofm-v1.2.0-x64-portable.exe.magnet.png",
        "ofm-v1.2.0-x86-portable.exe",
        "ofm-v1.2.0-arm64-portable.exe",
        "ofm-v1.2.0-portable.zip",
        "ofm-v1.2.0-linux-x64.tar.gz",
        "ofm-v1.2.0-linux-x64.tar.gz.torrent",
        "ofm-v1.2.0-linux-x64",
        "ofm-v1.2.0-linux-x64.torrent",
        "ofm-v1.2.0-linux-x64.magnet.png",
        "install.ps1",
        "install.cmd",
        "SHA256SUMS.txt",
    ];

    fn release(tag: &str, files: &[&str]) -> Release {
        Release {
            tag_name: tag.into(),
            html_url: format!("https://github.com/Londopy/ofm/releases/tag/{tag}"),
            assets: files
                .iter()
                .map(|name| Asset {
                    name: name.to_string(),
                    browser_download_url: format!(
                        "https://github.com/Londopy/ofm/releases/download/{tag}/{name}"
                    ),
                })
                .collect(),
        }
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn each_platform_gets_its_own_build() {
        let cases = [
            ("windows", "x86_64", "ofm-v1.2.0-x64-portable.exe"),
            ("windows", "x86", "ofm-v1.2.0-x86-portable.exe"),
            ("windows", "aarch64", "ofm-v1.2.0-arm64-portable.exe"),
            ("linux", "x86_64", "ofm-v1.2.0-linux-x64"),
        ];
        for (os, arch, want) in cases {
            let suffix = asset_suffix(os, arch).unwrap();
            let update = pick(release("v1.2.0", V120), &v("1.1.0"), suffix)
                .unwrap()
                .unwrap();
            assert_eq!(update.binary.name, want, "{os}/{arch}");
            assert_eq!(update.sums.name, "SHA256SUMS.txt");
            assert_eq!(update.version, v("1.2.0"));
        }
    }

    #[test]
    fn platforms_without_a_release_build() {
        assert_eq!(asset_suffix("macos", "aarch64"), None);
        assert_eq!(asset_suffix("linux", "aarch64"), None);
    }

    #[test]
    fn only_a_newer_release_is_an_update() {
        let suffix = asset_suffix("windows", "x86_64").unwrap();
        for current in ["1.2.0", "1.2.1", "2.0.0"] {
            let found = pick(release("v1.2.0", V120), &v(current), suffix).unwrap();
            assert!(found.is_none(), "running {current}");
        }
        for current in ["1.1.9", "1.2.0-rc1", "0.1.0"] {
            let found = pick(release("v1.2.0", V120), &v(current), suffix).unwrap();
            assert!(found.is_some(), "running {current}");
        }
    }

    #[test]
    fn newer_release_missing_our_build_or_checksums_is_an_error() {
        let suffix = asset_suffix("windows", "aarch64").unwrap();
        let no_arm64 = release("v1.2.0", &["ofm-v1.2.0-x64-portable.exe", "SHA256SUMS.txt"]);
        assert!(pick(no_arm64, &v("1.1.0"), suffix).is_err());
        let no_sums = release("v1.2.0", &["ofm-v1.2.0-arm64-portable.exe"]);
        assert!(pick(no_sums, &v("1.1.0"), suffix).is_err());
        assert!(pick(release("nightly", V120), &v("1.1.0"), suffix).is_err());
    }

    #[test]
    fn sums_lookup_matches_whole_file_names() {
        let sums = "ABCDEF0123  ofm-v1.2.0-x64-portable.exe\r\n\
                    111111  ofm-v1.2.0-x64-portable.exe.torrent\n\
                    \n\
                    222222 *ofm-v1.2.0-linux-x64\n\
                    333333  ofm-v1.2.0-linux-x64.tar.gz\n";
        assert_eq!(
            hash_from_sums(sums, "ofm-v1.2.0-x64-portable.exe").as_deref(),
            Some("abcdef0123")
        );
        assert_eq!(
            hash_from_sums(sums, "ofm-v1.2.0-linux-x64").as_deref(),
            Some("222222")
        );
        assert_eq!(hash_from_sums(sums, "ofm-v1.2.0-x86-portable.exe"), None);
    }

    #[test]
    fn write_probe_cleans_up_after_itself() {
        let dir = std::env::temp_dir().join(format!("ofm-probe-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(dir_is_writable(&dir));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir(&dir).unwrap();
        assert!(!dir_is_writable(&dir));
    }
}
