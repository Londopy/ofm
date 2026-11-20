//! Morse CW identification audio.

use crate::modem::FS;

fn pattern(c: char) -> Option<&'static str> {
    Some(match c.to_ascii_uppercase() {
        'A' => ".-",
        'B' => "-...",
        'C' => "-.-.",
        'D' => "-..",
        'E' => ".",
        'F' => "..-.",
        'G' => "--.",
        'H' => "....",
        'I' => "..",
        'J' => ".---",
        'K' => "-.-",
        'L' => ".-..",
        'M' => "--",
        'N' => "-.",
        'O' => "---",
        'P' => ".--.",
        'Q' => "--.-",
        'R' => ".-.",
        'S' => "...",
        'T' => "-",
        'U' => "..-",
        'V' => "...-",
        'W' => ".--",
        'X' => "-..-",
        'Y' => "-.--",
        'Z' => "--..",
        '0' => "-----",
        '1' => ".----",
        '2' => "..---",
        '3' => "...--",
        '4' => "....-",
        '5' => ".....",
        '6' => "-....",
        '7' => "--...",
        '8' => "---..",
        '9' => "----.",
        '/' => "-..-.",
        '-' => "-....-",
        _ => return None,
    })
}

/// Keys `text` as CW at `wpm` on a `tone_hz` sine, with 5 ms ramps to keep clicks down.
pub fn cw(text: &str, wpm: f32, tone_hz: f32, amplitude: f32) -> Vec<f32> {
    let unit = (1.2 / wpm * FS as f32) as usize;
    let mut keying: Vec<(bool, usize)> = Vec::new();
    for word in text.split_whitespace() {
        for c in word.chars().filter_map(pattern) {
            for e in c.chars() {
                keying.push((true, if e == '.' { unit } else { 3 * unit }));
                keying.push((false, unit));
            }
            keying.push((false, 2 * unit));
        }
        keying.push((false, 4 * unit));
    }
    let ramp = FS / 200;
    let mut out = Vec::new();
    for (on, len) in keying {
        for i in 0..len {
            let env = if on {
                (i.min(len - 1 - i) as f32 / ramp as f32).min(1.0)
            } else {
                0.0
            };
            let t = out.len() as f32 / FS as f32;
            out.push(env * amplitude * (std::f32::consts::TAU * tone_hz * t).sin());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_follows_paris_standard() {
        // "PARIS " is 50 units, so at 20 wpm it lasts 3 s
        let audio = cw("PARIS", 20.0, 800.0, 0.5);
        let secs = audio.len() as f32 / FS as f32;
        assert!((secs - 3.0).abs() < 0.1, "{secs}");
    }

    #[test]
    fn unknown_characters_are_skipped() {
        assert_eq!(
            cw("E", 20.0, 800.0, 0.5).len(),
            cw("E#", 20.0, 800.0, 0.5).len()
        );
    }
}
