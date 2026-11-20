use super::*;
use crate::modem::PREAMBLE;

struct Side {
    link: Link,
    got: Vec<u8>,
    events: Vec<HostEvent>,
    // wait_ms of each CW ID asked for
    cw: Vec<u64>,
    keyed_until: Option<u64>,
}

// two links over a half-duplex channel with frame timing taken from the real waveform
struct Air {
    now: u64,
    sides: [Side; 2],
    flying: Vec<(u64, usize, RxFrame)>,
    // audio on the air: who hears it, when it starts, when the first preamble ends, when it stops
    bursts: Vec<(usize, u64, Option<u64>, u64)>,
    // frames that arrived while their receiver was transmitting
    collided: usize,
    // a third station both sides hear but cannot decode, until this time
    qrm_until: u64,
    rng: u64,
    chunk_loss: u64,
    frame_loss: u64,
    // everything this side sends is lost
    unheard: Option<usize>,
    snr: f32,
}

const CFG: LinkConfig = LinkConfig {
    profile: Profile::Wide,
    txdelay_ms: 150,
    max_retries: 10,
};

impl Air {
    fn new() -> Air {
        let side = |call: &str, seed| {
            let mut link = Link::new(CFG, seed);
            link.set_mycalls(vec![call.to_string()]);
            Side {
                link,
                got: Vec::new(),
                events: Vec::new(),
                cw: Vec::new(),
                keyed_until: None,
            }
        };
        Air {
            now: 0,
            sides: [side("N0CALL", 11), side("W1AW-10", 23)],
            flying: Vec::new(),
            bursts: Vec::new(),
            collided: 0,
            qrm_until: 0,
            rng: 99,
            chunk_loss: 0,
            frame_loss: 0,
            unheard: None,
            snr: 30.0,
        }
    }

    fn roll(&mut self, percent: u64) -> bool {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng % 100 < percent
    }

    fn step(&mut self) {
        self.now += 10;
        let now = self.now;
        let (arrived, flying): (Vec<_>, Vec<_>) =
            self.flying.drain(..).partition(|(at, _, _)| *at <= now);
        self.flying = flying;
        for (_, to, f) in arrived {
            if self.sides[to].keyed_until.is_none() {
                self.sides[to].link.on_frame(now, &f);
            } else {
                self.collided += 1;
            }
        }
        self.bursts.retain(|b| b.3 > now);
        for i in 0..2 {
            if let Some(t) = self.sides[i].keyed_until {
                if now >= t {
                    self.sides[i].keyed_until = None;
                    self.sides[i].link.tx_done(now);
                }
            }
            let keyed = self.sides[i].keyed_until.is_some();
            // energy shows up just after the far end keys; decoding starts after its preamble
            let energy = keyed
                || now < self.qrm_until
                || self.bursts.iter().any(|b| b.0 == i && now >= b.1 + 30);
            let receiving = keyed
                || self
                    .bursts
                    .iter()
                    .any(|b| b.0 == i && b.2.is_some_and(|t| now >= t));
            let sense = Sense {
                receiving,
                energy,
                busy: energy || receiving,
            };
            self.sides[i].link.tick(now, sense);
            while let Some(out) = self.sides[i].link.poll() {
                match out {
                    Out::Transmit(frames) => self.send(i, frames),
                    Out::Host(HostEvent::Data(d)) => self.sides[i].got.extend(d),
                    Out::Host(e) => self.sides[i].events.push(e),
                    Out::CwId { wait_ms, .. } => self.sides[i].cw.push(wait_ms),
                }
            }
        }
    }

    fn send(&mut self, from: usize, frames: Vec<(Header, Vec<Chunk>)>) {
        let start = self.now;
        let mut at = start + CFG.txdelay_ms as u64;
        let preamble_end = at + (PREAMBLE * 1000 / FS) as u64 + 15;
        let lost_whole = self.roll(self.frame_loss) || self.unheard == Some(from);
        for (h, chunks) in frames {
            let level = LEVELS[h.level as usize];
            at += (frame_samples(CFG.profile, &level, chunks.len()) * 1000 / FS) as u64;
            let loss = self.chunk_loss;
            let chunks = chunks
                .into_iter()
                .map(|c| if self.roll(loss) { None } else { Some(c) })
                .collect();
            if !lost_whole {
                self.flying.push((
                    at,
                    1 - from,
                    RxFrame {
                        header: h,
                        chunks,
                        snr_db: self.snr,
                    },
                ));
            }
        }
        self.sides[from].keyed_until = Some(at);
        self.bursts
            .push((1 - from, start, (!lost_whole).then_some(preamble_end), at));
    }

    fn run_until(&mut self, limit_s: u64, done: impl Fn(&Air) -> bool) {
        while !done(self) {
            assert!(self.now < limit_s * 1000, "timed out at {} ms", self.now);
            self.step();
        }
    }
}

fn data(n: usize, salt: u8) -> Vec<u8> {
    (0..n).map(|i| (i * 13 % 251) as u8 ^ salt).collect()
}

fn connected(air: &mut Air) {
    air.sides[1].link.set_listen(true);
    air.sides[0].link.connect(0, "N0CALL", "W1AW-10");
    air.run_until(30, |a| {
        a.sides.iter().all(|s| {
            s.events
                .iter()
                .any(|e| matches!(e, HostEvent::Connected { .. }))
        })
    });
}

fn transfer(air: &mut Air, a_to_b: usize, b_to_a: usize) {
    let (da, db) = (data(a_to_b, 1), data(b_to_a, 2));
    let now = air.now;
    air.sides[0].link.send(now, &da);
    air.sides[1].link.send(now, &db);
    air.run_until(900, |a| {
        a.sides[1].got.len() >= da.len() && a.sides[0].got.len() >= db.len()
    });
    assert_eq!(air.sides[1].got, da);
    assert_eq!(air.sides[0].got, db);
}

fn last_buffer(side: &Side) -> Option<usize> {
    side.events.iter().rev().find_map(|e| {
        if let HostEvent::Buffer(n) = e {
            Some(*n)
        } else {
            None
        }
    })
}

fn levels(side: &Side) -> Vec<u8> {
    side.events
        .iter()
        .filter_map(|e| {
            if let HostEvent::Bitrate { level, .. } = e {
                Some(*level)
            } else {
                None
            }
        })
        .collect()
}

#[test]
fn connect_reports_caller_first_on_both_sides() {
    let mut air = Air::new();
    connected(&mut air);
    let want = HostEvent::Connected {
        src: "N0CALL".into(),
        dst: "W1AW-10".into(),
        profile: Profile::Wide,
    };
    assert!(air.sides[0].events.contains(&want));
    assert!(air.sides[1].events.contains(&want));
    assert_eq!(air.sides[1].events[0], HostEvent::Pending);
}

#[test]
fn transfers_both_ways_then_disconnects() {
    let mut air = Air::new();
    connected(&mut air);
    transfer(&mut air, 20_000, 3_000);
    air.run_until(900, |a| last_buffer(&a.sides[0]) == Some(0));
    let now = air.now;
    air.sides[0].link.disconnect(now);
    air.run_until(1000, |a| {
        a.sides
            .iter()
            .all(|s| s.events.last() == Some(&HostEvent::Disconnected))
    });
    assert!(!air.sides[0].link.is_connected() && !air.sides[1].link.is_connected());
}

#[test]
fn lossy_channel_still_delivers_exact_bytes() {
    let mut air = Air::new();
    connected(&mut air);
    air.chunk_loss = 25;
    air.frame_loss = 15;
    transfer(&mut air, 15_000, 8_000);
}

#[test]
fn climbs_levels_on_a_good_channel() {
    let mut air = Air::new();
    connected(&mut air);
    transfer(&mut air, 60_000, 0);
    let top = levels(&air.sides[0]).into_iter().max().unwrap();
    assert!(top >= 8, "only reached level {top}");
}

#[test]
fn stays_low_when_snr_is_poor() {
    let mut air = Air::new();
    air.snr = 8.0;
    connected(&mut air);
    transfer(&mut air, 3_000, 0);
    let top = levels(&air.sides[0]).into_iter().max().unwrap();
    assert!(top <= 3, "went up to level {top}");
}

#[test]
fn nobody_home_gives_up_with_disconnected() {
    let mut air = Air::new();
    air.sides[0].link.connect(0, "N0CALL", "W1AW-10");
    air.run_until(120, |a| {
        a.sides[0].events.last() == Some(&HostEvent::Disconnected)
    });
    assert_eq!(air.sides[0].events[0], HostEvent::Pending);
    assert!(air.sides[0].events.contains(&HostEvent::CancelPending));
    assert!(air.sides[1].events.is_empty());
}

#[test]
fn link_lost_mid_transfer_disconnects() {
    let mut air = Air::new();
    connected(&mut air);
    air.frame_loss = 100;
    let now = air.now;
    air.sides[0].link.send(now, &data(5000, 3));
    air.run_until(900, |a| {
        a.sides[0].events.last() == Some(&HostEvent::Disconnected)
    });
}

#[test]
fn listen_change_mid_link_drops_it() {
    let mut air = Air::new();
    connected(&mut air);
    air.sides[1].link.set_listen(false);
    air.step();
    assert_eq!(air.sides[1].events.last(), Some(&HostEvent::Disconnected));
}

#[test]
fn buffer_counts_unacked_bytes() {
    let mut air = Air::new();
    connected(&mut air);
    let now = air.now;
    air.sides[0].link.send(now, &data(1000, 0));
    air.step();
    assert!(air.sides[0].events.contains(&HostEvent::Buffer(1000)));
    air.run_until(120, |a| last_buffer(&a.sides[0]) == Some(0));
}

#[test]
fn a_second_caller_is_rejected() {
    let mut air = Air::new();
    connected(&mut air);
    let mut third = Link::new(CFG, 7);
    third.set_mycalls(vec!["K1ABC".into()]);
    third.connect(air.now, "K1ABC", "W1AW-10");
    third.tick(air.now, Sense::default());
    let mut connect = None;
    while let Some(out) = third.poll() {
        if let Out::Transmit(mut f) = out {
            connect = f.pop();
        }
    }
    let (header, _) = connect.expect("connect frame");
    let now = air.now;
    air.sides[1].link.on_frame(
        now,
        &RxFrame {
            header,
            chunks: Vec::new(),
            snr_db: 20.0,
        },
    );
    let mut reject = None;
    while let Some(out) = air.sides[1].link.poll() {
        if let Out::Transmit(f) = out {
            reject = Some(f[0].0.clone());
        }
    }
    let reject = reject.expect("reply to second caller");
    assert_eq!(reject.kind, Kind::Reject);
    assert_eq!(reject.dst, "K1ABC");
    third.on_frame(
        now,
        &RxFrame {
            header: reject,
            chunks: Vec::new(),
            snr_db: 20.0,
        },
    );
    let events: Vec<Out> = std::iter::from_fn(|| third.poll()).collect();
    assert!(events.contains(&Out::Host(HostEvent::Disconnected)));
    assert!(air.sides[1].link.is_connected());
}

#[test]
fn a_caller_that_aborts_and_dials_again_gets_a_clean_link() {
    let mut air = Air::new();
    connected(&mut air);
    transfer(&mut air, 3_000, 3_000);
    air.run_until(2_000, |a| last_buffer(&a.sides[0]) == Some(0));
    // the callee never hears the abort's DISCONNECT and still thinks the old link is up
    air.unheard = Some(0);
    air.sides[0].link.abort();
    air.step();
    air.unheard = None;
    assert!(air.sides[1].link.is_connected());
    let now = air.now;
    air.sides[0].link.connect(now, "N0CALL", "W1AW-10");
    air.run_until(2_000, |a| {
        a.sides[0]
            .events
            .iter()
            .filter(|e| matches!(e, HostEvent::Connected { .. }))
            .count()
            == 2
    });
    air.sides[0].got.clear();
    air.sides[1].got.clear();
    transfer(&mut air, 1_500, 1_500);
}

#[test]
fn selective_acks_keep_working_after_the_sequence_number_wraps() {
    let mut air = Air::new();
    connected(&mut air);
    for i in 0..300 {
        let now = air.now;
        air.sides[0].link.send(now, &data(40, i as u8));
        air.run_until(
            100_000,
            |a| matches!(&a.sides[0].link.state, State::Connected(s) if s.tx.unacked() == 0),
        );
    }
    let State::Connected(s) = &air.sides[0].link.state else {
        panic!("link dropped");
    };
    assert!(s.tx.frames.iter().any(|(q, _)| *q == s.frame_seq));
}

#[test]
fn a_longer_chunk_at_a_held_offset_is_not_dropped() {
    let mut rx = RxStream::default();
    let src: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
    let chunk = |a: usize, b: usize| Chunk {
        offset: a as u32,
        data: src[a..b].to_vec(),
    };
    assert!(rx.accept(&chunk(100, 150)).is_empty());
    assert!(rx.accept(&chunk(100, 300)).is_empty());
    assert_eq!(rx.accept(&chunk(0, 100)), src);
}

fn disconnected(side: &Side) -> bool {
    side.events.last() == Some(&HostEvent::Disconnected)
}

#[test]
fn the_side_that_answers_a_disconnect_ids_first() {
    let mut air = Air::new();
    connected(&mut air);
    let now = air.now;
    air.sides[0].link.disconnect(now);
    air.run_until(60, |a| a.sides.iter().all(disconnected));
    assert_eq!(air.sides[0].cw, vec![CW_HOLDOFF_MS]);
    assert_eq!(air.sides[1].cw, vec![0]);
}

#[test]
fn an_abort_tells_the_peer() {
    let mut air = Air::new();
    connected(&mut air);
    air.sides[0].link.abort();
    let start = air.now;
    air.run_until(60, |a| disconnected(&a.sides[1]));
    assert!(air.now - start < 5_000, "took {} ms", air.now - start);
}

#[test]
fn a_callee_whose_accept_never_arrives_gives_up() {
    let mut air = Air::new();
    air.unheard = Some(1);
    air.sides[1].link.set_listen(true);
    air.sides[0].link.connect(0, "N0CALL", "W1AW-10");
    air.run_until(120, |a| a.sides.iter().all(disconnected));
    let callee = &air.sides[1].events;
    assert_eq!(callee[0], HostEvent::Pending);
    assert!(callee.contains(&HostEvent::CancelPending));
    assert!(!callee
        .iter()
        .any(|e| matches!(e, HostEvent::Connected { .. })));
    assert!(!air.sides[1].link.is_connected());
}

#[test]
fn an_idle_link_is_closed() {
    let mut air = Air::new();
    connected(&mut air);
    let start = air.now;
    air.run_until(IDLE_MS / 1000 + 120, |a| a.sides.iter().all(disconnected));
    assert!(air.now - start >= IDLE_MS);
}

fn idle(side: &Side) -> bool {
    matches!(&side.link.state, State::Connected(s) if matches!(s.turn, Turn::Idle))
}

#[test]
fn both_sides_sending_at_once_rarely_collide() {
    let mut air = Air::new();
    connected(&mut air);
    for round in 0..20u8 {
        air.run_until(900, |a| a.sides.iter().all(idle));
        air.sides[0].got.clear();
        air.sides[1].got.clear();
        transfer(&mut air, 600, 600 + round as usize);
    }
    assert!(
        air.collided <= 6,
        "{} frames hit a keyed receiver",
        air.collided
    );
}

#[test]
fn stations_held_by_the_same_signal_do_not_key_together_when_it_ends() {
    let mut air = Air::new();
    connected(&mut air);
    for round in 0..20u8 {
        air.run_until(900, |a| a.sides.iter().all(idle));
        air.qrm_until = air.now + 1500;
        air.sides[0].got.clear();
        air.sides[1].got.clear();
        transfer(&mut air, 600, 600 + round as usize);
    }
    assert!(
        air.collided <= 6,
        "{} frames hit a keyed receiver",
        air.collided
    );
}

#[test]
fn a_caller_whose_first_frame_is_lost_still_gets_connected() {
    let mut air = Air::new();
    air.sides[1].link.set_listen(true);
    air.sides[0].link.connect(0, "N0CALL", "W1AW-10");
    air.run_until(30, |a| {
        a.sides[0]
            .events
            .iter()
            .any(|e| matches!(e, HostEvent::Connected { .. }))
    });
    // the caller's first Ack goes nowhere
    air.unheard = Some(0);
    air.run_until(30, |a| a.sides[0].keyed_until.is_some());
    air.run_until(30, |a| a.sides[0].keyed_until.is_none());
    air.unheard = None;
    air.run_until(60, |a| {
        a.sides[1]
            .events
            .iter()
            .any(|e| matches!(e, HostEvent::Connected { .. }))
    });
    for side in &air.sides {
        let n = side
            .events
            .iter()
            .filter(|e| matches!(e, HostEvent::Connected { .. }))
            .count();
        assert_eq!(n, 1, "{:?}", side.events);
    }
}

#[test]
fn the_called_station_can_dial_back_after_losing_the_link() {
    let mut air = Air::new();
    air.sides[0].link.set_listen(true);
    connected(&mut air);
    // the callee restarts without a word and calls the old caller
    air.sides[1].link = Link::new(CFG, 31);
    air.sides[1].link.set_mycalls(vec!["W1AW-10".into()]);
    let now = air.now;
    air.sides[1].link.connect(now, "W1AW-10", "N0CALL");
    let want = HostEvent::Connected {
        src: "W1AW-10".into(),
        dst: "N0CALL".into(),
        profile: Profile::Wide,
    };
    air.run_until(60, |a| a.sides.iter().all(|s| s.events.contains(&want)));
    assert!(air.sides[0].events.contains(&HostEvent::Disconnected));
}

#[test]
fn a_dial_given_up_after_the_peer_accepted_tells_it() {
    let mut air = Air::new();
    air.sides[1].link.set_listen(true);
    air.sides[0].link.connect(0, "N0CALL", "W1AW-10");
    air.run_until(30, |a| a.sides[1].events.contains(&HostEvent::Pending));
    let now = air.now;
    air.sides[0].link.disconnect(now);
    let start = air.now;
    air.run_until(60, |a| !a.sides[1].link.is_connected());
    assert!(air.now - start < 5_000, "took {} ms", air.now - start);
}

#[test]
fn endless_audio_on_the_channel_does_not_freeze_a_link() {
    let mut air = Air::new();
    connected(&mut air);
    air.qrm_until = u64::MAX;
    air.sides[0].got.clear();
    air.sides[1].got.clear();
    transfer(&mut air, 600, 0);
}

fn crossed_dial(listen: [bool; 2]) -> Air {
    let mut air = Air::new();
    for (side, on) in air.sides.iter_mut().zip(listen) {
        side.link.set_listen(on);
    }
    air.sides[0].link.connect(0, "N0CALL", "W1AW-10");
    air.sides[1].link.connect(0, "W1AW-10", "N0CALL");
    air.run_until(120, |a| {
        a.sides.iter().all(|s| {
            s.events
                .iter()
                .any(|e| matches!(e, HostEvent::Connected { .. }))
        })
    });
    air
}

#[test]
fn two_stations_dialling_each_other_end_up_in_one_link() {
    for listen in [[true, true], [false, true], [true, false]] {
        let air = crossed_dial(listen);
        let links: Vec<_> = air
            .sides
            .iter()
            .map(|s| {
                s.events
                    .iter()
                    .filter(|e| matches!(e, HostEvent::Connected { .. }))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(links[0].len(), 1, "{listen:?} {links:?}");
        assert_eq!(links[0], links[1], "{listen:?}");
    }
}

fn connect_from(src: &str, dst: &str) -> RxFrame {
    let mut header = Header::new(Kind::Connect, Profile::Wide, src, dst);
    header.body[0..4].copy_from_slice(&7u32.to_le_bytes());
    RxFrame {
        header,
        chunks: Vec::new(),
        snr_db: 20.0,
    }
}

fn dialling(call: &str, peer: &str, listen: bool) -> Link {
    let mut link = Link::new(CFG, 3);
    link.set_mycalls(vec![call.to_string()]);
    link.set_listen(listen);
    link.connect(0, call, peer);
    std::iter::from_fn(|| link.poll()).for_each(drop);
    link
}

fn drained(link: &mut Link) -> Vec<Out> {
    std::iter::from_fn(|| link.poll()).collect()
}

fn accepts(out: &[Out]) -> bool {
    out.iter()
        .any(|o| matches!(o, Out::Transmit(f) if f[0].0.kind == Kind::Accept))
}

#[test]
fn a_listening_station_that_sorts_first_answers_the_other_dialler_at_once() {
    let mut link = dialling("N0CALL", "W1AW-10", true);
    link.on_frame(0, &connect_from("W1AW-10", "N0CALL"));
    let out = drained(&mut link);
    assert_eq!(
        out[..3],
        [
            Out::Host(HostEvent::CancelPending),
            Out::Host(HostEvent::Disconnected),
            Out::Host(HostEvent::Pending),
        ]
    );
    assert!(accepts(&out), "{out:?}");
}

#[test]
fn a_listening_station_that_sorts_last_waits_for_a_second_connect() {
    let mut link = dialling("W1AW-10", "N0CALL", true);
    link.on_frame(0, &connect_from("N0CALL", "W1AW-10"));
    assert!(drained(&mut link).is_empty());
    link.on_frame(10, &connect_from("N0CALL", "W1AW-10"));
    assert!(accepts(&drained(&mut link)));
}

#[test]
fn a_station_that_is_not_listening_keeps_dialling() {
    let mut link = dialling("N0CALL", "W1AW-10", false);
    for t in 0..3 {
        link.on_frame(t, &connect_from("W1AW-10", "N0CALL"));
    }
    assert!(drained(&mut link).is_empty());
    assert!(matches!(link.state, State::Connecting { .. }));
}

#[test]
fn the_order_rule_ignores_the_case_a_call_was_registered_in() {
    // frames carry upper case calls, so a call registered in lower case still has to sort the
    // way the other station sees it
    let mut link = dialling("n0call", "W1AW-10", true);
    link.on_frame(0, &connect_from("W1AW-10", "n0call"));
    assert!(accepts(&drained(&mut link)));
}
