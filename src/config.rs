// settings in %APPDATA%\ofm\config.toml (~/.config/ofm on linux), or next to the exe
// when a file named `portable` sits beside it

use anyhow::{Context, Result};
use ofm_core::modem::LEVELS;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileName {
    Narrow,
    Wide,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PttMethod {
    /// The client keys the radio (Pat with ptt_ctrl and rigctld), or the radio uses VOX.
    Client,
    Vox,
    Rts,
    Dtr,
    Cm108,
    Rigctld,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    pub input: Option<String>,
    pub output: Option<String>,
    /// Multiplies the transmit audio; set it with `ofm calibrate`.
    pub tx_gain: f32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        AudioConfig {
            input: None,
            output: None,
            tx_gain: 1.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RadioConfig {
    pub profile: ProfileName,
    pub txdelay_ms: u32,
    pub papr_db: f32,
    pub cwid: bool,
}

impl Default for RadioConfig {
    fn default() -> Self {
        RadioConfig {
            profile: ProfileName::Narrow,
            txdelay_ms: 200,
            papr_db: 9.0,
            cwid: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PttConfig {
    pub method: PttMethod,
    pub serial_port: String,
    pub cm108_path: Option<String>,
    pub cm108_gpio: u8,
    pub rigctld: String,
}

impl Default for PttConfig {
    fn default() -> Self {
        PttConfig {
            method: PttMethod::Client,
            serial_port: if cfg!(windows) {
                "COM3".into()
            } else {
                "/dev/ttyUSB0".into()
            },
            cm108_path: None,
            cm108_gpio: 3,
            rigctld: "127.0.0.1:4532".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostConfig {
    /// Command port; the data port is this plus one.
    pub port: u16,
    pub max_retries: u32,
}

impl Default for HostConfig {
    fn default() -> Self {
        HostConfig {
            port: 8300,
            max_retries: 10,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KissConfig {
    pub enabled: bool,
    pub port: u16,
    pub level: u8,
}

impl Default for KissConfig {
    fn default() -> Self {
        KissConfig {
            enabled: true,
            port: 8310,
            level: 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdaterConfig {
    pub check_on_start: bool,
}

impl Default for UpdaterConfig {
    fn default() -> Self {
        UpdaterConfig {
            check_on_start: true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub audio: AudioConfig,
    pub radio: RadioConfig,
    pub ptt: PttConfig,
    pub host: HostConfig,
    pub kiss: KissConfig,
    pub updater: UpdaterConfig,
}

pub fn dir() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    if let Some(exe_dir) = exe.parent() {
        if exe_dir.join("portable").exists() {
            return Ok(exe_dir.to_path_buf());
        }
    }
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    Ok(base
        .context("can't find the config folder (APPDATA or HOME is not set)")?
        .join("ofm"))
}

pub fn path() -> Result<PathBuf> {
    Ok(dir()?.join("config.toml"))
}

impl Config {
    /// Loads the config, writing the defaults out on first run.
    pub fn load() -> Result<Config> {
        let path = path()?;
        if !path.exists() {
            let cfg = Config::default();
            cfg.save()?;
            return Ok(cfg);
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.radio.txdelay_ms = cfg.radio.txdelay_ms.clamp(50, 500);
        cfg.kiss.level = cfg.kiss.level.min(LEVELS.len() as u8 - 1);
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_roundtrips_through_toml() {
        let cfg = Config::default();
        let back: Config = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn missing_keys_fall_back_to_defaults() {
        let cfg: Config = toml::from_str("[radio]\nprofile = \"wide\"\n").unwrap();
        assert_eq!(cfg.radio.profile, ProfileName::Wide);
        assert_eq!(cfg.radio.txdelay_ms, 200);
        assert_eq!(cfg.host.port, 8300);
    }
}
