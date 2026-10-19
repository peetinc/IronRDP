//! EGFX replay bench: drives the real graphics pipeline from a capture, flat out, and reports
//! the sustained frame rate. Captures are the rig's length-prefixed server messages.

use core::num::NonZeroU32;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;
use ironrdp::dvc::DvcProcessor as _;
use ironrdp::pdu::geometry::InclusiveRectangle;
use ironrdp_egfx::client::{GraphicsPipelineClient, GraphicsPipelineHandler};
use js_sys::{Array, Object, Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

use crate::canvas::{Canvas, Presenter};

/// Splits a capture into its messages: each is a `u32` little-endian length, then the message.
fn split_capture(mut capture: &[u8]) -> Result<Vec<&[u8]>, &'static str> {
    let mut messages = Vec::new();
    while !capture.is_empty() {
        let (length, rest) = capture.split_first_chunk::<4>().ok_or("truncated message length")?;
        let length = usize::try_from(u32::from_le_bytes(*length)).map_err(|_| "message too long")?;
        if rest.len() < length {
            return Err("truncated message");
        }
        let (message, rest) = rest.split_at(length);
        messages.push(message);
        capture = rest;
    }
    Ok(messages)
}

/// Queued EGFX entries at which the replay waits for the decoder: twice the depth at which
/// suspended frame acknowledgements resume.
const BENCH_QUEUE_LIMIT: usize = 64;

fn now() -> f64 {
    web_sys::window()
        .and_then(|window| window.performance())
        .map_or(0.0, |performance| performance.now())
}

/// Records when each frame completes.
struct BenchHandler {
    frame_ends: Arc<Mutex<Vec<f64>>>,
}

impl GraphicsPipelineHandler for BenchHandler {
    fn on_frame_complete(&mut self, _frame_id: u32) {
        if let Ok(mut frame_ends) = self.frame_ends.lock() {
            frame_ends.push(now());
        }
    }
}

/// Replays a capture through [`GraphicsPipelineClient`] and the session's renderer.
#[wasm_bindgen]
pub struct EgfxBench {
    presenter: Presenter,
    h264: bool,
}

#[wasm_bindgen]
impl EgfxBench {
    /// A bench drawing to `canvas`. With `h264`, AVC frames decode through WebCodecs, as in a
    /// session with H.264 on; with `gpu`, EGFX renders through the WebGL2 backend.
    #[wasm_bindgen(js_name = create)]
    pub async fn create(canvas: HtmlCanvasElement, h264: bool, gpu: bool) -> Result<EgfxBench, JsValue> {
        let h264 = h264 && crate::h264::probe_support().await;
        let presenter = if gpu {
            Presenter::Gl(crate::gpu::GlBackend::new(&canvas, 1, 1).map_err(|error| error.to_string())?)
        } else {
            Presenter::Canvas2d(
                Canvas::new(canvas, NonZeroU32::MIN, NonZeroU32::MIN).map_err(|error| error.to_string())?,
            )
        };
        Ok(Self { presenter, h264 })
    }

    /// Replays `capture` as fast as the pipeline allows and returns
    /// `{ messages, errors, frames, ms, fps, frameMsP50, frameMsP95, frameMsMax, decoderWaitMs,
    /// samples }`. With `sample_every` above zero, `samples` holds, after every
    /// `sample_every`th frame, every 8th pixel of every 8th row of the canvas as RGB.
    pub async fn run(&mut self, capture: Uint8Array, sample_every: u32) -> Result<JsValue, JsValue> {
        let capture = capture.to_vec();
        let messages = split_capture(&capture).map_err(JsValue::from_str)?;

        let frame_ends = Arc::new(Mutex::new(Vec::new()));
        let handler = BenchHandler {
            frame_ends: Arc::clone(&frame_ends),
        };
        let (wake_tx, mut wake_rx) = futures_channel::mpsc::unbounded::<()>();
        let decoder: Option<Box<dyn ironrdp_egfx::decode::H264Decoder>> = if self.h264 {
            Some(Box::new(crate::h264::WebCodecsH264Decoder::new(
                move || {
                    let _ = wake_tx.unbounded_send(());
                },
                if matches!(self.presenter, Presenter::Gl(_)) {
                    crate::h264::FrameOutput::Gpu
                } else {
                    crate::h264::FrameOutput::Readback
                },
            )))
        } else {
            None
        };
        // The policy a session uses, so the bench exercises its acknowledgement path too.
        let mut client = GraphicsPipelineClient::new(Box::new(handler), decoder).with_frame_ack_policy(
            ironrdp_egfx::client::FrameAckPolicy::SuspendWhenCaughtUp,
            crate::clock_ms,
        );
        if let Presenter::Gl(gl) = &self.presenter {
            client = client.with_surface_backend(Box::new(gl.clone()));
        }
        client.start(0).map_err(|error| error.to_string())?;
        let samples = Array::new();
        let mut sampled_frames = 0;

        let (mut errors, mut decoder_wait_ms) = (0u32, 0.0);
        let started = now();
        for (index, message) in messages.iter().enumerate() {
            if client.process(0, message).is_err() {
                errors += 1;
            }
            // Like a session: frames that finished decoding apply as their wakes arrive,
            // while messages keep coming. Waits only when the queue is deep (the server's
            // flow control would hold it about there), and at the end.
            let last = index + 1 == messages.len();
            loop {
                while wake_rx.try_recv().is_ok() {
                    if client.resume().is_err() {
                        errors += 1;
                    }
                }
                if !client.is_awaiting_decoder() || (!last && client.queued_len() < BENCH_QUEUE_LIMIT) {
                    break;
                }
                let waited = now();
                if wake_rx.next().await.is_none() {
                    break;
                }
                decoder_wait_ms += now() - waited;
                if client.resume().is_err() {
                    errors += 1;
                }
            }
            if let Some((width, height)) = client.take_output_reset()
                && let (Some(width), Some(height)) =
                    (NonZeroU32::new(u32::from(width)), NonZeroU32::new(u32::from(height)))
            {
                self.presenter.resize(width, height);
            }
            for mut update in client.drain_output() {
                let region = InclusiveRectangle {
                    left: update.region.left,
                    top: update.region.top,
                    right: update.region.right.saturating_sub(1),
                    bottom: update.region.bottom.saturating_sub(1),
                };
                self.presenter
                    .draw(&mut update.data, region)
                    .map_err(|error| error.to_string())?;
            }
            if 0 < sample_every {
                let frames = frame_ends.lock().map(|ends| ends.len()).unwrap_or(0);
                if sampled_frames + usize::try_from(sample_every).unwrap_or(usize::MAX) <= frames {
                    sampled_frames = frames;
                    let sample = match &self.presenter {
                        Presenter::Gl(gl) => gl.sample_output(8),
                        Presenter::Canvas2d(canvas) => canvas.sample(8),
                    };
                    samples.push(&Uint8Array::from(sample.as_slice()));
                }
            }
            if let Some(reason) = self.presenter.failure() {
                return Err(JsValue::from_str(reason));
            }
        }
        let ms = now() - started;

        let frame_ends = frame_ends.lock().map(|ends| ends.clone()).unwrap_or_default();
        let mut frame_ms: Vec<f64> = frame_ends.windows(2).map(|pair| pair[1] - pair[0]).collect();
        frame_ms.sort_by(f64::total_cmp);
        let percentile = |fraction: f64| {
            #[expect(
                clippy::as_conversions,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss,
                reason = "an index into a short list of frame times"
            )]
            let index = ((frame_ms.len().saturating_sub(1)) as f64 * fraction).round() as usize;
            frame_ms.get(index).copied().unwrap_or(0.0)
        };

        let result = Object::new();
        let frames = frame_ends.len();
        #[expect(
            clippy::as_conversions,
            clippy::cast_precision_loss,
            reason = "counts well below 2^52"
        )]
        let fields = [
            ("messages", messages.len() as f64),
            ("errors", f64::from(errors)),
            ("frames", frames as f64),
            ("ms", ms),
            ("fps", if 0.0 < ms { frames as f64 * 1000.0 / ms } else { 0.0 }),
            ("frameMsP50", percentile(0.5)),
            ("frameMsP95", percentile(0.95)),
            ("frameMsMax", frame_ms.last().copied().unwrap_or(0.0)),
            ("decoderWaitMs", decoder_wait_ms),
        ];
        for (key, value) in fields {
            Reflect::set(&result, &JsValue::from_str(key), &JsValue::from_f64(value))?;
        }
        Reflect::set(&result, &JsValue::from_str("samples"), &samples)?;
        Ok(result.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_capture_reads_length_prefixed_messages() {
        let capture = [2, 0, 0, 0, 0xAA, 0xBB, 1, 0, 0, 0, 0xCC];

        let messages = split_capture(&capture).unwrap();

        assert_eq!(messages, vec![&[0xAA, 0xBB][..], &[0xCC][..]]);
    }

    #[test]
    fn split_capture_rejects_a_truncated_message() {
        assert!(split_capture(&[3, 0, 0, 0, 0xAA]).is_err());
    }
}
