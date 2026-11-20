//! Connected-mode link: connect handshake, selective-repeat data transfer with
//! piggybacked ACKs, rate adaptation and disconnect. Pure logic; time comes in as
//! milliseconds and transmissions go out as frames for the audio side to send.

use crate::modem::{frame_samples, Chunk, Header, Kind, Profile, RxFrame, FS, LEVELS, MAX_CHUNKS};
use std::collections::{BTreeMap, VecDeque};

const ID_INTERVAL_MS: u64 = 10 * 60 * 1000;
const TURNAROUND_MS: u64 = 120;
const CONNECT_TRIES: u32 = 6;
// a link with nothing heard for this long is closed
const IDLE_MS: u64 = 10 * 60 * 1000;
// whoever sent the DISCONNECT ACK sends its CW ID first; the other side holds off this long
const CW_HOLDOFF_MS: u64 = 1500;
// audio that never stops (hiss after a level change, a stuck input) stops counting after this
const MAX_HOLD_MS: u64 = 30_000;
// headroom above a level's threshold before we ask for it
const SNR_MARGIN: f32 = 1.5;

/// Link settings that come from the operator's config.
#[derive(Clone, Debug)]
pub struct LinkConfig {
    pub profile: Profile,
    pub txdelay_ms: u32,
    /// Unanswered transmissions before the link is declared lost.
    pub max_retries: u32,
}

/// Something the link wants done.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    /// Key up and send these frames back to back.
    Transmit(Vec<(Header, Vec<Chunk>)>),
    /// Send a CW ID for `call` once the channel is quiet, but not before `wait_ms` from now.
    /// The driver decides whether CW ID is enabled.
    CwId {
        call: String,
        wait_ms: u64,
    },
    Host(HostEvent),
}

/// Messages for the host on the command or data port.
#[derive(Clone, Debug, PartialEq)]
pub enum HostEvent {
    Pending,
    CancelPending,
    /// `src` and `dst` are as VARA reports them: caller first.
    Connected {
        src: String,
        dst: String,
        profile: Profile,
    },
    Disconnected,
    Buffer(usize),
    Bitrate {
        level: u8,
        bps: u32,
    },
    Snr(f32),
    Data(Vec<u8>),
}

/// What the receiver hears, for carrier sense.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sense {
    /// A frame is coming in.
    pub receiving: bool,
    /// In-band audio well above the noise floor.
    pub energy: bool,
    /// Either of those, or a preamble heard in the last second.
    pub busy: bool,
}

#[derive(Debug)]
enum Turn {
    // our move, not before this time
    Ours(u64),
    // an answer to the frame just heard; it goes as soon as that frame has ended
    Reply(u64),
    // we sent something that needs an answer; deadline is set when the transmission ends
    Waiting(Option<u64>),
    Idle,
}

#[derive(Debug)]
struct TxStream {
    base: u32,
    bytes: VecDeque<u8>,
    acked: Vec<(u32, u32)>,
    frames: VecDeque<(u8, Vec<(u32, u32)>)>,
}

impl TxStream {
    fn new() -> TxStream {
        TxStream {
            base: 0,
            bytes: VecDeque::new(),
            acked: Vec::new(),
            frames: VecDeque::new(),
        }
    }

    fn end(&self) -> u32 {
        self.base + self.bytes.len() as u32
    }

    fn unacked(&self) -> usize {
        self.bytes.len()
            - self
                .acked
                .iter()
                .map(|(s, e)| (e - s) as usize)
                .sum::<usize>()
    }

    fn has_work(&self) -> bool {
        self.unacked() > 0
    }

    fn mark(&mut self, start: u32, end: u32) {
        let (start, end) = (start.max(self.base), end);
        if start >= end {
            return;
        }
        self.acked.push((start, end));
        self.acked.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::new();
        for (s, e) in self.acked.drain(..) {
            match merged.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        self.acked = merged;
        if let Some(&(s, e)) = self.acked.first() {
            if s <= self.base {
                self.advance(e);
            }
        }
    }

    fn advance(&mut self, to: u32) {
        if to <= self.base {
            return;
        }
        let n = (to - self.base).min(self.bytes.len() as u32);
        self.bytes.drain(..n as usize);
        self.base += n;
        self.acked.retain(|&(_, e)| e > self.base);
        if let Some(first) = self.acked.first_mut() {
            first.0 = first.0.max(self.base);
        }
    }

    // anything not acked and already sent goes again first, then new bytes
    fn take(&self, cap: usize, max: usize) -> Vec<Chunk> {
        let mut out = Vec::new();
        let mut at = self.base;
        let mut acked = self.acked.iter().peekable();
        while out.len() < max && at < self.end() {
            while let Some(&&(s, e)) = acked.peek() {
                if e <= at {
                    acked.next();
                } else if s <= at {
                    at = e;
                    acked.next();
                } else {
                    break;
                }
            }
            if at >= self.end() {
                break;
            }
            let mut stop = self.end().min(at + cap as u32);
            if let Some(&&(s, _)) = acked.peek() {
                stop = stop.min(s);
            }
            let from = (at - self.base) as usize;
            let data: Vec<u8> = self
                .bytes
                .range(from..(stop - self.base) as usize)
                .copied()
                .collect();
            out.push(Chunk { offset: at, data });
            at = stop;
        }
        out
    }
}

#[derive(Debug, Default)]
struct RxStream {
    next: u32,
    held: BTreeMap<u32, Vec<u8>>,
}

impl RxStream {
    fn accept(&mut self, chunk: &Chunk) -> Vec<u8> {
        let end = chunk.offset + chunk.data.len() as u32;
        if end > self.next {
            let slot = self.held.entry(chunk.offset).or_default();
            if slot.len() < chunk.data.len() {
                slot.clone_from(&chunk.data);
            }
        }
        let mut out = Vec::new();
        while let Some((&off, _)) = self.held.first_key_value() {
            if off > self.next {
                break;
            }
            let data = self.held.remove(&off).unwrap_or_default();
            let skip = (self.next - off) as usize;
            if skip < data.len() {
                out.extend_from_slice(&data[skip..]);
                self.next = off + data.len() as u32;
            }
        }
        out
    }
}

#[derive(Debug)]
struct Session {
    mycall: String,
    peer: String,
    caller: bool,
    tx: TxStream,
    rx: RxStream,
    frame_seq: u8,
    level: u8,
    cap: u8,
    want: u8,
    clean: u8,
    snr: f32,
    // ack fields for the peer's latest frame
    ack_seq: u8,
    ack_bits: u64,
    owe_ack: bool,
    turn: Turn,
    retries: u32,
    closing: bool,
    last_id: u64,
    nonce: u32,
    // last time we heard the peer
    heard: u64,
    // the host has been told CONNECTED; a callee waits for the caller's first frame
    confirmed: bool,
}

#[derive(Debug)]
enum State {
    Idle,
    Connecting {
        mycall: String,
        peer: String,
        tries: u32,
        turn: Turn,
        nonce: u32,
        // the peer is dialling us too, and we let its Connect pass once
        crossed: bool,
    },
    Connected(Box<Session>),
    Disconnecting {
        mycall: String,
        peer: String,
        tries: u32,
        turn: Turn,
    },
}

/// The link state machine.
pub struct Link {
    cfg: LinkConfig,
    mycalls: Vec<String>,
    listen: bool,
    state: State,
    out: VecDeque<Out>,
    rng: u64,
    last_buffer: usize,
    // when the current unbroken stretch of channel energy began
    energy_since: Option<u64>,
}

fn best_level(snr: f32) -> u8 {
    LEVELS
        .iter()
        .rposition(|l| snr >= l.min_snr_db as f32 + SNR_MARGIN)
        .unwrap_or(0) as u8
}

impl Link {
    pub fn new(cfg: LinkConfig, seed: u64) -> Link {
        Link {
            cfg,
            mycalls: Vec::new(),
            listen: false,
            state: State::Idle,
            out: VecDeque::new(),
            rng: seed | 1,
            last_buffer: 0,
            energy_since: None,
        }
    }

    /// Next thing to do, if any.
    pub fn poll(&mut self) -> Option<Out> {
        self.out.pop_front()
    }

    pub fn set_mycalls(&mut self, calls: Vec<String>) {
        self.mycalls = calls;
    }

    pub fn is_connected(&self) -> bool {
        !matches!(self.state, State::Idle)
    }

    /// Turns listening on or off. Changing it during a link drops the link.
    pub fn set_listen(&mut self, on: bool) {
        if on != self.listen && self.is_connected() {
            self.abort();
        }
        self.listen = on;
    }

    pub fn connect(&mut self, now: u64, src: &str, dst: &str) {
        if self.is_connected() {
            return;
        }
        self.host(HostEvent::Pending);
        let nonce = (self.rng_next() >> 16) as u32;
        self.state = State::Connecting {
            mycall: src.to_string(),
            peer: dst.to_string(),
            tries: 0,
            turn: Turn::Ours(now),
            nonce,
            crossed: false,
        };
    }

    /// Queues bytes from the data port.
    pub fn send(&mut self, now: u64, data: &[u8]) {
        if let State::Connected(s) = &mut self.state {
            s.tx.bytes.extend(data);
            if matches!(s.turn, Turn::Idle) {
                let jitter = self.rng_next() % 300;
                if let State::Connected(s) = &mut self.state {
                    s.turn = Turn::Ours(now + jitter);
                }
            }
        }
        self.report_buffer();
    }

    /// Graceful close: drains queued data first.
    pub fn disconnect(&mut self, now: u64) {
        match &mut self.state {
            State::Connected(s) => {
                s.closing = true;
                if matches!(s.turn, Turn::Idle) {
                    s.turn = Turn::Ours(now);
                }
            }
            State::Connecting { .. } => self.abort(),
            State::Idle => self.host(HostEvent::Disconnected),
            State::Disconnecting { .. } => {}
        }
    }

    /// Drops the link at once. A peer still in the link gets one DISCONNECT, unanswered.
    pub fn abort(&mut self) {
        let calls = match &self.state {
            State::Connected(s) => Some((s.mycall.clone(), s.peer.clone())),
            State::Disconnecting { mycall, peer, .. } => Some((mycall.clone(), peer.clone())),
            // the peer may already have accepted a Connect we sent
            State::Connecting {
                mycall,
                peer,
                tries,
                turn,
                ..
            } if *tries > 0 || matches!(turn, Turn::Waiting(_)) => {
                Some((mycall.clone(), peer.clone()))
            }
            _ => None,
        };
        if let Some((mycall, peer)) = &calls {
            let h = Header::new(Kind::Disconnect, self.cfg.profile, mycall, peer);
            self.out.push_back(Out::Transmit(vec![(h, Vec::new())]));
        }
        if let Some(call) = self.drop_link() {
            if calls.is_some() {
                self.out.push_back(Out::CwId {
                    call,
                    wait_ms: CW_HOLDOFF_MS,
                });
            }
        }
    }

    /// Called when our transmission has finished playing.
    pub fn tx_done(&mut self, now: u64) {
        let wait = self.reply_wait();
        let turn = match &mut self.state {
            State::Connected(s) => &mut s.turn,
            State::Connecting { turn, .. } | State::Disconnecting { turn, .. } => turn,
            State::Idle => return,
        };
        if let Turn::Waiting(None) = turn {
            *turn = Turn::Waiting(Some(now + wait));
        }
    }

    // long enough for the peer to unkey, key up, and get its header through
    fn reply_wait(&self) -> u64 {
        let header_ms = (frame_samples(self.cfg.profile, &LEVELS[0], 0) * 1000 / FS) as u64;
        2 * self.cfg.txdelay_ms as u64 + header_ms + 1500
    }

    /// Advances timers. A new connect waits for a clear channel. Inside a link an answer goes
    /// as soon as the peer's frame has ended, and anything else waits for the audio to stop.
    pub fn tick(&mut self, now: u64, sense: Sense) {
        self.energy_since = sense.energy.then(|| self.energy_since.unwrap_or(now));
        let stale = self.energy_since.is_some_and(|t| now >= t + MAX_HOLD_MS);
        let (energy, busy) = if stale {
            (false, sense.receiving)
        } else {
            (sense.energy, sense.busy)
        };
        let (reply_hold, hold) = if matches!(self.state, State::Connecting { .. }) {
            (busy, busy)
        } else {
            (sense.receiving, sense.receiving || energy)
        };
        if let State::Connected(s) = &mut self.state {
            if matches!(s.turn, Turn::Idle) && now >= s.heard + IDLE_MS {
                s.closing = true;
                s.turn = Turn::Ours(now);
            }
        }
        let backoff = 200 + self.rng_next() % 1500;
        let retries = self.cfg.max_retries;
        let mut expired = false;
        let mut due = false;
        {
            let turn = match &mut self.state {
                State::Connected(s) => &mut s.turn,
                State::Connecting { turn, .. } | State::Disconnecting { turn, .. } => turn,
                State::Idle => return,
            };
            match turn {
                Turn::Waiting(Some(deadline)) => {
                    if hold {
                        *deadline = (*deadline).max(now + 500);
                    } else if now >= *deadline {
                        expired = true;
                    }
                }
                // stations held by the same busy channel would all key the moment it clears, so
                // each keeps drawing a random wait while held and the last draw counts
                Turn::Ours(at) if hold => *at = now + backoff,
                Turn::Ours(at) if now >= *at => due = true,
                Turn::Reply(at) if now >= *at && !reply_hold => due = true,
                _ => {}
            }
        }
        if expired {
            self.on_timeout(now, backoff, retries);
        } else if due {
            self.transmit(now);
        }
    }

    fn on_timeout(&mut self, now: u64, backoff: u64, max_retries: u32) {
        match &mut self.state {
            State::Connecting { tries, turn, .. } => {
                *tries += 1;
                if *tries >= CONNECT_TRIES {
                    self.drop_link();
                } else {
                    *turn = Turn::Ours(now + backoff);
                }
            }
            State::Disconnecting { tries, turn, .. } => {
                *tries += 1;
                if *tries >= 3 {
                    self.finish(0);
                } else {
                    *turn = Turn::Ours(now + backoff);
                }
            }
            State::Connected(s) => {
                s.retries += 1;
                let limit = if s.confirmed {
                    max_retries
                } else {
                    CONNECT_TRIES
                };
                if s.retries > limit {
                    self.finish(0);
                } else {
                    // nothing came back at all: drop a level before trying again
                    s.cap = s.cap.saturating_sub(1);
                    s.turn = Turn::Ours(now + backoff);
                }
            }
            State::Idle => {}
        }
    }

    // back to idle; a host never told CONNECTED hears CANCELPENDING first
    fn drop_link(&mut self) -> Option<String> {
        let (mycall, told) = match std::mem::replace(&mut self.state, State::Idle) {
            State::Idle => return None,
            State::Connecting { mycall, .. } => (mycall, false),
            State::Connected(s) => (s.mycall, s.confirmed),
            State::Disconnecting { mycall, .. } => (mycall, true),
        };
        self.last_buffer = 0;
        if !told {
            self.host(HostEvent::CancelPending);
        }
        self.host(HostEvent::Disconnected);
        Some(mycall)
    }

    fn finish(&mut self, cw_wait_ms: u64) {
        if let Some(call) = self.drop_link() {
            self.out.push_back(Out::CwId {
                call,
                wait_ms: cw_wait_ms,
            });
        }
    }

    fn transmit(&mut self, now: u64) {
        let profile = self.cfg.profile;
        let budget_for = |level: u8| (1.0 + 7.0 * level as f32 / 9.0) * FS as f32;
        match &mut self.state {
            State::Connecting {
                mycall,
                peer,
                turn,
                nonce,
                ..
            } => {
                let mut h = Header::new(Kind::Connect, profile, mycall, peer);
                h.body[0..4].copy_from_slice(&nonce.to_le_bytes());
                *turn = Turn::Waiting(None);
                self.out.push_back(Out::Transmit(vec![(h, Vec::new())]));
            }
            State::Disconnecting {
                mycall, peer, turn, ..
            } => {
                let h = Header::new(Kind::Disconnect, profile, mycall, peer);
                *turn = Turn::Waiting(None);
                self.out.push_back(Out::Transmit(vec![(h, Vec::new())]));
            }
            State::Connected(s) if !s.confirmed => {
                let mut h = Header::new(Kind::Accept, profile, &s.mycall, &s.peer);
                h.body = ack_body(s);
                s.turn = Turn::Waiting(None);
                self.out.push_back(Out::Transmit(vec![(h, Vec::new())]));
            }
            State::Connected(s) => {
                if s.closing && !s.tx.has_work() {
                    let (mycall, peer) = (s.mycall.clone(), s.peer.clone());
                    self.state = State::Disconnecting {
                        mycall,
                        peer,
                        tries: 0,
                        turn: Turn::Ours(now),
                    };
                    self.transmit(now);
                    return;
                }
                let level = s.level.min(s.cap);
                let lv = LEVELS[level as usize];
                let cap = Chunk::capacity(lv.info_bytes());
                let mut max = 0;
                while max < MAX_CHUNKS
                    && (frame_samples(profile, &lv, max + 1) as f32) <= budget_for(level)
                {
                    max += 1;
                }
                let chunks = s.tx.take(cap, max.max(1));
                s.frame_seq = s.frame_seq.wrapping_add(1);
                let mut h = Header::new(
                    if chunks.is_empty() {
                        Kind::Ack
                    } else {
                        Kind::Data
                    },
                    profile,
                    &s.mycall,
                    &s.peer,
                );
                h.seq = s.frame_seq;
                h.level = if chunks.is_empty() { 0 } else { level };
                h.body = ack_body(s);
                s.tx.frames.push_back((
                    s.frame_seq,
                    chunks
                        .iter()
                        .map(|c| (c.offset, c.offset + c.data.len() as u32))
                        .collect(),
                ));
                while s.tx.frames.len() > 8 {
                    s.tx.frames.pop_front();
                }
                s.owe_ack = false;
                // a frame carrying data needs an answer; a bare ACK does not
                s.turn = if chunks.is_empty() {
                    Turn::Idle
                } else {
                    Turn::Waiting(None)
                };
                let mut burst = Vec::new();
                if now >= s.last_id + ID_INTERVAL_MS {
                    s.last_id = now;
                    burst.push((Header::new(Kind::Id, profile, &s.mycall, "ID"), Vec::new()));
                }
                burst.push((h, chunks));
                self.out.push_back(Out::Transmit(burst));
            }
            State::Idle => {}
        }
    }

    /// Handles a decoded frame. Frames for other stations and UI frames are ignored.
    pub fn on_frame(&mut self, now: u64, f: &RxFrame) {
        let h = &f.header;
        let Some(me) = self
            .mycalls
            .iter()
            .find(|c| c.eq_ignore_ascii_case(&h.dst))
            .cloned()
        else {
            return;
        };
        match (&mut self.state, h.kind) {
            (State::Connecting { peer, crossed, .. }, Kind::Connect)
                if peer.eq_ignore_ascii_case(&h.src) =>
            {
                // both ends dialled at once; a listening station gives way if its call sorts
                // first, or once the other side has shown it will not
                if self.listen && (me.to_ascii_uppercase() < h.src.to_ascii_uppercase() || *crossed)
                {
                    self.drop_link();
                    self.on_frame(now, f);
                } else {
                    *crossed = true;
                }
            }
            (State::Idle, Kind::Connect) if self.listen && h.profile == self.cfg.profile => {
                self.host(HostEvent::Pending);
                self.open(now, me, h.src.clone(), false, f.snr_db);
                if let State::Connected(s) = &mut self.state {
                    s.want = best_level(f.snr_db);
                    s.nonce = u32::from_le_bytes(h.body[0..4].try_into().unwrap());
                }
                self.transmit(now);
            }
            (State::Connected(s), Kind::Connect) if s.peer.eq_ignore_ascii_case(&h.src) => {
                if !s.caller && u32::from_le_bytes(h.body[0..4].try_into().unwrap()) == s.nonce {
                    // our accept was lost; say it again
                    self.transmit(now);
                } else {
                    // the peer started over, so the old session is dead
                    self.drop_link();
                    self.on_frame(now, f);
                }
            }
            (State::Connected(s), Kind::Connect) if !s.peer.eq_ignore_ascii_case(&h.src) => {
                let reply = Header::new(Kind::Reject, self.cfg.profile, &me, &h.src);
                self.out.push_back(Out::Transmit(vec![(reply, Vec::new())]));
            }
            (State::Connecting { mycall, peer, .. }, Kind::Accept)
                if peer.eq_ignore_ascii_case(&h.src) =>
            {
                let (mycall, peer) = (mycall.clone(), peer.clone());
                self.open(now, mycall, peer, true, f.snr_db);
                if let State::Connected(s) = &mut self.state {
                    s.level = h.body[9].min(LEVELS.len() as u8 - 1);
                    s.want = best_level(f.snr_db);
                    s.owe_ack = true;
                    s.turn = Turn::Reply(now + TURNAROUND_MS);
                }
                self.report_rate();
            }
            (State::Connected(s), Kind::Accept)
                if s.caller && s.peer.eq_ignore_ascii_case(&h.src) =>
            {
                // our first frame never got there; the callee is still waiting for it
                s.heard = now;
                s.owe_ack = true;
                s.turn = Turn::Reply(now + TURNAROUND_MS);
            }
            (State::Connecting { peer, .. }, Kind::Reject) if peer.eq_ignore_ascii_case(&h.src) => {
                self.drop_link();
            }
            (State::Connected(s), Kind::Data | Kind::Ack)
                if s.peer.eq_ignore_ascii_case(&h.src) =>
            {
                self.on_data(now, f);
            }
            (_, Kind::Disconnect) => {
                let reply = Header::new(Kind::DisconnectAck, self.cfg.profile, &me, &h.src);
                self.out.push_back(Out::Transmit(vec![(reply, Vec::new())]));
                if let State::Connected(s) = &self.state {
                    if s.peer.eq_ignore_ascii_case(&h.src) {
                        self.finish(0);
                    }
                }
            }
            (State::Disconnecting { peer, .. }, Kind::DisconnectAck)
                if peer.eq_ignore_ascii_case(&h.src) =>
            {
                self.finish(CW_HOLDOFF_MS);
            }
            _ => {}
        }
    }

    fn open(&mut self, now: u64, mycall: String, peer: String, caller: bool, snr: f32) {
        self.state = State::Connected(Box::new(Session {
            mycall,
            peer,
            caller,
            tx: TxStream::new(),
            rx: RxStream::default(),
            frame_seq: 0,
            level: 0,
            cap: LEVELS.len() as u8 - 1,
            want: 0,
            clean: 0,
            snr,
            ack_seq: 0,
            ack_bits: 0,
            owe_ack: false,
            turn: Turn::Idle,
            retries: 0,
            closing: false,
            last_id: now,
            nonce: 0,
            heard: now,
            confirmed: caller,
        }));
        if caller {
            self.report_connected();
        }
    }

    // VARA reports the caller first
    fn report_connected(&mut self) {
        if let State::Connected(s) = &self.state {
            let (src, dst) = if s.caller {
                (s.mycall.clone(), s.peer.clone())
            } else {
                (s.peer.clone(), s.mycall.clone())
            };
            let snr = s.snr;
            self.host(HostEvent::Connected {
                src,
                dst,
                profile: self.cfg.profile,
            });
            self.host(HostEvent::Snr(snr));
        }
    }

    fn on_data(&mut self, now: u64, f: &RxFrame) {
        let State::Connected(s) = &mut self.state else {
            return;
        };
        let h = &f.header;
        s.retries = 0;
        s.snr = f.snr_db;
        s.heard = now;
        let first = !s.confirmed;
        s.confirmed = true;

        // the peer's view of our stream
        let body = &h.body;
        let rx_next = u32::from_le_bytes(body[0..4].try_into().unwrap());
        s.tx.advance(rx_next);
        if let Some(layout) =
            s.tx.frames
                .iter()
                .rev()
                .find(|(q, _)| *q == body[4])
                .map(|(_, l)| l.clone())
        {
            let bits = u64::from(u32::from_le_bytes(body[5..9].try_into().unwrap()))
                | (u64::from(body[10]) << 32);
            for (i, (start, end)) in layout.into_iter().enumerate() {
                if bits >> i & 1 == 1 {
                    s.tx.mark(start, end);
                }
            }
        }
        let peer_want = body[9];
        s.level = peer_want.min(LEVELS.len() as u8 - 1);
        s.cap = (s.cap + 1).min(LEVELS.len() as u8 - 1);

        // our view of their stream
        let mut delivered = Vec::new();
        let mut bits = 0u64;
        let mut lost = 0;
        for (i, c) in f.chunks.iter().enumerate() {
            match c {
                Some(c) => {
                    bits |= 1 << i;
                    delivered.extend(s.rx.accept(c));
                }
                None => lost += 1,
            }
        }
        if !f.chunks.is_empty() {
            s.ack_seq = h.seq;
            s.ack_bits = bits;
            s.owe_ack = true;
            adapt(s, h.level, lost, f.chunks.len(), f.snr_db);
        }

        let reply = s.owe_ack || s.tx.has_work() || s.closing;
        s.turn = if reply {
            Turn::Reply(now + TURNAROUND_MS)
        } else {
            Turn::Idle
        };

        if first {
            self.report_connected();
        }
        self.host(HostEvent::Snr(f.snr_db));
        if !delivered.is_empty() {
            self.host(HostEvent::Data(delivered));
        }
        self.report_buffer();
        self.report_rate();
    }

    fn report_buffer(&mut self) {
        let n = match &self.state {
            State::Connected(s) => s.tx.unacked(),
            _ => 0,
        };
        if n != self.last_buffer {
            self.last_buffer = n;
            self.host(HostEvent::Buffer(n));
        }
    }

    fn report_rate(&mut self) {
        if let State::Connected(s) = &self.state {
            let level = s.level.min(s.cap);
            let bps = LEVELS[level as usize].bps(self.cfg.profile) as u32;
            self.host(HostEvent::Bitrate { level, bps });
        }
    }

    fn host(&mut self, e: HostEvent) {
        self.out.push_back(Out::Host(e));
    }

    fn rng_next(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }
}

// ack layout in the header body: next expected offset, which frame the bitmap is for,
// a 40-bit bitmap of its good codewords, and the level we want the peer to use
fn ack_body(s: &Session) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[0..4].copy_from_slice(&s.rx.next.to_le_bytes());
    b[4] = s.ack_seq;
    b[5..9].copy_from_slice(&(s.ack_bits as u32).to_le_bytes());
    b[9] = s.want;
    b[10] = (s.ack_bits >> 32) as u8;
    b[11] = s.snr.clamp(-128.0, 127.0) as i8 as u8;
    b
}

// receiver side of rate adaptation: up one level after two clean frames with SNR to spare,
// down one on more than 20% loss
fn adapt(s: &mut Session, level: u8, lost: usize, count: usize, snr: f32) {
    let top = LEVELS.len() as u8 - 1;
    if lost * 5 > count {
        s.want = level.saturating_sub(1);
        s.clean = 0;
    } else if lost == 0
        && level < top
        && snr >= LEVELS[level as usize + 1].min_snr_db as f32 + SNR_MARGIN
    {
        s.clean += 1;
        if s.clean >= 2 {
            s.want = level + 1;
            s.clean = 0;
        } else {
            s.want = level;
        }
    } else {
        s.want = level;
        s.clean = 0;
    }
    s.want = s.want.min(best_level(snr).max(level.saturating_sub(1)));
}

#[cfg(test)]
#[path = "arq_tests.rs"]
mod tests;
