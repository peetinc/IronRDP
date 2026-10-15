# Web Audio Playback (PCM) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Play RDPSND audio (PCM 16-bit) in the browser client behind an opt-in `enable_audio_playback` extension, structured so compressed codecs slot in later.

**Architecture:** New `crates/ironrdp-web/src/audio/` module: `codec.rs` (pure Rust: advertised formats + bytes→planar f32), `player.rs` (Web Audio `AudioContext` + a pure `Schedule` jitter buffer), `mod.rs` (`WasmRdpsndBackend: RdpsndClientHandler`). `session.rs` swaps it in for `NoopRdpsndBackend` when the extension is on.

**Tech Stack:** Rust (wasm32), `web-sys` 0.3.103 Web Audio bindings, `ironrdp-rdpsnd` client, TypeScript extension factory, Svelte test client.

**Spec:** `docs/plans/2026-10-15-web-audio-playback-design.md`

## Global Constraints

- Default off: without the extension, behaviour is byte-identical to today (Noop backend, `enable_audio_playback: false`).
- RDPSND stays attached in every configuration (RDPDR depends on it, MS-RDPEFS Appendix A<1>).
- Audio failure never fails the connection: `AudioContext` creation failure → warn + Noop backend + `INFO_NOAUDIOPLAYBACK` kept set.
- Advertise only formats `codec::decode` handles. Phase 1: PCM, 16 bits, 1–2 channels.
- `LEAD_SECS = 0.05`, `MAX_QUEUED_SECS = 0.5`.
- Do not advertise `PITCH`. Advertise `VOLUME`.
- No `unwrap`/panic on server data; no new non-workspace deps (only `web-sys` features).
- Log style: capitalized, no trailing period, structured fields (`STYLE.md`). Error style: lowercase.
- Inline `#[cfg(test)] mod tests` at the end of each file.

## Review Focus

1. Server sends a wave block whose length is not a multiple of `n_block_align` → partial trailing frame dropped, rest plays (test in Task 1).
2. Tab backgrounded / stall then a burst of blocks → queue capped at 0.5 s, excess dropped, no unbounded latency (test in Task 2).
3. Arrival gap longer than the queued audio (underrun) → next block restarts at `now + LEAD`, not in the past (test in Task 2).
4. Server renegotiates formats mid-session (`AudioFormat` PDU / Close) → schedule reset, new format logged (test in Task 3 via `close` → `active_format` cleared; schedule reset covered by Task 2 `reset` test).
5. Audio on + drive share on → RDPDR still works (manual check, Task 5).

---

### Task 1: `codec` — advertised formats and PCM decode

**Files:**
- Create: `crates/ironrdp-web/src/audio/mod.rs` (module declarations only for now)
- Create: `crates/ironrdp-web/src/audio/codec.rs`
- Modify: `crates/ironrdp-web/src/lib.rs` (add `mod audio;`)

**Interfaces:**
- Produces: `codec::supported_formats() -> Vec<AudioFormat>`, `codec::decode(&AudioFormat, &[u8], Volume) -> Option<DecodedAudio>`, `codec::DecodedAudio { sample_rate: u32, channels: Vec<Vec<f32>> }` with `fn frames(&self) -> usize`, `codec::Volume { left: u16, right: u16 }` with `const FULL`.

- [ ] **Step 1: Scaffold module and write the failing tests**

`crates/ironrdp-web/src/lib.rs` — add next to the other `mod` lines:

```rust
mod audio;
```

`crates/ironrdp-web/src/audio/mod.rs`:

```rust
//! RDPSND audio playback through Web Audio.

mod codec;
```

`crates/ironrdp-web/src/audio/codec.rs` — tests first (implementation stubs `todo!()` so it compiles):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(n_channels: u16) -> AudioFormat {
        supported_formats()
            .into_iter()
            .find(|format| format.n_channels == n_channels)
            .expect("table has this channel count")
    }

    fn le(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|sample| sample.to_le_bytes()).collect()
    }

    #[test]
    fn stereo_is_split_into_planar_channels() {
        let audio = decode(&pcm(2), &le(&[0, i16::MAX, i16::MIN, 16384]), Volume::FULL).expect("decodes");
        assert_eq!(audio.sample_rate, 48000);
        assert_eq!(audio.channels.len(), 2);
        assert_eq!(audio.channels[0], vec![0.0, -1.0]);
        assert_eq!(audio.channels[1], vec![f32::from(i16::MAX) / 32768.0, 0.5]);
        assert_eq!(audio.frames(), 2);
    }

    #[test]
    fn mono_uses_left_volume() {
        let volume = Volume { left: 0, right: 0xFFFF };
        let audio = decode(&pcm(1), &le(&[16384, -16384]), volume).expect("decodes");
        assert_eq!(audio.channels, vec![vec![0.0, 0.0]]);
    }

    #[test]
    fn volume_scales_each_lane() {
        let volume = Volume { left: 0xFFFF, right: 0 };
        let audio = decode(&pcm(2), &le(&[16384, 16384]), volume).expect("decodes");
        assert_eq!(audio.channels[0], vec![0.5]);
        assert_eq!(audio.channels[1], vec![0.0]);
    }

    #[test]
    fn trailing_partial_frame_is_dropped() {
        let mut data = le(&[100, 200, 300]);
        data.push(0x7F);
        let audio = decode(&pcm(2), &data, Volume::FULL).expect("decodes");
        assert_eq!(audio.frames(), 1);
    }

    #[test]
    fn short_or_empty_block_yields_nothing() {
        assert_eq!(decode(&pcm(2), &[], Volume::FULL), None);
        assert_eq!(decode(&pcm(2), &[1, 2, 3], Volume::FULL), None);
    }

    #[test]
    fn unsupported_formats_yield_nothing() {
        let data = le(&[1, 2, 3, 4]);

        let mut eight_bit = pcm(2);
        eight_bit.bits_per_sample = 8;
        assert_eq!(decode(&eight_bit, &data, Volume::FULL), None);

        let mut six_channel = pcm(2);
        six_channel.n_channels = 6;
        assert_eq!(decode(&six_channel, &data, Volume::FULL), None);

        let mut adpcm = pcm(2);
        adpcm.format = WaveFormat::ADPCM;
        assert_eq!(decode(&adpcm, &data, Volume::FULL), None);
    }

    #[test]
    fn advertised_formats_are_consistent_pcm16() {
        let formats = supported_formats();
        assert!(!formats.is_empty());
        for format in formats {
            assert_eq!(format.format, WaveFormat::PCM);
            assert_eq!(format.bits_per_sample, 16);
            assert!((1..=2).contains(&format.n_channels));
            assert_eq!(format.n_block_align, format.n_channels * 2);
            assert_eq!(
                format.n_avg_bytes_per_sec,
                format.n_samples_per_sec * u32::from(format.n_block_align)
            );
            assert_eq!(format.data, None);
        }
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ironrdp-web --lib audio::codec`
Expected: FAIL — `todo!()` panics (or compile errors for missing items until stubs exist).

- [ ] **Step 3: Implement**

Top of `crates/ironrdp-web/src/audio/codec.rs`:

```rust
//! RDPSND wave decoding for the browser player.
//!
//! Pure Rust and free of `web-sys`, so it is unit-testable on the host. Every
//! format [`supported_formats`] advertises must be decodable by [`decode`]:
//! `Rdpsnd` negotiates only from that list (filtered to what the server offers),
//! so it is the single place a new codec gets enabled.

use ironrdp::rdpsnd::pdu::{AudioFormat, WaveFormat};

/// Decoded audio, planar (one `Vec` per channel, equal lengths), the shape
/// `AudioBuffer::copy_to_channel` takes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DecodedAudio {
    pub(crate) sample_rate: u32,
    pub(crate) channels: Vec<Vec<f32>>,
}

impl DecodedAudio {
    pub(crate) fn frames(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }
}

/// Server-set playback volume (from the MS-RDPEA Volume PDU, [`VolumePdu`]); `0xFFFF` is full scale.
///
/// [`VolumePdu`]: ironrdp::rdpsnd::pdu::VolumePdu
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Volume {
    pub(crate) left: u16,
    pub(crate) right: u16,
}

impl Volume {
    pub(crate) const FULL: Self = Self {
        left: 0xFFFF,
        right: 0xFFFF,
    };
}

/// Formats advertised to the server, best first.
///
/// Phase 1 is uncompressed 16-bit PCM only (the same rates the native backend offers).
/// Web Audio resamples any `AudioBuffer` rate to the device, so no device filtering is needed.
pub(crate) fn supported_formats() -> Vec<AudioFormat> {
    const PCM16: &[(u16, u32)] = &[
        (2, 48000),
        (2, 44100),
        (2, 22050),
        (2, 16000),
        (1, 44100),
        (1, 22050),
        (1, 16000),
        (1, 8000),
    ];

    PCM16
        .iter()
        .map(|&(n_channels, n_samples_per_sec)| {
            let n_block_align = n_channels * 2 /* bytes per 16-bit sample */;
            AudioFormat {
                format: WaveFormat::PCM,
                n_channels,
                n_samples_per_sec,
                n_avg_bytes_per_sec: n_samples_per_sec * u32::from(n_block_align),
                n_block_align,
                bits_per_sample: 16,
                data: None,
            }
        })
        .collect()
}

/// Decode one wave block, applying `volume`.
///
/// Returns `None` for formats this player does not handle, and for blocks too
/// short to hold a single frame. A trailing partial frame is dropped.
pub(crate) fn decode(format: &AudioFormat, data: &[u8], volume: Volume) -> Option<DecodedAudio> {
    match (format.format, format.bits_per_sample) {
        (WaveFormat::PCM, 16) if (1..=2).contains(&format.n_channels) => {
            let n_channels = usize::from(format.n_channels);
            let frame_len = n_channels * 2 /* bytes per 16-bit sample */;
            let frames = data.len() / frame_len;
            if frames == 0 {
                return None;
            }

            // Same lane mapping as the native backend: left on even lanes, right on odd;
            // mono takes the left level. Linear amplitude scale.
            let gains = [volume.left, volume.right].map(|level| f32::from(level) / f32::from(u16::MAX));

            let mut channels = vec![Vec::with_capacity(frames); n_channels];
            for frame in data.chunks_exact(frame_len) {
                for ((sample, out), gain) in frame.chunks_exact(2).zip(channels.iter_mut()).zip(gains) {
                    let sample = i16::from_le_bytes([sample[0], sample[1]]);
                    out.push(f32::from(sample) / 32768.0 * gain);
                }
            }

            Some(DecodedAudio {
                sample_rate: format.n_samples_per_sec,
                channels,
            })
        }
        _ => None,
    }
}
```

Note for the `volume_scales_each_lane` / `stereo...` tests: `0xFFFF / 0xFFFF == 1.0` exactly, and `16384 / 32768 == 0.5` exactly, so `assert_eq!` on f32 is sound for these values.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ironrdp-web --lib audio::codec`
Expected: 7 passed. (`dead_code` warnings for `codec` items are expected until Task 3.)

- [ ] **Step 5: Commit**

```bash
git add crates/ironrdp-web/src/lib.rs crates/ironrdp-web/src/audio
git commit -m "feat(web): RDPSND PCM decoder for browser audio playback"
```

---

### Task 2: `player` — schedule and Web Audio output

**Files:**
- Create: `crates/ironrdp-web/src/audio/player.rs`
- Modify: `crates/ironrdp-web/src/audio/mod.rs` (add `mod player;`)
- Modify: `crates/ironrdp-web/Cargo.toml` (web-sys features)

**Interfaces:**
- Consumes: `codec::DecodedAudio` (Task 1).
- Produces: `player::Player::new() -> anyhow::Result<Player>`, `Player::enqueue(&mut self, &DecodedAudio)`, `Player::reset(&mut self)`; pure `player::Schedule` with `place(&mut self, now: f64, duration: f64) -> Option<f64>` and `reset(&mut self)`; consts `LEAD_SECS`, `MAX_QUEUED_SECS`.

- [ ] **Step 1: Write the failing `Schedule` tests**

End of `crates/ironrdp-web/src/audio/player.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_block_starts_after_lead() {
        let mut schedule = Schedule::default();
        assert_eq!(schedule.place(10.0, 0.02), Some(10.0 + LEAD_SECS));
    }

    #[test]
    fn contiguous_blocks_play_back_to_back() {
        let mut schedule = Schedule::default();
        let first = schedule.place(10.0, 0.02).expect("placed");
        let second = schedule.place(10.001, 0.02).expect("placed");
        assert_eq!(second, first + 0.02);
    }

    #[test]
    fn underrun_restarts_after_lead() {
        let mut schedule = Schedule::default();
        schedule.place(10.0, 0.02).expect("placed");
        // Next block arrives long after the queued audio ran out.
        assert_eq!(schedule.place(20.0, 0.02), Some(20.0 + LEAD_SECS));
    }

    #[test]
    fn burst_beyond_cap_is_dropped() {
        let mut schedule = Schedule::default();
        let now = 10.0;
        let mut placed = 0;
        let mut dropped = 0;
        for _ in 0..100 {
            match schedule.place(now, 0.02) {
                Some(start) => {
                    assert!(start - now <= MAX_QUEUED_SECS);
                    placed += 1;
                }
                None => dropped += 1,
            }
        }
        assert!(0 < placed);
        assert!(0 < dropped);
    }

    #[test]
    fn reset_forgets_the_queue() {
        let mut schedule = Schedule::default();
        schedule.place(10.0, 0.4).expect("placed");
        schedule.reset();
        assert_eq!(schedule.place(10.0, 0.02), Some(10.0 + LEAD_SECS));
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p ironrdp-web --lib audio::player`
Expected: FAIL to compile (`Schedule` not defined).

- [ ] **Step 3: Implement `Schedule` and `Player`**

`crates/ironrdp-web/Cargo.toml` — add to the `web-sys` feature list, keeping it alphabetical:

```toml
    "AudioBuffer",
    "AudioBufferSourceNode",
    "AudioContext",
    "AudioContextState",
    "AudioDestinationNode",
    "AudioNode",
    "AudioScheduledSourceNode",
```

`crates/ironrdp-web/src/audio/mod.rs`:

```rust
//! RDPSND audio playback through Web Audio.

mod codec;
mod player;
```

`crates/ironrdp-web/src/audio/player.rs` (above the tests):

```rust
//! Web Audio output for decoded RDPSND blocks.
//!
//! Each block becomes one `AudioBufferSourceNode` started at a precise context
//! time, so blocks play gapless. [`Schedule`] holds the timing policy; it is
//! pure so it is unit-testable on the host.

use core::cell::Cell;
use std::rc::Rc;

use anyhow::anyhow;
use tracing::{debug, warn};
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::{AudioContext, AudioContextState};

use super::codec::DecodedAudio;

/// Delay before the first block (and after an underrun), absorbing arrival jitter.
pub(crate) const LEAD_SECS: f64 = 0.05;

/// Most audio allowed to sit in the queue. `Rdpsnd` confirms each block on receipt,
/// so the server is never throttled by playback: without a cap, a stall (background
/// tab, GC pause, network burst) would turn into permanent latency.
pub(crate) const MAX_QUEUED_SECS: f64 = 0.5;

/// Gapless block timing on the `AudioContext` clock.
#[derive(Debug, Default)]
pub(crate) struct Schedule {
    /// Context time at which the queued audio runs out.
    next_start: Option<f64>,
}

impl Schedule {
    /// Context time to start a block of `duration` seconds, or `None` to drop it
    /// because the queue already holds [`MAX_QUEUED_SECS`].
    pub(crate) fn place(&mut self, now: f64, duration: f64) -> Option<f64> {
        let start = match self.next_start {
            Some(next_start) if now <= next_start => next_start,
            // First block, or the queue ran dry (underrun).
            _ => now + LEAD_SECS,
        };

        if MAX_QUEUED_SECS < start - now {
            return None;
        }

        self.next_start = Some(start + duration);
        Some(start)
    }

    pub(crate) fn reset(&mut self) {
        self.next_start = None;
    }
}

#[derive(Debug)]
pub(crate) struct Player {
    context: AudioContext,
    schedule: Schedule,
    /// Set while an `AudioContext::resume()` promise is outstanding.
    resume_pending: Rc<Cell<bool>>,
}

impl Player {
    pub(crate) fn new() -> anyhow::Result<Self> {
        let context = AudioContext::new().map_err(|error| anyhow!("create AudioContext: {error:?}"))?;

        Ok(Self {
            context,
            schedule: Schedule::default(),
            resume_pending: Rc::new(Cell::new(false)),
        })
    }

    pub(crate) fn enqueue(&mut self, audio: &DecodedAudio) {
        // Autoplay policy: a context created without a fresh user gesture starts
        // suspended. `resume()` succeeds once the page has had any user activation
        // (the Connect click); until then the promise stays pending, so ask once.
        if self.context.state() == AudioContextState::Suspended && !self.resume_pending.get() {
            match self.context.resume() {
                Ok(promise) => {
                    self.resume_pending.set(true);
                    let resume_pending = Rc::clone(&self.resume_pending);
                    wasm_bindgen_futures::spawn_local(async move {
                        if let Err(error) = JsFuture::from(promise).await {
                            debug!(?error, "AudioContext resume rejected");
                        }
                        resume_pending.set(false);
                    });
                }
                Err(error) => debug!(?error, "AudioContext resume failed"),
            }
        }

        let Ok(frames) = u32::try_from(audio.frames()) else {
            warn!(frames = audio.frames(), "Dropping oversized RDPSND block");
            return;
        };
        if frames == 0 || audio.sample_rate == 0 {
            return;
        }

        let duration = f64::from(frames) / f64::from(audio.sample_rate);
        let Some(when) = self.schedule.place(self.context.current_time(), duration) else {
            debug!(duration, "Dropping RDPSND block: playback queue is full");
            return;
        };

        if let Err(error) = start_block(&self.context, audio, frames, when) {
            warn!(?error, "Failed to schedule RDPSND block");
        }

        fn start_block(context: &AudioContext, audio: &DecodedAudio, frames: u32, when: f64) -> Result<(), JsValue> {
            let n_channels = u32::try_from(audio.channels.len()).map_err(|_| JsValue::from_str("too many channels"))?;
            #[expect(
                clippy::as_conversions,
                clippy::cast_precision_loss,
                reason = "RDPSND sample rates are far below f32's exact-integer range (2^24)"
            )]
            let sample_rate = audio.sample_rate as f32;

            let buffer = context.create_buffer(n_channels, frames, sample_rate)?;
            for (index, samples) in audio.channels.iter().enumerate() {
                let index = i32::try_from(index).map_err(|_| JsValue::from_str("too many channels"))?;
                buffer.copy_to_channel(samples, index)?;
            }

            let source = context.create_buffer_source()?;
            source.set_buffer(Some(&buffer));
            source.connect_with_audio_node(&context.destination())?;
            source.start_with_when(when)?;
            Ok(())
        }
    }

    /// Forget queued timing (format change or server Close). Already-started
    /// sources play out.
    pub(crate) fn reset(&mut self) {
        self.schedule.reset();
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Err(error) = self.context.close() {
            debug!(?error, "Failed to close AudioContext");
        }
    }
}
```

- [ ] **Step 4: Run tests and the wasm build**

Run: `cargo test -p ironrdp-web --lib audio`
Expected: 12 passed (7 codec + 5 player).

Run: `cargo check -p ironrdp-web --target wasm32-unknown-unknown`
Expected: builds (dead-code warnings only). If a `web-sys` method is missing, add the feature named in the compiler error.

- [ ] **Step 5: Commit**

```bash
git add crates/ironrdp-web/Cargo.toml crates/ironrdp-web/src/audio
git commit -m "feat(web): Web Audio player with bounded jitter buffer for RDPSND"
```

---

### Task 3: `WasmRdpsndBackend` and session wiring

**Files:**
- Modify: `crates/ironrdp-web/src/audio/mod.rs`
- Modify: `crates/ironrdp-web/src/session.rs` (builder field, extension arm, `connect` config, `ConnectParams`, attach site)

**Interfaces:**
- Consumes: `codec::{supported_formats, decode, Volume}`, `player::Player`.
- Produces: `audio::WasmRdpsndBackend::new() -> anyhow::Result<WasmRdpsndBackend>` (impl `RdpsndClientHandler`); extension ident `enable_audio_playback: bool`; `ConnectParams.rdpsnd_backend: Option<WasmRdpsndBackend>`.

- [ ] **Step 1: Write the failing test**

The backend owns an `AudioContext`, so it cannot be constructed on the host; the host-testable piece is the volume mapping. End of `crates/ironrdp-web/src/audio/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_pdu_maps_lanes() {
        let volume = volume_from_pdu(&VolumePdu {
            volume_left: 0x1234,
            volume_right: 0xABCD,
        });
        assert_eq!(
            volume,
            Volume {
                left: 0x1234,
                right: 0xABCD
            }
        );
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p ironrdp-web --lib audio::tests`
Expected: FAIL to compile (`volume_from_pdu` not defined).

- [ ] **Step 3: Implement the backend**

`crates/ironrdp-web/src/audio/mod.rs` (above the tests):

```rust
//! RDPSND audio playback through Web Audio.
//!
//! [`WasmRdpsndBackend`] is the `RdpsndClientHandler` the session attaches when
//! the `enable_audio_playback` extension is on. Wave bytes go through
//! [`codec::decode`] to planar f32 and then to [`player::Player`]. A new codec is
//! added in `codec` alone (sync) or, for WebCodecs, with an async path that feeds
//! the player from the decoder's output callback — see the design doc
//! `docs/plans/2026-10-15-web-audio-playback-design.md`.

mod codec;
mod player;

use std::borrow::Cow;

use ironrdp::rdpsnd::client::RdpsndClientHandler;
use ironrdp::rdpsnd::pdu::{AudioFormat, AudioFormatFlags, PitchPdu, VolumePdu};
use tracing::{info, trace};

use self::codec::Volume;
use self::player::Player;

#[derive(Debug)]
pub(crate) struct WasmRdpsndBackend {
    formats: Vec<AudioFormat>,
    player: Player,
    volume: Volume,
    /// Last format a block played in; logged on change so the negotiated codec is visible.
    active_format: Option<AudioFormat>,
}

// SAFETY: `RdpsndClientHandler: Send` is a supertrait bound required by `Rdpsnd`'s
// `Box<dyn RdpsndClientHandler>` storage, not evidence the backend ever crosses an OS
// thread. Like `WasmDriveBackend`, this backend is created and driven on the single wasm
// event-loop thread; its `AudioContext` and `Rc` never leave it.
unsafe impl Send for WasmRdpsndBackend {}

impl WasmRdpsndBackend {
    pub(crate) fn new() -> anyhow::Result<Self> {
        Ok(Self {
            formats: codec::supported_formats(),
            player: Player::new()?,
            volume: Volume::FULL,
            active_format: None,
        })
    }
}

impl RdpsndClientHandler for WasmRdpsndBackend {
    fn get_flags(&self) -> AudioFormatFlags {
        // Volume is applied while decoding. Pitch is not implemented, so not advertised.
        AudioFormatFlags::VOLUME
    }

    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn wave(&mut self, format: &AudioFormat, _ts: u32, data: Cow<'_, [u8]>) {
        if self.active_format.as_ref() != Some(format) {
            info!(?format, "Playing RDPSND format");
            self.active_format = Some(format.clone());
        }

        match codec::decode(format, &data, self.volume) {
            Some(audio) => self.player.enqueue(&audio),
            None => trace!(len = data.len(), "Dropping undecodable RDPSND block"),
        }
    }

    fn set_volume(&mut self, volume: VolumePdu) {
        self.volume = volume_from_pdu(&volume);
    }

    fn set_pitch(&mut self, _pitch: PitchPdu) {}

    fn close(&mut self) {
        self.active_format = None;
        self.player.reset();
    }
}

fn volume_from_pdu(volume: &VolumePdu) -> Volume {
    Volume {
        left: volume.volume_left,
        right: volume.volume_right,
    }
}
```

(`volume_from_pdu` is a named function rather than an inline block only so the lane mapping is unit-testable without an `AudioContext`.)

- [ ] **Step 4: Wire into `session.rs`**

Imports — next to the existing `use ironrdp::rdpsnd::client::{NoopRdpsndBackend, Rdpsnd};`:

```rust
use crate::audio::WasmRdpsndBackend;
```

`SessionBuilderInner` — after `use_display_control: bool,`:

```rust
    enable_audio_playback: bool,
```

`impl Default for SessionBuilderInner` — after `use_display_control: false,`:

```rust
            enable_audio_playback: false,
```

`SessionBuilder::extension` — after the `display_control` arm:

```rust
            |enable_audio_playback: bool| { self.0.borrow_mut().enable_audio_playback = enable_audio_playback };
```

`SessionBuilder::connect` — right after `config.enable_credssp = enable_credssp;`:

```rust
        // Create the AudioContext as early as possible, while the page's user activation from
        // the Connect click is freshest (autoplay policy). Audio failing must not fail the
        // connection: fall back to the no-op backend and keep the server playing locally.
        let rdpsnd_backend = if self.0.borrow().enable_audio_playback {
            match WasmRdpsndBackend::new() {
                Ok(backend) => Some(backend),
                Err(error) => {
                    warn!(%error, "Audio playback unavailable; continuing without sound");
                    None
                }
            }
        } else {
            None
        };
        config.enable_audio_playback = rdpsnd_backend.is_some();
```

`connect(ConnectParams { ... })` call site — after `use_display_control,`:

```rust
            rdpsnd_backend,
```

`struct ConnectParams` — after the `use_display_control` field:

```rust
    /// `Some` when the `enable_audio_playback` extension is on and Web Audio is available.
    rdpsnd_backend: Option<WasmRdpsndBackend>,
```

`async fn connect(ConnectParams { ... })` destructuring — after `use_display_control,`:

```rust
        rdpsnd_backend,
```

Attach site — replace:

```rust
        // Windows servers only speak on RDPDR when RDPSND is advertised too
        // (MS-RDPEFS Appendix A<1>). We do not play audio in the web client,
        // but the no-op RDPSND processor satisfies that channel dependency.
        connector.attach_static_channel(Rdpsnd::new(Box::new(NoopRdpsndBackend)));
```

with:

```rust
        // Windows servers only speak on RDPDR when RDPSND is advertised too
        // (MS-RDPEFS Appendix A<1>), so RDPSND is always attached: with the Web Audio
        // backend when audio playback is enabled, otherwise with the no-op backend.
        let rdpsnd_backend: Box<dyn RdpsndClientHandler> = match rdpsnd_backend {
            Some(backend) => Box::new(backend),
            None => Box::new(NoopRdpsndBackend),
        };
        connector.attach_static_channel(Rdpsnd::new(rdpsnd_backend));
```

and extend the import to `use ironrdp::rdpsnd::client::{NoopRdpsndBackend, Rdpsnd, RdpsndClientHandler};`.

- [ ] **Step 5: Run tests, wasm build, lints**

Run: `cargo test -p ironrdp-web --lib audio`
Expected: 13 passed.

Run: `cargo check -p ironrdp-web --target wasm32-unknown-unknown`
Expected: builds with no warnings from `audio/`.

Run: `cargo xtask check lints -v`
Expected: pass.

- [ ] **Step 6: Commit**

```bash
git add crates/ironrdp-web/src
git commit -m "feat(web): play RDPSND audio behind the enable_audio_playback extension"
```

---

### Task 4: TypeScript extension and Svelte toggle

**Files:**
- Modify: `web-client/iron-remote-desktop-rdp/src/main.ts` (after `displayControl`)
- Modify: `web-client/iron-svelte-client/src/lib/login/login.svelte`
- Modify (generated): `web-client/iron-remote-desktop-rdp/dist/*`, `web-client/DIST_SOURCE_COMMIT`

**Interfaces:**
- Consumes: extension ident `enable_audio_playback` (Task 3).
- Produces: `enableAudioPlayback(enable: boolean): Extension` exported from `iron-remote-desktop-rdp`.

- [ ] **Step 1: Add the factory**

`web-client/iron-remote-desktop-rdp/src/main.ts`, after `displayControl`:

```ts
/**
 * Play remote session audio (RDPSND) in the browser through Web Audio.
 *
 * Off by default. Browsers only start audio after a user gesture on the page; connecting
 * from a click satisfies that.
 */
export function enableAudioPlayback(enable: boolean): Extension {
    return new Extension('enable_audio_playback', enable);
}
```

- [ ] **Step 2: Wire the Svelte test client**

`login.svelte` script — add `enableAudioPlayback` to the `iron-remote-desktop-rdp` import list, add state next to `enable_clipboard`:

```ts
    let enable_audio = true;
```

In `StartSession`, after `.withExtension(displayControl(true));` chain ends:

```ts
        if (enable_audio) {
            configBuilder.withExtension(enableAudioPlayback(true));
        }
```

Template — after the `enable_clipboard` checkbox wrapper:

```svelte
                            <div class="checkbox-wrapper">
                                <input
                                    id="enable_audio"
                                    type="checkbox"
                                    bind:checked={enable_audio}
                                    style="width: 1.5em; height: 1.5em; margin-right: 0.5em;"
                                />
                                <label for="enable_audio">Play Remote Audio</label>
                            </div>
```

- [ ] **Step 3: Build and check**

Run: `cargo xtask web install -v && cargo xtask web check -v`
Expected: pass.

Run: `cargo xtask web build -v`
Expected: rebuilds `crates/ironrdp-web/pkg` and the `dist` bundles.

- [ ] **Step 4: Commit source, then dist separately (repo convention)**

```bash
git add web-client/iron-remote-desktop-rdp/src/main.ts web-client/iron-svelte-client/src/lib/login/login.svelte
git commit -m "feat(web-client): enableAudioPlayback extension + test-client toggle"
# Set DIST_SOURCE_COMMIT to the commit above, then:
git add web-client/DIST_SOURCE_COMMIT web-client/*/dist
git commit -m "chore(web-client): rebuild dist artifacts (audio playback)"
```

---

### Task 5: Verification

- [ ] `cargo xtask check fmt -v`, `cargo xtask check lints -v`, `cargo xtask check typos -v`, `cargo xtask check locks -v` (lockfile must be unchanged — only `web-sys` features changed).
- [ ] `cargo xtask wasm check -v`.
- [ ] `cargo test -p ironrdp-web --lib audio` — 13 pass. (Baseline: 19 `clipboard::tests` fail natively; unrelated.)
- [ ] Manual (user, against the Windows test host): connect with "Play Remote Audio" on, play a sound → audible; browser console shows `Playing RDPSND format` with the negotiated format; share a folder → RDPDR still works. Repeat with the box off → silent, server-side audio, drive still works.
