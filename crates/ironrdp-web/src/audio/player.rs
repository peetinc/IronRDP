//! Web Audio output for decoded RDPSND blocks.
//!
//! Each block becomes one `AudioBufferSourceNode` started at a precise context
//! time, so blocks play gapless. [`Schedule`] holds the timing policy; it is
//! pure so it is unit-testable on the host.

use anyhow::anyhow;
use tracing::{debug, warn};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{AudioContext, AudioContextState, AudioDestinationNode, Window};

use super::codec::DecodedAudio;

/// Delay before the first block (and after an underrun), absorbing arrival jitter.
pub(crate) const LEAD_SECS: f64 = 0.05;

/// Most audio allowed to sit in the queue. `Rdpsnd` confirms each block on receipt,
/// so the server is never throttled by playback: without a cap, a stall (background
/// tab, GC pause, network burst) would turn into permanent latency.
pub(crate) const MAX_QUEUED_SECS: f64 = 0.5;

/// Once the cap is hit, arrivals are dropped until the queue drains below this, so a
/// stall costs one dropped stretch of audio instead of permanent latency at the cap.
pub(crate) const LOW_WATERMARK_SECS: f64 = 2.0 * LEAD_SECS;

/// Whether `resume()` can still start this context. Anything but running or closed
/// (`suspended`, or a state web-sys does not model such as Safari's `interrupted`).
pub(crate) fn is_resumable(state: AudioContextState) -> bool {
    !matches!(state, AudioContextState::Running | AudioContextState::Closed)
}

/// Events that grant user activation, the only time browsers honor `resume()` for a
/// context the autoplay policy blocked ([HTML activation-triggering input events]).
///
/// [HTML activation-triggering input events]: https://html.spec.whatwg.org/multipage/interaction.html#activation-triggering-input-event
const GESTURE_EVENTS: [&str; 4] = ["keydown", "pointerdown", "pointerup", "touchend"];

/// Ask a context blocked by the autoplay policy to start. Fire-and-forget: a rejection
/// only means the browser wants a user gesture first, and the next gesture retries.
fn resume(context: &AudioContext) {
    if !is_resumable(context.state()) {
        return;
    }

    match context.resume() {
        Ok(promise) => wasm_bindgen_futures::spawn_local(async move {
            if let Err(error) = JsFuture::from(promise).await {
                debug!(?error, "AudioContext resume rejected");
            }
        }),
        Err(error) => debug!(?error, "AudioContext resume failed"),
    }
}

/// Gapless block timing on the `AudioContext` clock.
#[derive(Debug, Default)]
pub(crate) struct Schedule {
    /// Context time at which the queued audio runs out.
    next_start: Option<f64>,
    /// Set when the cap was hit; cleared once the queue is back under [`LOW_WATERMARK_SECS`].
    draining: bool,
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

        let queued = start - now;
        if self.draining && LOW_WATERMARK_SECS < queued {
            return None;
        }
        if MAX_QUEUED_SECS < queued {
            self.draining = true;
            return None;
        }
        self.draining = false;

        self.next_start = Some(start + duration);
        Some(start)
    }

    pub(crate) fn reset(&mut self) {
        self.next_start = None;
        self.draining = false;
    }
}

#[derive(Debug)]
pub(crate) struct Player {
    context: AudioContext,
    destination: AudioDestinationNode,
    schedule: Schedule,
    /// Whether playback already asked a suspended context to start. Later attempts come
    /// from [`GestureListener`], where browsers actually honor them.
    resume_attempted: bool,
    _gesture_listener: Option<GestureListener>,
}

/// Page-wide capture listener resuming the context on any user gesture, including ones
/// outside the session canvas. Removed on drop.
#[derive(Debug)]
struct GestureListener {
    window: Window,
    callback: Closure<dyn FnMut()>,
}

impl Drop for GestureListener {
    fn drop(&mut self) {
        for event in GESTURE_EVENTS {
            if let Err(error) = self.window.remove_event_listener_with_callback_and_bool(
                event,
                self.callback.as_ref().unchecked_ref(),
                true,
            ) {
                debug!(?error, event, "Failed to remove audio gesture listener");
            }
        }
    }
}

impl Player {
    pub(crate) fn new() -> anyhow::Result<Self> {
        let context = AudioContext::new().map_err(|error| anyhow!("create AudioContext: {error:?}"))?;

        let gesture_listener = web_sys::window().map(|window| {
            let gesture_context = context.clone();
            let callback = Closure::<dyn FnMut()>::new(move || resume(&gesture_context));
            for event in GESTURE_EVENTS {
                // Capture phase, so a canvas handler stopping propagation cannot hide the gesture.
                if let Err(error) =
                    window.add_event_listener_with_callback_and_bool(event, callback.as_ref().unchecked_ref(), true)
                {
                    warn!(?error, event, "Failed to install audio gesture listener");
                }
            }
            GestureListener { window, callback }
        });

        Ok(Self {
            destination: context.destination(),
            context,
            schedule: Schedule::default(),
            resume_attempted: false,
            _gesture_listener: gesture_listener,
        })
    }

    pub(crate) fn enqueue(&mut self, audio: &DecodedAudio) {
        // Autoplay policy: a context created without user activation starts suspended.
        // Ask once here (enough when the page already had a gesture, e.g. the Connect
        // click); the gesture listener retries otherwise.
        if !self.resume_attempted {
            self.resume_attempted = true;
            resume(&self.context);
        }

        // While suspended (or interrupted), the context clock is frozen: scheduling against
        // it would queue stale audio that plays whenever the context finally starts. Drop
        // the block and start fresh once it runs.
        if self.context.state() != AudioContextState::Running {
            self.schedule.reset();
            return;
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

        if let Err(error) = start_block(&self.context, &self.destination, audio, frames, when) {
            warn!(?error, "Failed to schedule RDPSND block");
        }

        fn start_block(
            context: &AudioContext,
            destination: &AudioDestinationNode,
            audio: &DecodedAudio,
            frames: u32,
            when: f64,
        ) -> Result<(), JsValue> {
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
            source.connect_with_audio_node(destination)?;
            source.start_with_when(when)?;
            Ok(())
        }
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
        assert!((second - (first + 0.02)).abs() < 1e-9);
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
    fn after_cap_queue_drains_to_low_watermark() {
        let mut schedule = Schedule::default();
        let mut now = 10.0;
        // Burst after a stall: fill the queue to the cap.
        while schedule.place(now, 0.02).is_some() {}
        // Blocks now arrive in real time. Without hysteresis the queue would sit at the cap forever.
        let mut accepted_lag = None;
        for _ in 0..100 {
            now += 0.02;
            if let Some(start) = schedule.place(now, 0.02) {
                accepted_lag = Some(start - now);
                break;
            }
        }
        let lag = accepted_lag.expect("queue drains and playback resumes");
        assert!(lag <= LOW_WATERMARK_SECS);
    }

    #[test]
    fn resumable_states() {
        assert!(is_resumable(AudioContextState::Suspended));
        assert!(!is_resumable(AudioContextState::Running));
        assert!(!is_resumable(AudioContextState::Closed));
    }

    #[test]
    fn reset_forgets_the_queue() {
        let mut schedule = Schedule::default();
        schedule.place(10.0, 0.4).expect("placed");
        schedule.reset();
        assert_eq!(schedule.place(10.0, 0.02), Some(10.0 + LEAD_SECS));
    }
}
