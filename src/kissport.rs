// KISS over TCP for connectionless frames: every client can send, every client hears
// every frame that passed its CRCs

use crate::host::writer;
use crate::station::KissOut;
use ofm_core::kiss;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Clone, Default)]
pub struct Clients(Arc<Mutex<Vec<TcpStream>>>);

impl KissOut for Clients {
    fn deliver(&self, frame: &[u8]) {
        let wire = kiss::encode(frame);
        self.0.lock().unwrap().retain_mut(|c| {
            let ok = c.write_all(&wire).is_ok();
            if !ok {
                // its reader thread sees the close and stops taking frames from it
                let _ = c.shutdown(Shutdown::Both);
            }
            ok
        });
    }
}

pub fn serve(port: u16, tx: Sender<Vec<u8>>) -> io::Result<Clients> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let clients = Clients::default();
    let list = clients.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let Ok(write) = writer(&stream) else {
                continue;
            };
            list.0.lock().unwrap().push(write);
            let tx = tx.clone();
            thread::spawn(move || {
                let mut decoder = kiss::Decoder::default();
                let mut buf = [0u8; 4096];
                let mut reader = stream;
                while let Ok(n @ 1..) = reader.read(&mut buf) {
                    for frame in decoder.feed(&buf[..n]) {
                        if tx.send(frame).is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    Ok(clients)
}
