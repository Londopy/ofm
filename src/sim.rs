// two modems in one process joined by the channel simulator in real time, so a client such
// as Pat can be tried end to end without radios

use crate::station::Audio;
use ofm_core::chansim::Channel;
use ofm_core::modem::FS;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Air = Arc<Mutex<[VecDeque<f32>; 2]>>;

/// One end of the simulated radio link.
pub struct SimAudio {
    side: usize,
    air: Air,
    channel: Channel,
    start: Instant,
    taken: usize,
    busy_until: Instant,
}

/// Both ends; `a_to_b` shapes what the second modem hears from the first.
pub fn pair(a_to_b: Channel, b_to_a: Channel) -> (SimAudio, SimAudio) {
    let air: Air = Arc::new(Mutex::new([VecDeque::new(), VecDeque::new()]));
    let now = Instant::now();
    let end = |side, channel| SimAudio {
        side,
        air: air.clone(),
        channel,
        start: now,
        taken: 0,
        busy_until: now,
    };
    (end(0, a_to_b), end(1, b_to_a))
}

impl Audio for SimAudio {
    // as much audio as a sound card would have captured by now
    fn capture(&mut self) -> Vec<f32> {
        let due = (self.start.elapsed().as_secs_f64() * FS as f64) as usize;
        let n = due - self.taken;
        self.taken = due;
        let mut air = self.air.lock().unwrap();
        let q = &mut air[self.side];
        let mut out: Vec<f32> = q.drain(..q.len().min(n)).collect();
        out.resize(n, 0.0);
        out
    }

    fn play(&mut self, samples: Vec<f32>) {
        let from = self.busy_until.max(Instant::now());
        self.busy_until = from + Duration::from_secs_f64(samples.len() as f64 / FS as f64);
        let heard = self.channel.transmit(&samples);
        self.air.lock().unwrap()[1 - self.side].extend(heard);
    }

    fn playing(&self) -> bool {
        Instant::now() < self.busy_until
    }
}
