//! Sound (spec 6.9): kira mixer with effects, alerts and music tracks.
//!
//! Every sound is synthesised at start-up from a few oscillators and noise,
//! so the game ships no audio files and has no third-party audio licences.
//! Combat is one looping bed whose volume follows how many units fight on
//! screen, instead of a sound per shot, so big battles never turn into
//! noise (spec: "no cacophony", at most a handful of sources at once).

use crate::settings::Settings;
use kira::sound::static_sound::{StaticSoundData, StaticSoundHandle, StaticSoundSettings};
use kira::track::{TrackBuilder, TrackHandle};
use kira::{AudioManager, AudioManagerSettings, Decibels, DefaultBackend, Frame, Tween};
use std::f32::consts::TAU;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

const RATE: u32 = 44_100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sfx {
    Select,
    Order,
    Buy,
    Death,
    CityCaptured,
    CityLost,
    CapitalAttacked,
    Victory,
    Defeat,
}

impl Sfx {
    const COUNT: usize = 9;

    /// Shortest gap between two plays of the same sound, seconds.
    fn cooldown(self) -> f32 {
        match self {
            Sfx::Select | Sfx::Order => 0.05,
            Sfx::Buy => 0.08,
            Sfx::Death => 0.12,
            Sfx::CityCaptured | Sfx::CityLost => 0.5,
            Sfx::CapitalAttacked => 20.0,
            Sfx::Victory | Sfx::Defeat => 5.0,
        }
    }

    fn alert(self) -> bool {
        matches!(self, Sfx::CapitalAttacked | Sfx::CityLost)
    }
}

/// Amplitude 0..1 to decibels for kira.
fn db(a: f32) -> Decibels {
    if a <= 0.001 { Decibels::SILENCE } else { Decibels((20.0 * a.log10()).max(Decibels::SILENCE.0)) }
}

// ------------------------------------------------------------- synthesis

/// Deterministic white noise (xorshift), -1..1.
struct Noise(u32);

impl Noise {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        (self.0 as f32 / u32::MAX as f32) * 2.0 - 1.0
    }
}

fn samples(seconds: f32) -> usize {
    (seconds * RATE as f32) as usize
}

/// Sine notes `(start s, freq Hz, length s, gain)` with soft attack and an
/// exponential tail.
fn notes(total: f32, list: &[(f32, f32, f32, f32)]) -> Vec<f32> {
    let mut out = vec![0.0; samples(total)];
    for &(start, freq, len, gain) in list {
        let s0 = samples(start);
        let n = samples(len).min(out.len().saturating_sub(s0));
        for k in 0..n {
            let t = k as f32 / RATE as f32;
            let attack = (t / 0.008).min(1.0);
            let decay = (-4.0 * t / len).exp();
            // A touch of the octave makes it less of a pure test tone.
            let w = (TAU * freq * t).sin() + 0.25 * (TAU * 2.0 * freq * t).sin();
            out[s0 + k] += w * attack * decay * gain;
        }
    }
    out
}

/// Low thump of filtered noise.
fn thud(len: f32, cutoff: f32, seed: u32) -> Vec<f32> {
    let mut noise = Noise(seed);
    let a = (TAU * cutoff / RATE as f32).min(1.0);
    let mut lp = 0.0;
    (0..samples(len))
        .map(|k| {
            let t = k as f32 / RATE as f32;
            lp += a * (noise.next() - lp);
            lp * 3.0 * (-t / (len * 0.25)).exp()
        })
        .collect()
}

/// A loop of distant battle: rumble plus random crackle.
fn battle_bed(len: f32) -> Vec<f32> {
    let mut noise = Noise(0x1234_5678);
    let n = samples(len);
    let mut out = vec![0.0; n];
    let mut lp = 0.0;
    for v in out.iter_mut() {
        lp += 0.02 * (noise.next() - lp);
        *v = lp * 2.5;
    }
    // Crackle: short noise bursts at random times.
    let mut k = 0;
    while k < n {
        let gap = samples(0.02 + 0.08 * (noise.next() * 0.5 + 0.5));
        let burst = samples(0.012);
        let gain = 0.15 + 0.2 * (noise.next() * 0.5 + 0.5);
        for j in 0..burst.min(n - k) {
            out[k + j] += noise.next() * gain * (1.0 - j as f32 / burst as f32);
        }
        k += gap;
    }
    // Fade the ends into each other so the loop does not click.
    let fade = samples(0.05);
    for j in 0..fade {
        let w = j as f32 / fade as f32;
        let (a, b) = (out[j], out[n - fade + j]);
        out[j] = a * w + b * (1.0 - w);
    }
    out.truncate(n - fade);
    out
}

/// Ambient pad: slow chords of detuned sines, `bars` × 8 s, loops cleanly.
fn ambient(seed: u32) -> Vec<f32> {
    // A minor-ish progression (Hz of the chord tones).
    const CHORDS: [[f32; 4]; 4] = [
        [110.0, 164.81, 220.0, 261.63],
        [98.0, 146.83, 196.0, 246.94],
        [87.31, 130.81, 174.61, 220.0],
        [82.41, 123.47, 164.81, 207.65],
    ];
    let bar = 8.0;
    let n = samples(bar * CHORDS.len() as f32);
    let mut out = vec![0.0; n];
    let mut noise = Noise(seed | 1);
    for (c, chord) in CHORDS.iter().enumerate() {
        let start = samples(bar * c as f32);
        // Each chord fades in and out over 10 s, overlapping its neighbours.
        let len = samples(bar + 2.0);
        for (v, &f) in chord.iter().enumerate() {
            let detune = 1.0 + 0.002 * noise.next();
            let gain = 0.11 / (1.0 + v as f32 * 0.3);
            for k in 0..len {
                let t = k as f32 / RATE as f32;
                let env = (std::f32::consts::PI * t / (bar + 2.0)).sin().powi(2);
                let s = (TAU * f * detune * t).sin() + 0.3 * (TAU * f * 2.003 * t).sin();
                out[(start + k) % n] += s * env * gain;
            }
        }
    }
    out
}

fn data(mono: Vec<f32>, settings: StaticSoundSettings) -> StaticSoundData {
    let peak = mono.iter().fold(0.0f32, |m, v| m.max(v.abs())).max(1e-6);
    let scale = if peak > 0.95 { 0.95 / peak } else { 1.0 };
    let frames: Arc<[Frame]> = mono.into_iter().map(|v| Frame::new(v * scale, v * scale)).collect();
    StaticSoundData { sample_rate: RATE, frames, settings, slice: None }
}

fn effect(s: Sfx) -> Vec<f32> {
    let (c, e, g, a) = (523.25, 659.25, 783.99, 880.0);
    match s {
        Sfx::Select => notes(0.08, &[(0.0, 1320.0, 0.05, 0.25)]),
        Sfx::Order => notes(0.15, &[(0.0, 660.0, 0.07, 0.3), (0.05, 990.0, 0.08, 0.25)]),
        Sfx::Buy => notes(0.3, &[(0.0, c, 0.12, 0.35), (0.09, g, 0.18, 0.35)]),
        Sfx::Death => thud(0.18, 300.0, 77),
        Sfx::CityCaptured => notes(0.8, &[(0.0, c, 0.35, 0.3), (0.1, e, 0.35, 0.3), (0.2, g, 0.5, 0.35)]),
        Sfx::CityLost => notes(0.8, &[(0.0, g, 0.3, 0.3), (0.12, 622.25, 0.3, 0.3), (0.24, c, 0.5, 0.35)]),
        Sfx::CapitalAttacked => notes(
            1.3,
            &[
                (0.0, a, 0.18, 0.4),
                (0.2, e, 0.18, 0.4),
                (0.45, a, 0.18, 0.4),
                (0.65, e, 0.18, 0.4),
                (0.9, a, 0.18, 0.4),
                (1.1, e, 0.18, 0.4),
            ],
        ),
        Sfx::Victory => {
            notes(2.5, &[(0.0, c, 1.5, 0.25), (0.15, e, 1.5, 0.25), (0.3, g, 1.6, 0.25), (0.6, 2.0 * c, 1.8, 0.3)])
        }
        Sfx::Defeat => notes(2.5, &[(0.0, 220.0, 1.8, 0.3), (0.3, 261.63, 1.8, 0.25), (0.6, 164.81, 2.0, 0.3)]),
    }
}

const ALL: [Sfx; Sfx::COUNT] = [
    Sfx::Select,
    Sfx::Order,
    Sfx::Buy,
    Sfx::Death,
    Sfx::CityCaptured,
    Sfx::CityLost,
    Sfx::CapitalAttacked,
    Sfx::Victory,
    Sfx::Defeat,
];

// ------------------------------------------------------------------ mixer

pub struct Audio {
    manager: AudioManager,
    effects: TrackHandle,
    alerts: TrackHandle,
    music: TrackHandle,
    sounds: Vec<StaticSoundData>,
    last: [f32; Sfx::COUNT],
    time: f32,
    combat: Option<StaticSoundHandle>,
    combat_level: f32,
    /// Level last sent to the mixer.
    combat_sent: f32,
    in_match: Option<bool>,
    music_rx: Option<mpsc::Receiver<StaticSoundData>>,
    music_playing: Option<StaticSoundHandle>,
}

impl Audio {
    /// `None` when there is no audio device; the game then stays silent.
    pub fn new() -> Option<Audio> {
        let mut manager = match AudioManager::<DefaultBackend>::new(AudioManagerSettings::default()) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("no sound: {e}");
                return None;
            }
        };
        let effects = manager.add_sub_track(TrackBuilder::new()).ok()?;
        let alerts = manager.add_sub_track(TrackBuilder::new()).ok()?;
        let music = manager.add_sub_track(TrackBuilder::new()).ok()?;
        let sounds = ALL.iter().map(|&s| data(effect(s), StaticSoundSettings::new())).collect();
        // The music takes a moment to synthesise: do it off the main thread.
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("music".into())
            .spawn(move || {
                let _ = tx.send(data(ambient(7), StaticSoundSettings::new().loop_region(..)));
            })
            .ok()?;
        let mut a = Audio {
            manager,
            effects,
            alerts,
            music,
            sounds,
            last: [f32::NEG_INFINITY; Sfx::COUNT],
            time: 0.0,
            combat: None,
            combat_level: 0.0,
            combat_sent: 0.0,
            in_match: None,
            music_rx: Some(rx),
            music_playing: None,
        };
        let bed = data(battle_bed(3.0), StaticSoundSettings::new().loop_region(..).volume(Decibels::SILENCE));
        a.combat = a.effects.play(bed).ok();
        Some(a)
    }

    /// Track volumes from the settings; everything muted in the
    /// background if the player asked for that.
    pub fn apply(&mut self, s: &Settings, focused: bool) {
        let master = if focused || !s.mute_unfocused { s.master_volume } else { 0.0 };
        let tween = Tween { duration: Duration::from_millis(150), ..Default::default() };
        self.manager.main_track().set_volume(db(master), tween);
        self.effects.set_volume(db(s.effects_volume), tween);
        self.alerts.set_volume(db(s.alert_volume), tween);
        self.music.set_volume(db(s.music_volume * 0.6), tween);
    }

    /// Advances time; starts the music once it is ready.
    pub fn update(&mut self, dt: f32) {
        self.time += dt;
        if let Some(rx) = &self.music_rx {
            if let Ok(m) = rx.try_recv() {
                self.music_playing = self.music.play(m).ok();
                self.music_rx = None;
            }
        }
    }

    pub fn play(&mut self, s: Sfx) {
        self.play_with(s, 1.0, 1.0);
    }

    /// Plays `s` at `gain` and `rate` (pitch), unless it played too recently.
    pub fn play_with(&mut self, s: Sfx, gain: f32, rate: f64) {
        let k = ALL.iter().position(|&x| x == s).expect("listed");
        if self.time - self.last[k] < s.cooldown() {
            return;
        }
        self.last[k] = self.time;
        let d = self.sounds[k].volume(db(gain.clamp(0.0, 1.0))).playback_rate(rate);
        let track = if s.alert() { &mut self.alerts } else { &mut self.effects };
        let _ = track.play(d);
    }

    /// Fighting on screen: `units` in combat in view at `scale` px/tile.
    /// The battle bed rises with the log of the count and with zoom.
    pub fn set_combat(&mut self, units: u32, scale: f32) {
        let target = if units == 0 {
            0.0
        } else {
            let size = ((units as f32).ln_1p() / 8.0).min(1.0);
            let near = (scale / 12.0).clamp(0.35, 1.0);
            0.15 + 0.6 * size * near
        };
        // Smooth, so a flicker of combat does not pump the sound.
        let speed = if target > self.combat_level { 0.08 } else { 0.03 };
        self.combat_level += (target - self.combat_level) * speed;
        if (self.combat_level - self.combat_sent).abs() < 0.01 {
            return;
        }
        self.combat_sent = self.combat_level;
        if let Some(h) = &mut self.combat {
            h.set_volume(db(self.combat_level), Tween { duration: Duration::from_millis(60), ..Default::default() });
        }
    }

    /// Menu music volume vs. match: a bit quieter while playing.
    pub fn set_in_match(&mut self, in_match: bool) {
        if self.in_match == Some(in_match) && self.music_rx.is_none() {
            return;
        }
        if self.music_playing.is_some() {
            self.in_match = Some(in_match);
        }
        if let Some(h) = &mut self.music_playing {
            let gain = if in_match { 0.6 } else { 1.0 };
            h.set_volume(db(gain), Tween { duration: Duration::from_secs(2), ..Default::default() });
        }
        if !in_match {
            self.combat_level = 0.0;
            self.combat_sent = 0.0;
            if let Some(h) = &mut self.combat {
                h.set_volume(Decibels::SILENCE, Tween::default());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesised_sounds_are_sane() {
        for s in ALL {
            let v = effect(s);
            assert!(!v.is_empty(), "{s:?}");
            assert!(v.iter().all(|x| x.is_finite()), "{s:?}");
            assert!(v.iter().any(|x| x.abs() > 0.05), "{s:?} is silent");
        }
        let bed = battle_bed(1.0);
        assert!(bed.iter().all(|x| x.is_finite()));
        assert_eq!(db(0.0), Decibels::SILENCE);
        assert!((db(1.0).0).abs() < 1e-6);
    }
}
