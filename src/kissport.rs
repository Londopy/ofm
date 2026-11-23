// KISS over TCP for connectionless frames: every client can send, every client hears
// every frame that passed its CRCs

use crate::host::{incoming, read_until_closed, writer, Listening};
use crate::station::KissOut;
use ofm_core::kiss;
use std::io::{self, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
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

pub fn serve(port: u16, tx: Sender<Vec<u8>>) -> io::Result<(Clients, Listening)> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let clients = Clients::default();
    let closed = Arc::new(AtomicBool::new(false));
    let all = clients.clone();
    let mut listening = Listening::new(closed.clone(), move || {
        for c in all.0.lock().unwrap().drain(..) {
            let _ = c.shutdown(Shutdown::Both);
        }
    });
    let streams = incoming(listener, closed.clone())?;
    let list = clients.clone();
    listening.watch(thread::spawn(move || {
        for stream in streams {
            let Ok(write) = writer(&stream) else {
                continue;
            };
            list.0.lock().unwrap().push(write);
            let (tx, closed) = (tx.clone(), closed.clone());
            thread::spawn(move || {
                let mut decoder = kiss::Decoder::default();
                read_until_closed(stream, &closed, |bytes| {
                    decoder
                        .feed(bytes)
                        .into_iter()
                        .all(|frame| tx.send(frame).is_ok())
                });
            });
        }
    }));
    Ok((clients, listening))
}
