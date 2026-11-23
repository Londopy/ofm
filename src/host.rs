// VARA-style host interface: a text command port and a raw data port on the next port up.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

// commands are short; anything longer without a CR is junk
const MAX_LINE: usize = 1024;

/// A write handle for a client. Writes happen on the modem loop, so a client that stops
/// reading gets cut off after a short wait instead of stalling PTT and the receiver.
pub fn writer(stream: &TcpStream) -> io::Result<TcpStream> {
    let w = stream.try_clone()?;
    w.set_write_timeout(Some(Duration::from_secs(2)))?;
    w.set_nodelay(true)?;
    Ok(w)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    MyCall(Vec<String>),
    Listen(bool),
    Connect { src: String, dst: String },
    Disconnect,
    Abort,
    Compression,
    Public,
    CwId(bool),
    Version,
    // accepted for compatibility, nothing to do for an FM modem
    Ignored,
    Wrong,
}

fn on_off(arg: Option<&str>) -> Option<bool> {
    match arg?.to_ascii_uppercase().as_str() {
        "ON" => Some(true),
        "OFF" => Some(false),
        _ => None,
    }
}

pub fn parse(line: &str) -> Command {
    let mut parts = line.split_whitespace();
    let Some(word) = parts.next() else {
        return Command::Wrong;
    };
    let args: Vec<&str> = parts.collect();
    let upper = line.trim().to_ascii_uppercase();
    match word.to_ascii_uppercase().as_str() {
        "MYCALL" if (1..=5).contains(&args.len()) => {
            Command::MyCall(args.iter().map(|s| s.to_string()).collect())
        }
        "LISTEN" => on_off(args.first().copied()).map_or(Command::Wrong, Command::Listen),
        "CONNECT" if args.len() == 2 => Command::Connect {
            src: args[0].to_string(),
            dst: args[1].to_string(),
        },
        "DISCONNECT" if args.is_empty() => Command::Disconnect,
        "ABORT" if args.is_empty() => Command::Abort,
        "COMPRESSION"
            if matches!(
                args.first().map(|a| a.to_ascii_uppercase()).as_deref(),
                Some("OFF" | "TEXT" | "FILES")
            ) =>
        {
            Command::Compression
        }
        "PUBLIC" if on_off(args.first().copied()).is_some() => Command::Public,
        "CWID" => on_off(args.first().copied()).map_or(Command::Wrong, Command::CwId),
        "VERSION" if args.is_empty() => Command::Version,
        "BW500" | "BW2300" | "BW2750" | "CHAT" | "CQFRAME" => Command::Ignored,
        _ if upper == "WINLINK SESSION" || upper == "P2P SESSION" => Command::Ignored,
        _ => Command::Wrong,
    }
}

/// What the host connection threads hand to the station loop.
pub enum HostInput {
    Command(Command),
    Data(Vec<u8>),
    /// The command port client went away.
    Closed,
}

/// Write side of whichever client is attached to a port.
#[derive(Clone, Default)]
pub struct Port(Arc<Mutex<Option<TcpStream>>>);

impl Port {
    fn attach(&self, s: TcpStream) {
        *self.0.lock().unwrap() = Some(s);
    }

    fn detach(&self) {
        *self.0.lock().unwrap() = None;
    }

    pub fn is_attached(&self) -> bool {
        self.0.lock().unwrap().is_some()
    }

    // its reader thread sees the socket close and lets go of the client
    fn close(&self) {
        if let Some(s) = self.0.lock().unwrap().take() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    /// Sends one command-port line. Pat splits each read on CR without carrying partial
    /// lines over, so every line goes out in a single write.
    pub fn line(&self, text: &str) {
        self.write(format!("{text}\r").as_bytes());
    }

    pub fn write(&self, bytes: &[u8]) {
        let mut guard = self.0.lock().unwrap();
        if let Some(s) = guard.as_mut() {
            if let Err(e) = s.write_all(bytes) {
                eprintln!("dropping host client: {e}");
                // the reader thread sees the close and reports it
                let _ = s.shutdown(Shutdown::Both);
                *guard = None;
            }
        }
    }
}

/// Splits a byte stream into CR (or LF) terminated lines, keeping partial lines between reads.
#[derive(Default)]
pub struct LineSplitter(Vec<u8>);

impl LineSplitter {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for &b in bytes {
            if b == b'\r' || b == b'\n' {
                if !self.0.is_empty() {
                    out.push(String::from_utf8_lossy(&self.0).trim().to_string());
                    self.0.clear();
                }
            } else if self.0.len() < MAX_LINE {
                self.0.push(b);
            }
        }
        out
    }
}

/// A running server's sockets. Dropping it drops the clients and closes the listening ports,
/// so the next modem (after a settings change in the window) can open them again.
pub struct Listening {
    closed: Arc<AtomicBool>,
    close_clients: Box<dyn Fn() + Send>,
    threads: Vec<JoinHandle<()>>,
}

impl Listening {
    pub fn new(closed: Arc<AtomicBool>, close_clients: impl Fn() + Send + 'static) -> Listening {
        Listening {
            closed,
            close_clients: Box::new(close_clients),
            threads: Vec::new(),
        }
    }

    pub fn watch(&mut self, thread: JoinHandle<()>) {
        self.threads.push(thread);
    }
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        (self.close_clients)();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

const ACCEPT_POLL: Duration = Duration::from_millis(50);

/// Hands what a client sends to `each` until the client hangs up, `each` returns false, or
/// `closed` is set. Reads time out now and then to look at `closed`: on Windows, shutting a
/// socket down from another thread does not wake a read blocked on it.
pub fn read_until_closed(
    mut stream: TcpStream,
    closed: &AtomicBool,
    mut each: impl FnMut(&[u8]) -> bool,
) {
    if stream.set_read_timeout(Some(ACCEPT_POLL)).is_err() {
        return;
    }
    let mut buf = [0u8; 4096];
    while !closed.load(Ordering::SeqCst) {
        match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                if !each(&buf[..n]) {
                    return;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return,
        }
    }
}

/// Connections on `listener` until `closed` is set. The listener is polled rather than
/// blocked on so that it can be closed; the connections it hands out block as usual.
pub fn incoming(
    listener: TcpListener,
    closed: Arc<AtomicBool>,
) -> io::Result<impl Iterator<Item = TcpStream>> {
    listener.set_nonblocking(true)?;
    Ok(std::iter::from_fn(move || loop {
        if closed.load(Ordering::SeqCst) {
            return None;
        }
        match listener.accept() {
            Ok((s, _)) if s.set_nonblocking(false).is_ok() => return Some(s),
            // a client that hung up before we got to it, or nobody waiting yet
            _ => thread::sleep(ACCEPT_POLL),
        }
    }))
}

/// Binds the command port and the data port (command port + 1) and serves one client on
/// each at a time. Returns the write handles for both.
pub fn serve(cmd_port: u16, tx: Sender<HostInput>) -> io::Result<(Port, Port, Listening)> {
    let data_port = cmd_port.checked_add(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("command port {cmd_port} leaves no room for the data port above it"),
        )
    })?;
    let cmd = TcpListener::bind(("127.0.0.1", cmd_port))?;
    let data = TcpListener::bind(("127.0.0.1", data_port))?;
    let (cmd_out, data_out) = (Port::default(), Port::default());
    let closed = Arc::new(AtomicBool::new(false));
    let (c, d) = (cmd_out.clone(), data_out.clone());
    let mut listening = Listening::new(closed.clone(), move || {
        c.close();
        d.close();
    });
    let cmd = incoming(cmd, closed.clone())?;
    let data = incoming(data, closed.clone())?;

    let (out, tx_cmd, shut) = (cmd_out.clone(), tx.clone(), closed.clone());
    listening.watch(thread::spawn(move || {
        for stream in cmd {
            let Ok(write) = writer(&stream) else {
                continue;
            };
            out.attach(write);
            let mut split = LineSplitter::default();
            read_until_closed(stream, &shut, |bytes| {
                split
                    .feed(bytes)
                    .into_iter()
                    .all(|line| tx_cmd.send(HostInput::Command(parse(&line))).is_ok())
            });
            out.detach();
            if tx_cmd.send(HostInput::Closed).is_err() {
                return;
            }
        }
    }));

    let out = data_out.clone();
    listening.watch(thread::spawn(move || {
        for stream in data {
            let Ok(write) = writer(&stream) else {
                continue;
            };
            out.attach(write);
            read_until_closed(stream, &closed, |bytes| {
                tx.send(HostInput::Data(bytes.to_vec())).is_ok()
            });
            out.detach();
        }
    }));
    Ok((cmd_out, data_out, listening))
}

/// A test server on ports the OS says are free, and the command port it got. Fixed or
/// derived port numbers collide with each other and with whatever else the machine runs.
#[cfg(test)]
pub fn serve_anywhere(tx: Sender<HostInput>) -> (u16, Port, Port, Listening) {
    for _ in 0..50 {
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .unwrap()
            .port();
        // the data port above it may be taken; then try another
        if let Ok((cmd, data, listening)) = serve(port, tx.clone()) {
            return (port, cmd, data, listening);
        }
    }
    panic!("found no free pair of ports in 50 tries");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_commands_pat_sends() {
        assert_eq!(
            parse("MYCALL N0CALL"),
            Command::MyCall(vec!["N0CALL".into()])
        );
        assert_eq!(parse("LISTEN OFF"), Command::Listen(false));
        assert_eq!(parse("COMPRESSION TEXT"), Command::Compression);
        assert_eq!(parse("PUBLIC ON"), Command::Public);
        assert_eq!(
            parse("CONNECT N0CALL W1AW-10"),
            Command::Connect {
                src: "N0CALL".into(),
                dst: "W1AW-10".into()
            }
        );
        assert_eq!(parse("VERSION"), Command::Version);
        assert_eq!(parse("DISCONNECT"), Command::Disconnect);
        assert_eq!(parse("ABORT"), Command::Abort);
    }

    #[test]
    fn accepts_other_clients_commands_and_rejects_junk() {
        assert_eq!(parse("BW2300"), Command::Ignored);
        assert_eq!(parse("WINLINK SESSION"), Command::Ignored);
        assert_eq!(parse("P2P SESSION"), Command::Ignored);
        assert_eq!(parse("CWID OFF"), Command::CwId(false));
        assert_eq!(parse("MYCALL A B C D E F"), Command::Wrong);
        assert_eq!(parse("CONNECT N0CALL"), Command::Wrong);
        assert_eq!(parse("FROB"), Command::Wrong);
        assert_eq!(parse("LISTEN MAYBE"), Command::Wrong);
    }

    #[test]
    fn splitter_keeps_partial_lines() {
        let mut s = LineSplitter::default();
        assert_eq!(s.feed(b"PUBLIC ON\rCOMPRESS"), vec!["PUBLIC ON"]);
        assert_eq!(
            s.feed(b"ION TEXT\rMYCALL N0CALL\r"),
            vec!["COMPRESSION TEXT", "MYCALL N0CALL"]
        );
    }

    #[test]
    fn closing_a_server_drops_its_client_and_frees_the_ports() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (port, cmd, _data, listening) = serve_anywhere(tx);
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        client.write_all(b"VERSION\r").unwrap();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(HostInput::Command(Command::Version))
        ));
        assert!(cmd.is_attached());

        drop(listening);
        let mut buf = [0u8; 16];
        // a reset counts as closed; a read that times out means the server never let go
        let read = client.read(&mut buf);
        assert!(
            matches!(&read, Ok(0))
                || matches!(&read, Err(e) if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                )),
            "{read:?}"
        );
        assert!(!cmd.is_attached());
        // the next modem can take the same ports straight away
        let (tx, _rx) = std::sync::mpsc::channel();
        let again = serve(port, tx);
        assert!(again.is_ok(), "{:?}", again.err());
    }
}
