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
            let n_channels =
                u32::try_from(audio.channels.len()).map_err(|_| JsValue::from_str("too many channels"))?;
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
