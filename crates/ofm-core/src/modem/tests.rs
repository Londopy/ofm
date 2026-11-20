use super::*;
use crate::chansim::{Channel, ChannelConfig};

fn payload(level: &Level, count: usize, salt: u8) -> Vec<Chunk> {
    let cap = Chunk::capacity(level.info_bytes());
    (0..count)
        .map(|i| Chunk {
            offset: (i * cap) as u32,
            data: (0..cap).map(|j| (i * 31 + j * 7) as u8 ^ salt).collect(),
        })
        .collect()
}

// silence either side of the frame, fed to the receiver in 10 ms blocks like a sound card
fn receive(profile: Profile, audio: &[f32]) -> Vec<RxEvent> {
    let mut rx = Receiver::new(profile);
    let mut padded = vec![0.0; 7_000];
    padded.extend_from_slice(audio);
    padded.extend(std::iter::repeat_n(0.0, 20_000));
    let mut events = Vec::new();
    for block in padded.chunks(480) {
        events.extend(rx.push(block));
    }
    events
}

fn frames(events: &[RxEvent]) -> Vec<&RxFrame> {
    events
        .iter()
        .filter_map(|e| match e {
            RxEvent::Frame(f) => Some(f),
            _ => None,
        })
        .collect()
}

fn run(profile: Profile, level: u8, count: usize, cfg: Option<ChannelConfig>) -> (usize, f32) {
    let mut h = Header::new(Kind::Data, profile, "N0CALL", "W1AW");
    h.level = level;
    h.seq = 7;
    let chunks = payload(&LEVELS[level as usize], count, level);
    let mut audio = lead_in(150);
    audio.extend(build_frame(&h, &chunks, 9.0));
    let audio = match cfg {
        Some(c) => Channel::new(c).transmit(&audio),
        None => audio,
    };
    let events = receive(profile, &audio);
    let got = frames(&events);
    assert_eq!(
        got.len(),
        1,
        "profile {profile:?} level {level}: {events:?}"
    );
    let f = got[0];
    assert_eq!(f.header.src, "N0CALL");
    assert_eq!(f.header.seq, 7);
    assert_eq!(f.header.count as usize, count);
    let good = f
        .chunks
        .iter()
        .zip(&chunks)
        .filter(|(r, t)| r.as_ref() == Some(t))
        .count();
    (good, f.snr_db)
}

#[test]
fn clean_loopback_every_level_both_profiles() {
    for profile in [Profile::Narrow, Profile::Wide] {
        for level in 0..LEVELS.len() as u8 {
            let (good, snr) = run(profile, level, 3, None);
            assert_eq!(good, 3, "{profile:?} level {level} snr {snr}");
        }
    }
}

#[test]
fn header_only_frame_decodes() {
    let h = Header::new(Kind::Ack, Profile::Narrow, "N0CALL", "W1AW");
    let events = receive(Profile::Narrow, &build_frame(&h, &[], 9.0));
    let got = frames(&events);
    assert_eq!(got.len(), 1);
    assert!(got[0].chunks.is_empty());
}

#[test]
fn back_to_back_frames_both_decode() {
    let p = Profile::Wide;
    let mut audio = build_frame(&Header::new(Kind::Id, p, "N0CALL", "ID"), &[], 9.0);
    let mut h = Header::new(Kind::Data, p, "N0CALL", "W1AW");
    h.level = 4;
    audio.extend(build_frame(&h, &payload(&LEVELS[4], 2, 1), 9.0));
    let events = receive(p, &audio);
    let got = frames(&events);
    assert_eq!(got.len(), 2, "{events:?}");
    assert_eq!(got[0].header.kind, Kind::Id);
    assert_eq!(got[1].chunks.iter().flatten().count(), 2);
}

#[test]
fn narrow_radio_path_with_clock_offset() {
    let mut cfg = ChannelConfig::narrow();
    cfg.snr_db = Some(30.0);
    cfg.clock_ppm = 150.0;
    cfg.keyup_ms = 80.0;
    let (good, snr) = run(Profile::Narrow, 6, 10, Some(cfg));
    assert_eq!(good, 10, "snr {snr}");
}

#[test]
fn wide_radio_path_with_negative_clock_offset() {
    let mut cfg = ChannelConfig::wide();
    cfg.snr_db = Some(30.0);
    cfg.clock_ppm = -200.0;
    let (good, snr) = run(Profile::Wide, 6, 20, Some(cfg));
    assert_eq!(good, 20, "snr {snr}");
}

#[test]
fn floor_level_survives_low_snr() {
    let mut cfg = ChannelConfig::narrow();
    cfg.snr_db = Some(4.0);
    let (good, snr) = run(Profile::Narrow, 0, 2, Some(cfg));
    assert_eq!(good, 2, "snr {snr}");
}

#[test]
fn snr_estimate_tracks_channel() {
    let mut cfg = ChannelConfig::wide();
    cfg.snr_db = Some(15.0);
    let (_, snr) = run(Profile::Wide, 2, 4, Some(cfg));
    assert!((10.0..20.0).contains(&snr), "estimated {snr}");
}

#[test]
fn noise_alone_is_not_a_frame() {
    let mut x = 1u32;
    let noise: Vec<f32> = (0..200_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x as f32 / u32::MAX as f32 - 0.5) * 0.3
        })
        .collect();
    assert!(frames(&receive(Profile::Narrow, &noise)).is_empty());
}

#[test]
fn lead_in_tones_are_not_a_preamble() {
    assert!(frames(&receive(Profile::Narrow, &lead_in(500))).is_empty());
}

#[test]
fn low_levels_get_through_full_band_noise() {
    let mut cfg = ChannelConfig::narrow();
    cfg.snr_db = Some(3.0);
    for level in [0, 1] {
        let (good, snr) = run(Profile::Narrow, level, 2, Some(cfg.clone()));
        assert_eq!(good, 2, "level {level} snr {snr}");
    }
}

#[test]
fn frames_stay_well_below_full_scale() {
    for profile in [Profile::Narrow, Profile::Wide] {
        for level in [0u8, 5, 9] {
            let mut h = Header::new(Kind::Data, profile, "N0CALL", "W1AW");
            h.level = level;
            let audio = build_frame(&h, &payload(&LEVELS[level as usize], 8, 3), 9.0);
            let peak = audio.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            assert!(peak < 0.8, "{profile:?} level {level} peak {peak}");
        }
    }
}

#[test]
fn repetitive_data_survives_peak_limiting() {
    // all-zero chunks would put every carrier in phase without the scrambler
    let p = Profile::Wide;
    let lv = LEVELS[9];
    let cap = Chunk::capacity(lv.info_bytes());
    let chunks: Vec<Chunk> = (0..6)
        .map(|i| Chunk {
            offset: (i * cap) as u32,
            data: vec![0; cap],
        })
        .collect();
    let mut h = Header::new(Kind::Data, p, "N0CALL", "W1AW");
    h.level = 9;
    let events = receive(p, &build_frame(&h, &chunks, 9.0));
    let got = frames(&events);
    assert_eq!(got.len(), 1);
    assert_eq!(
        got[0].chunks.iter().flatten().count(),
        6,
        "snr {}",
        got[0].snr_db
    );
    assert!(got[0].snr_db > 25.0, "snr {}", got[0].snr_db);
}

struct Splitmix(u64);

impl Splitmix {
    fn noise(&mut self) -> f32 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
}

// a 60 ms beep at a multiple of the carrier spacing, in a little channel noise
fn beep(seed: u64) -> Vec<f32> {
    let mut rng = Splitmix(seed);
    let mut audio: Vec<f32> = (0..9600).map(|_| rng.noise() * 0.01).collect();
    audio.extend((0..48 * 60).map(|i| {
        0.15 * (std::f32::consts::TAU * 1000.0 * i as f32 / 48000.0).sin() + rng.noise() * 0.01
    }));
    audio.extend((0..30000).map(|_| rng.noise() * 0.01));
    audio
}

#[test]
fn a_beep_is_not_a_frame_and_does_not_crash_the_receiver() {
    // these seeds made the receiver slice past the end of its buffer
    for seed in [1423, 4304] {
        let mut rx = Receiver::new(Profile::Narrow);
        let mut events = Vec::new();
        for block in beep(seed).chunks(480) {
            events.extend(rx.push(block));
        }
        assert!(events.is_empty(), "seed {seed}: {events:?}");
    }
}

#[test]
fn a_long_tone_is_ignored_and_a_frame_after_it_decodes() {
    let p = Profile::Narrow;
    let mut h = Header::new(Kind::Data, p, "N0CALL", "W1AW");
    h.level = 3;
    let chunks = payload(&LEVELS[3], 3, 0);
    let mut audio: Vec<f32> = (0..48000 * 8)
        .map(|i| 0.1 * (std::f32::consts::TAU * 1000.0 * i as f32 / 48000.0).sin())
        .collect();
    audio.extend(std::iter::repeat_n(0.0, 9600));
    audio.extend(build_frame(&h, &chunks, 9.0));
    audio.extend(std::iter::repeat_n(0.0, 48000));
    let mut rx = Receiver::new(p);
    let mut events = Vec::new();
    for block in audio.chunks(480) {
        events.extend(rx.push(block));
    }
    let got = frames(&events);
    assert_eq!(got.len(), 1, "{events:?}");
    assert_eq!(got[0].chunks.iter().flatten().count(), 3);
    assert_eq!(
        events.len(),
        1,
        "the tone caused a false detection: {events:?}"
    );
}

#[test]
fn wide_frames_get_through_a_de_emphasised_path() {
    let mut cfg = ChannelConfig::wide();
    cfg.snr_db = Some(25.0);
    cfg.tilt_db_per_octave = -5.0;
    cfg.tilt_corner_hz = 300.0;
    for seed in 1..=4 {
        cfg.seed = seed;
        let (good, snr) = run(Profile::Wide, 2, 4, Some(cfg.clone()));
        assert_eq!(good, 4, "seed {seed}, snr {snr}");
    }
}

#[test]
fn hiss_after_digital_silence_stops_counting_as_a_signal() {
    let mut rx = Receiver::new(Profile::Narrow);
    rx.push(&vec![0.0; FS / 2]);
    let mut rng = Splitmix(5);
    // -50 dBFS of steady hiss for 40 s: the floor has to climb up to it
    for _ in 0..FS * 40 / 480 {
        let block: Vec<f32> = (0..480).map(|_| rng.noise() * 0.0055).collect();
        rx.push(&block);
    }
    assert!(!rx.energy());
}
