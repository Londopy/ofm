// sound card i/o through cpal (WASAPI on Windows). the modem runs at 48 kHz; devices that
// insist on another rate are resampled linearly, which is plenty below 5 kHz of audio.

use crate::station::Audio;
use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, SizedSample, Stream, StreamConfig};
use ofm_core::modem::FS;
use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// device buffers keep playing for a moment after our queue empties
const DRAIN_GRACE: Duration = Duration::from_millis(60);

struct Resampler {
    step: f64,
    pos: f64,
    prev: f32,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Resampler {
        Resampler {
            step: from as f64 / to as f64,
            pos: 0.0,
            prev: 0.0,
        }
    }

    fn run(&mut self, input: &[f32]) -> Vec<f32> {
        if self.step == 1.0 {
            return input.to_vec();
        }
        let mut out = Vec::with_capacity((input.len() as f64 / self.step) as usize + 2);
        // pos is measured from the sample before input[0], which is self.prev
        while self.pos < input.len() as f64 {
            let i = self.pos.floor() as usize;
            let frac = (self.pos - i as f64) as f32;
            let a = if i == 0 { self.prev } else { input[i - 1] };
            let b = input[i];
            out.push(a + (b - a) * frac);
            self.pos += self.step;
        }
        self.pos -= input.len() as f64;
        if let Some(&last) = input.last() {
            self.prev = last;
        }
        out
    }
}

#[derive(Default)]
struct Playback {
    queue: VecDeque<f32>,
    drained_at: Option<Instant>,
}

pub struct SoundCard {
    _input: Stream,
    _output: Stream,
    captured: Receiver<Vec<f32>>,
    in_resample: Resampler,
    out_rate: u32,
    playback: Arc<Mutex<Playback>>,
}

fn find(devices: impl Iterator<Item = Device>, want: &str) -> Option<Device> {
    let want = want.to_ascii_lowercase();
    devices.into_iter().find(|d| {
        d.description()
            .is_ok_and(|desc| desc.name().to_ascii_lowercase().contains(&want))
    })
}

fn device_name(d: &Device) -> String {
    d.description()
        .map(|desc| desc.name().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

/// Prints input and output devices for `ofm devices`.
pub fn list() -> Result<()> {
    let host = cpal::default_host();
    let default_in = host.default_input_device().map(|d| device_name(&d));
    let default_out = host.default_output_device().map(|d| device_name(&d));
    println!("input devices:");
    for d in host.input_devices()? {
        let name = device_name(&d);
        let mark = if Some(&name) == default_in.as_ref() {
            " (default)"
        } else {
            ""
        };
        println!("  {name}{mark}");
    }
    println!("output devices:");
    for d in host.output_devices()? {
        let name = device_name(&d);
        let mark = if Some(&name) == default_out.as_ref() {
            " (default)"
        } else {
            ""
        };
        println!("  {name}{mark}");
    }
    Ok(())
}

fn input_stream<T>(
    device: &Device,
    config: StreamConfig,
    tx: std::sync::mpsc::Sender<Vec<f32>>,
) -> Result<Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            // first channel only; a radio interface is mono
            let mono: Vec<f32> = data
                .chunks(channels)
                .map(|f| f[0].to_sample::<f32>())
                .collect();
            let _ = tx.send(mono);
        },
        |e| eprintln!("audio input: {e}"),
        None,
    )?;
    Ok(stream)
}

fn output_stream<T>(
    device: &Device,
    config: StreamConfig,
    playback: Arc<Mutex<Playback>>,
) -> Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let mut p = playback.lock().unwrap();
            let had = !p.queue.is_empty();
            for frame in data.chunks_mut(channels) {
                let v = p.queue.pop_front().unwrap_or(0.0);
                frame.fill(T::from_sample(v));
            }
            if had && p.queue.is_empty() {
                p.drained_at = Some(Instant::now());
            }
        },
        |e| eprintln!("audio output: {e}"),
        None,
    )?;
    Ok(stream)
}

fn config_for(supported: cpal::SupportedStreamConfig) -> (StreamConfig, cpal::SampleFormat) {
    (supported.config(), supported.sample_format())
}

// 48 kHz in a sample format we handle, best format first; else the device's own default
fn pick(
    configs: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    default: impl FnOnce() -> Result<cpal::SupportedStreamConfig, cpal::Error>,
) -> Result<cpal::SupportedStreamConfig> {
    let rank = |f: cpal::SampleFormat| match f {
        cpal::SampleFormat::F32 => Some(0),
        cpal::SampleFormat::I16 => Some(1),
        cpal::SampleFormat::I32 => Some(2),
        _ => None,
    };
    let best = configs
        .filter_map(|c| c.try_with_sample_rate(FS as u32))
        .filter_map(|c| rank(c.sample_format()).map(|r| (r, c)))
        .min_by_key(|(r, _)| *r)
        .map(|(_, c)| c);
    match best {
        Some(c) => Ok(c),
        None => Ok(default()?),
    }
}

impl SoundCard {
    /// Opens the named devices (substring match), or the system defaults.
    pub fn open(input: Option<&str>, output: Option<&str>) -> Result<SoundCard> {
        let host = cpal::default_host();
        let in_dev = match input {
            Some(n) => find(host.input_devices()?, n)
                .ok_or_else(|| anyhow!("no input device matching {n:?}"))?,
            None => host
                .default_input_device()
                .ok_or_else(|| anyhow!("no default input device"))?,
        };
        let out_dev = match output {
            Some(n) => find(host.output_devices()?, n)
                .ok_or_else(|| anyhow!("no output device matching {n:?}"))?,
            None => host
                .default_output_device()
                .ok_or_else(|| anyhow!("no default output device"))?,
        };

        let pick_in = pick(in_dev.supported_input_configs()?, || {
            in_dev.default_input_config()
        })
        .context("reading input device formats")?;
        let pick_out = pick(out_dev.supported_output_configs()?, || {
            out_dev.default_output_config()
        })
        .context("reading output device formats")?;
        let (in_cfg, in_fmt) = config_for(pick_in);
        let (out_cfg, out_fmt) = config_for(pick_out);
        let in_rate = in_cfg.sample_rate;
        let out_rate = out_cfg.sample_rate;

        let (tx, captured) = channel();
        let input = match in_fmt {
            cpal::SampleFormat::F32 => input_stream::<f32>(&in_dev, in_cfg, tx)?,
            cpal::SampleFormat::I16 => input_stream::<i16>(&in_dev, in_cfg, tx)?,
            cpal::SampleFormat::I32 => input_stream::<i32>(&in_dev, in_cfg, tx)?,
            f => return Err(anyhow!("input sample format {f} is not supported")),
        };
        let playback = Arc::new(Mutex::new(Playback::default()));
        let output = match out_fmt {
            cpal::SampleFormat::F32 => output_stream::<f32>(&out_dev, out_cfg, playback.clone())?,
            cpal::SampleFormat::I16 => output_stream::<i16>(&out_dev, out_cfg, playback.clone())?,
            cpal::SampleFormat::I32 => output_stream::<i32>(&out_dev, out_cfg, playback.clone())?,
            f => return Err(anyhow!("output sample format {f} is not supported")),
        };
        input.play()?;
        output.play()?;
        eprintln!(
            "audio in: {} at {in_rate} Hz, out: {} at {out_rate} Hz",
            device_name(&in_dev),
            device_name(&out_dev)
        );
        Ok(SoundCard {
            _input: input,
            _output: output,
            captured,
            in_resample: Resampler::new(in_rate, FS as u32),
            out_rate,
            playback,
        })
    }
}

impl Audio for SoundCard {
    fn capture(&mut self) -> Vec<f32> {
        let mut raw = Vec::new();
        while let Ok(block) = self.captured.try_recv() {
            raw.extend(block);
        }
        self.in_resample.run(&raw)
    }

    fn play(&mut self, samples: Vec<f32>) {
        let out = Resampler::new(FS as u32, self.out_rate).run(&samples);
        let mut p = self.playback.lock().unwrap();
        p.queue.extend(out);
        p.drained_at = None;
    }

    fn playing(&self) -> bool {
        let p = self.playback.lock().unwrap();
        !p.queue.is_empty() || p.drained_at.is_some_and(|t| t.elapsed() < DRAIN_GRACE)
    }
}

/// Stands in when no sound card could be opened, so the host still gets answers.
pub struct NoAudio;

impl Audio for NoAudio {
    fn capture(&mut self) -> Vec<f32> {
        Vec::new()
    }

    fn play(&mut self, _samples: Vec<f32>) {}

    fn playing(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_keeps_tone_frequency_across_blocks() {
        let tone: Vec<f32> = (0..44_100)
            .map(|i| (std::f32::consts::TAU * 1000.0 * i as f32 / 44_100.0).sin())
            .collect();
        let mut r = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        for block in tone.chunks(441) {
            out.extend(r.run(block));
        }
        assert!((out.len() as i64 - 48_000).abs() <= 2, "{}", out.len());
        let crossings = out.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        assert!((crossings as i64 - 1000).abs() <= 2, "{crossings}");
    }
}
