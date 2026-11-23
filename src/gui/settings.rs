// the settings tab's fields as typed, and turning them back into a config

use crate::config::{Config, ProfileName, PttMethod};
use std::net::SocketAddr;

/// The fields a problem can belong to, so the window can show it beside them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    SerialPort,
    Rigctld,
    HostPort,
    KissPort,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Form {
    pub input: Option<String>,
    pub output: Option<String>,
    pub profile: ProfileName,
    pub txdelay_ms: u32,
    pub cwid: bool,
    pub ptt: PttMethod,
    pub serial_port: String,
    /// Empty picks the first CM108 interface found.
    pub cm108_path: String,
    pub cm108_gpio: u8,
    pub rigctld: String,
    pub host_port: String,
    pub kiss_enabled: bool,
    pub kiss_port: String,
    pub kiss_level: u8,
    pub check_updates: bool,
}

impl Form {
    pub fn from_config(cfg: &Config) -> Form {
        Form {
            input: cfg.audio.input.clone(),
            output: cfg.audio.output.clone(),
            profile: cfg.radio.profile,
            txdelay_ms: cfg.radio.txdelay_ms,
            cwid: cfg.radio.cwid,
            ptt: cfg.ptt.method,
            serial_port: cfg.ptt.serial_port.clone(),
            cm108_path: cfg.ptt.cm108_path.clone().unwrap_or_default(),
            cm108_gpio: cfg.ptt.cm108_gpio,
            rigctld: cfg.ptt.rigctld.clone(),
            host_port: cfg.host.port.to_string(),
            kiss_enabled: cfg.kiss.enabled,
            kiss_port: cfg.kiss.port.to_string(),
            kiss_level: cfg.kiss.level,
            check_updates: cfg.updater.check_on_start,
        }
    }

    /// `base` with these settings applied, or every reason they can't be.
    pub fn to_config(&self, base: &Config) -> Result<Config, Vec<(Field, String)>> {
        let mut problems = Vec::new();
        let mut cfg = base.clone();
        cfg.audio.input.clone_from(&self.input);
        cfg.audio.output.clone_from(&self.output);
        cfg.radio.profile = self.profile;
        cfg.radio.txdelay_ms = self.txdelay_ms.clamp(50, 500);
        cfg.radio.cwid = self.cwid;
        cfg.ptt.method = self.ptt;
        cfg.ptt.serial_port = self.serial_port.trim().to_string();
        let path = self.cm108_path.trim();
        cfg.ptt.cm108_path = (!path.is_empty()).then(|| path.to_string());
        cfg.ptt.cm108_gpio = self.cm108_gpio.clamp(1, 8);
        cfg.ptt.rigctld = self.rigctld.trim().to_string();
        cfg.kiss.enabled = self.kiss_enabled;
        cfg.kiss.level = self.kiss_level.min(9);
        cfg.updater.check_on_start = self.check_updates;

        if matches!(self.ptt, PttMethod::Rts | PttMethod::Dtr) && cfg.ptt.serial_port.is_empty() {
            problems.push((
                Field::SerialPort,
                "Name the serial port whose RTS or DTR line keys the radio.".into(),
            ));
        }
        if self.ptt == PttMethod::Rigctld && cfg.ptt.rigctld.parse::<SocketAddr>().is_err() {
            problems.push((
                Field::Rigctld,
                format!(
                    "\"{}\" is not a rigctld address; it looks like 127.0.0.1:4532.",
                    cfg.ptt.rigctld
                ),
            ));
        }
        match self.host_port.trim().parse::<u16>() {
            Ok(p) if (1..=65534).contains(&p) => cfg.host.port = p,
            _ => problems.push((
                Field::HostPort,
                format!(
                    "\"{}\" is not a command port; use a number from 1 to 65534 (the data port is one up).",
                    self.host_port.trim()
                ),
            )),
        }
        if self.kiss_enabled {
            match self.kiss_port.trim().parse::<u16>() {
                Ok(0) | Err(_) => problems.push((
                    Field::KissPort,
                    format!(
                        "\"{}\" is not a KISS port; use a number from 1 to 65535.",
                        self.kiss_port.trim()
                    ),
                )),
                Ok(p) if p == cfg.host.port || Some(p) == cfg.host.port.checked_add(1) => problems
                    .push((
                        Field::KissPort,
                        format!("KISS port {p} is already the command or data port for Pat."),
                    )),
                Ok(p) => cfg.kiss.port = p,
            }
        }
        if problems.is_empty() {
            Ok(cfg)
        } else {
            Err(problems)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_settings_give_back_the_same_config() {
        let mut cfg = Config::default();
        cfg.audio.input = Some("USB Audio".into());
        cfg.ptt.cm108_path = Some(r"\\?\hid#vid_0d8c".into());
        assert_eq!(Form::from_config(&cfg).to_config(&cfg), Ok(cfg));
    }

    #[test]
    fn edits_land_in_the_config() {
        let cfg = Config::default();
        let mut f = Form::from_config(&cfg);
        f.profile = ProfileName::Wide;
        f.host_port = " 8500 ".into();
        f.kiss_port = "8510".into();
        f.ptt = PttMethod::Rigctld;
        f.rigctld = "192.168.1.20:4532".into();
        f.cm108_path = "  ".into();
        let out = f.to_config(&cfg).unwrap();
        assert_eq!(out.radio.profile, ProfileName::Wide);
        assert_eq!((out.host.port, out.kiss.port), (8500, 8510));
        assert_eq!(out.ptt.rigctld, "192.168.1.20:4532");
        assert_eq!(out.ptt.cm108_path, None);
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let mut f = Form::from_config(&Config::default());
        f.host_port = "65535".into();
        f.kiss_port = "nope".into();
        f.ptt = PttMethod::Rigctld;
        f.rigctld = "localhost".into();
        let problems = f.to_config(&Config::default()).unwrap_err();
        let fields: Vec<Field> = problems.iter().map(|(f, _)| *f).collect();
        assert_eq!(
            fields,
            [Field::Rigctld, Field::HostPort, Field::KissPort],
            "{problems:?}"
        );
    }

    #[test]
    fn the_kiss_port_may_not_take_pat_s_ports() {
        let mut f = Form::from_config(&Config::default());
        for clash in ["8300", "8301"] {
            f.kiss_port = clash.into();
            assert!(f.to_config(&Config::default()).is_err(), "{clash}");
        }
        // a disabled KISS port is not checked
        f.kiss_enabled = false;
        assert!(f.to_config(&Config::default()).is_ok());
    }

    #[test]
    fn a_serial_ptt_needs_a_port() {
        let mut f = Form::from_config(&Config::default());
        f.ptt = PttMethod::Rts;
        f.serial_port = " ".into();
        assert!(f.to_config(&Config::default()).is_err());
        f.ptt = PttMethod::Client;
        assert!(f.to_config(&Config::default()).is_ok());
    }
}
