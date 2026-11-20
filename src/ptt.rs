// ways to key the transmitter: left to the client or VOX, a serial control line,
// a CM108-style HID GPIO (Digirig, AIOC and similar), or hamlib's rigctld

use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

pub trait Ptt {
    fn set(&mut self, on: bool) -> Result<()>;
}

/// The client keys the radio after our PTT ON/OFF lines, or the radio uses VOX.
pub struct External;

impl Ptt for External {
    fn set(&mut self, _on: bool) -> Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Line {
    Rts,
    Dtr,
}

pub struct Serial {
    port: Box<dyn serialport::SerialPort>,
    line: Line,
}

impl Serial {
    pub fn open(name: &str, line: Line) -> Result<Serial> {
        let port = serialport::new(name, 9600)
            .timeout(Duration::from_millis(500))
            .open()
            .with_context(|| format!("opening {name} for PTT"))?;
        let mut s = Serial { port, line };
        s.set(false)?;
        Ok(s)
    }
}

impl Ptt for Serial {
    fn set(&mut self, on: bool) -> Result<()> {
        match self.line {
            Line::Rts => self.port.write_request_to_send(on)?,
            Line::Dtr => self.port.write_data_terminal_ready(on)?,
        }
        Ok(())
    }
}

const CMEDIA_VID: u16 = 0x0d8c;
const AIOC_VID: u16 = 0x1209;
const AIOC_PID: u16 = 0x7388;

pub struct Cm108 {
    dev: hidapi::HidDevice,
    gpio: u8,
}

impl Cm108 {
    /// Opens the HID device at `path`, or else the first C-Media or AIOC HID interface that
    /// takes a GPIO report.
    pub fn open(path: Option<&str>, gpio: u8) -> Result<Cm108> {
        if !(1..=8).contains(&gpio) {
            bail!("cm108 gpio must be 1 to 8, got {gpio}");
        }
        let api = hidapi::HidApi::new()?;
        if let Some(p) = path {
            let dev = api.open_path(&std::ffi::CString::new(p)?)?;
            let mut c = Cm108 { dev, gpio };
            c.set(false)?;
            return Ok(c);
        }
        // a sound card can show up as several HID interfaces; only one of them has the GPIOs
        let found: Vec<_> = api
            .device_list()
            .filter(|d| {
                d.vendor_id() == CMEDIA_VID
                    || (d.vendor_id() == AIOC_VID && d.product_id() == AIOC_PID)
            })
            .collect();
        for info in &found {
            let Ok(dev) = info.open_device(&api) else {
                continue;
            };
            let mut c = Cm108 { dev, gpio };
            if c.set(false).is_ok() {
                eprintln!(
                    "cm108 ptt on {} (set cm108_path to pick another)",
                    info.path().to_string_lossy()
                );
                return Ok(c);
            }
        }
        if found.is_empty() {
            bail!("no CM108-compatible device found");
        }
        bail!(
            "found {} CM108-compatible HID interfaces but none took a GPIO report; set cm108_path",
            found.len()
        )
    }
}

impl Ptt for Cm108 {
    fn set(&mut self, on: bool) -> Result<()> {
        // report 0: [report id, 0, gpio data, gpio direction mask, 0], as the cm108 datasheet lays out
        let bit = 1u8 << (self.gpio - 1);
        let report = [0, 0, if on { bit } else { 0 }, bit, 0];
        self.dev.write(&report)?;
        Ok(())
    }
}

pub struct Rigctld {
    addr: SocketAddr,
    conn: Option<(TcpStream, BufReader<TcpStream>)>,
}

impl Rigctld {
    pub fn connect(addr: &str) -> Result<Rigctld> {
        let addr = addr
            .parse()
            .with_context(|| format!("rigctld address {addr:?} is not host:port"))?;
        let conn = Some(Rigctld::open(addr)?);
        Ok(Rigctld { addr, conn })
    }

    fn open(addr: SocketAddr) -> Result<(TcpStream, BufReader<TcpStream>)> {
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
            .with_context(|| format!("connecting to rigctld at {addr}"))?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok((stream, reader))
    }

    fn send(&mut self, on: bool) -> Result<()> {
        let (stream, reader) = match &mut self.conn {
            Some(c) => c,
            None => self.conn.insert(Rigctld::open(self.addr)?),
        };
        stream.write_all(if on { b"T 1\n" } else { b"T 0\n" })?;
        let mut reply = String::new();
        reader.read_line(&mut reply)?;
        match reply.trim() {
            "RPRT 0" => Ok(()),
            other => bail!("rigctld refused PTT: {other}"),
        }
    }
}

impl Ptt for Rigctld {
    fn set(&mut self, on: bool) -> Result<()> {
        let r = self.send(on);
        if r.is_err() {
            // a reply that turns up late would be read as the answer to the next command
            self.conn = None;
        }
        r
    }
}
