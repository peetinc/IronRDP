//! EGFX replay bench: drives the real graphics pipeline from a capture, flat out, and reports
//! the sustained frame rate. Captures are the rig's length-prefixed server messages.

use core::num::NonZeroU32;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;
use ironrdp::dvc::DvcProcessor as _;
use ironrdp::pdu::geometry::InclusiveRectangle;
use ironrdp_egfx::client::{GraphicsPipelineClient, GraphicsPipelineHandler};
use ironrdp_egfx::decode::{DecodedTiles, ProgressiveBackend, ProgressiveControl, ProgressiveJobError};
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

/// Nearest-rank percentiles of a set of times.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Percentiles {
    pub(crate) p50: f64,
    pub(crate) p95: f64,
    pub(crate) max: f64,
}

/// The p50, p95 and maximum of `values` (sorted in place); all zero for no values.
pub(crate) fn percentiles(values: &mut [f64]) -> Percentiles {
    values.sort_by(f64::total_cmp);
    let at = |fraction: f64| {
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "an index into a list far shorter than 2^52"
        )]
        let index = ((values.len().saturating_sub(1)) as f64 * fraction).round() as usize;
        values.get(index).copied().unwrap_or(0.0)
    };
    Percentiles {
        p50: at(0.5),
        p95: at(0.95),
        max: values.last().copied().unwrap_or(0.0),
    }
}

/// An in-thread decode of one job: packed pixels and placements, or an error code.
type ExpectedJob = Result<(Vec<u8>, Vec<i32>), u32>;

/// Wraps the Worker backend for `verify=workers`: decodes every job in-thread too, with the
/// same decoder the Worker runs, and compares the Worker's packed pixels and placements with
/// it byte for byte.
struct VerifyingBackend {
    inner: Box<dyn ProgressiveBackend>,
    shadow: ironrdp_rfx_worker::WorkerDecoder,
    expected: std::collections::BTreeMap<u32, ExpectedJob>,
    /// Jobs compared, and how many differed.
    counts: Arc<Mutex<(u32, u32)>>,
}

impl ProgressiveBackend for VerifyingBackend {
    fn max_in_flight(&self) -> usize {
        self.inner.max_in_flight()
    }

    fn submit(&mut self, job: u32, surface_id: u16, codec_context_id: u32, width: u16, height: u16, data: &[u8]) {
        let expected = self
            .shadow
            .submit(surface_id, codec_context_id, width, height, data)
            .map(|()| (self.shadow.pixels().to_vec(), self.shadow.placements().to_vec()));
        self.expected.insert(job, expected);
        self.inner
            .submit(job, surface_id, codec_context_id, width, height, data);
    }

    fn control(&mut self, control: ProgressiveControl) {
        let (kind, surface_id, codec_context_id) = match control {
            ProgressiveControl::StartFrame => (ironrdp_rfx_worker::CONTROL_START_FRAME, 0, 0),
            ProgressiveControl::EndFrame => (ironrdp_rfx_worker::CONTROL_END_FRAME, 0, 0),
            ProgressiveControl::DeleteContext {
                surface_id,
                codec_context_id,
            } => (ironrdp_rfx_worker::CONTROL_DELETE_CONTEXT, surface_id, codec_context_id),
            ProgressiveControl::DeleteSurface { surface_id } => {
                (ironrdp_rfx_worker::CONTROL_DELETE_SURFACE, surface_id, 0)
            }
        };
        self.shadow.control(kind, surface_id, codec_context_id);
        self.inner.control(control);
    }

    fn poll(&mut self, job: u32) -> Option<Result<DecodedTiles, ProgressiveJobError>> {
        let result = self.inner.poll(job)?;
        let expected = self.expected.remove(&job);
        let matches = match (&result, &expected) {
            (Ok(tiles), Some(Ok((pixels, placements)))) => tiles.external.is_some_and(|handle| {
                crate::rfx_worker::with_tiles(handle, |actual| {
                    let actual_pixels = Uint8Array::new_with_byte_offset_and_length(&actual.pixels, 0, actual.length);
                    actual.placements == crate::rfx_worker::placements(placements) && actual_pixels.to_vec() == *pixels
                })
                .unwrap_or(false)
            }),
            (Err(error), Some(Err(code))) => error.reason == ironrdp_rfx_worker::error_reason(*code),
            _ => false,
        };
        if let Ok(mut counts) = self.counts.lock() {
            counts.0 += 1;
            counts.1 += u32::from(!matches);
        }
        Some(result)
    }
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
    /// RemoteFX Progressive decodes in a Web Worker (GPU rendering only).
    workers: bool,
    /// With `workers`, every job is also decoded in-thread and compared.
    verify_workers: bool,
}

#[wasm_bindgen]
impl EgfxBench {
    /// A bench drawing to `canvas`. With `h264`, AVC frames decode through WebCodecs, as in a
    /// session with H.264 on; with `gpu`, EGFX renders through the WebGL2 backend; with
    /// `workers` (and `gpu`), RemoteFX Progressive decodes in a Web Worker, and with
    /// `verify_workers` every Worker job is compared with in-thread decoding.
    #[wasm_bindgen(js_name = create)]
    pub async fn create(
        canvas: HtmlCanvasElement,
        h264: bool,
        gpu: bool,
        workers: bool,
        verify_workers: bool,
    ) -> Result<EgfxBench, JsValue> {
        let h264 = h264 && crate::h264::probe_support().await;
        let presenter = if gpu {
            Presenter::Gl(crate::gpu::GlBackend::new(&canvas, 1, 1).map_err(|error| error.to_string())?)
        } else {
            Presenter::Canvas2d(
                Canvas::new(canvas, NonZeroU32::MIN, NonZeroU32::MIN).map_err(|error| error.to_string())?,
            )
        };
        Ok(Self {
            presenter,
            h264,
            workers: workers && gpu,
            verify_workers,
        })
    }

    /// Replays `capture` as fast as the pipeline allows and returns
    /// `{ messages, errors, frames, ms, fps, frameMsP50, frameMsP95, frameMsMax, decoderWaitMs,
    /// messageMsP50, messageMsP95, messageMsMax, messagesOver50, messagesOver150, uploadMs,
    /// uploadCalls, uploadMsMaxPerMessage, samples }`.
    ///
    /// A message's time is the main-thread time it costs, the bench's stand-in for one task
    /// in a session: from `process` to the end of its draws, less any wait for the decoder.
    /// Upload figures are the GL backend's surface uploads. With `sample_every` above zero, `samples` holds, after every
    /// `sample_every`th frame, every 8th pixel of every 8th row of the canvas as RGB.
    pub async fn run(&mut self, capture: Uint8Array, sample_every: u32) -> Result<JsValue, JsValue> {
        let capture = capture.to_vec();
        let messages = split_capture(&capture).map_err(JsValue::from_str)?;

        let frame_ends = Arc::new(Mutex::new(Vec::new()));
        let handler = BenchHandler {
            frame_ends: Arc::clone(&frame_ends),
        };
        let (wake_tx, mut wake_rx) = futures_channel::mpsc::unbounded::<()>();
        let worker_counts = Arc::new(Mutex::new((0u32, 0u32)));
        let progressive: Option<Box<dyn ProgressiveBackend>> = if self.workers {
            let wake_tx = wake_tx.clone();
            let backend = crate::rfx_worker::WorkerProgressiveBackend::new(move || {
                let _ = wake_tx.unbounded_send(());
            })
            .ok_or_else(|| JsValue::from_str("RemoteFX worker unavailable"))?;
            Some(if self.verify_workers {
                Box::new(VerifyingBackend {
                    inner: Box::new(backend),
                    shadow: ironrdp_rfx_worker::WorkerDecoder::new(),
                    expected: std::collections::BTreeMap::new(),
                    counts: Arc::clone(&worker_counts),
                })
            } else {
                Box::new(backend)
            })
        } else {
            None
        };
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
        if let Some(progressive) = progressive {
            client = client.with_progressive_backend(progressive);
        }
        client.start(0).map_err(|error| error.to_string())?;
        let samples = Array::new();
        let mut sampled_frames = 0;

        let (mut errors, mut decoder_wait_ms) = (0u32, 0.0);
        let mut message_ms = Vec::with_capacity(messages.len());
        let (mut upload_ms, mut upload_calls, mut upload_ms_max) = (0.0, 0u32, 0.0f64);
        crate::gpu::set_upload_timing(true);
        let _ = crate::gpu::take_upload_stats();
        let started = now();
        for (index, message) in messages.iter().enumerate() {
            let (message_started, wait_before) = (now(), decoder_wait_ms);
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
            message_ms.push(now() - message_started - (decoder_wait_ms - wait_before));
            let (ms, calls) = crate::gpu::take_upload_stats();
            upload_ms += ms;
            upload_calls += calls;
            upload_ms_max = upload_ms_max.max(ms);
            if let Some(reason) = self.presenter.failure() {
                crate::gpu::set_upload_timing(false);
                return Err(JsValue::from_str(reason));
            }
        }
        let ms = now() - started;
        crate::gpu::set_upload_timing(false);

        let frame_ends = frame_ends.lock().map(|ends| ends.clone()).unwrap_or_default();
        let mut frame_ms: Vec<f64> = frame_ends.windows(2).map(|pair| pair[1] - pair[0]).collect();
        let frame = percentiles(&mut frame_ms);
        let over = |limit: f64| message_ms.iter().filter(|ms| limit < **ms).count();
        let (over_budget, over_limit) = (over(50.0), over(150.0));
        let message = percentiles(&mut message_ms);
        let (worker_jobs, worker_mismatches) = worker_counts.lock().map(|counts| *counts).unwrap_or_default();

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
            ("frameMsP50", frame.p50),
            ("frameMsP95", frame.p95),
            ("frameMsMax", frame.max),
            ("decoderWaitMs", decoder_wait_ms),
            ("messageMsP50", message.p50),
            ("messageMsP95", message.p95),
            ("messageMsMax", message.max),
            ("messagesOver50", over_budget as f64),
            ("messagesOver150", over_limit as f64),
            ("uploadMs", upload_ms),
            ("uploadCalls", f64::from(upload_calls)),
            ("uploadMsMaxPerMessage", upload_ms_max),
            ("workerJobs", f64::from(worker_jobs)),
            ("workerMismatches", f64::from(worker_mismatches)),
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
    fn percentiles_of_nothing_are_zero() {
        assert_eq!(
            percentiles(&mut []),
            Percentiles {
                p50: 0.0,
                p95: 0.0,
                max: 0.0
            }
        );
    }

    #[test]
    fn percentiles_use_nearest_rank_on_sorted_values() {
        assert_eq!(
            percentiles(&mut [5.0, 1.0, 3.0, 2.0, 4.0]),
            Percentiles {
                p50: 3.0,
                p95: 5.0,
                max: 5.0
            }
        );
    }

    #[test]
    fn percentiles_ignore_input_order() {
        // 1..=100, in a scrambled order.
        let mut values: Vec<f64> = (0..100u32).map(|i| f64::from((i * 37) % 100 + 1)).collect();
        assert_eq!(
            percentiles(&mut values),
            Percentiles {
                p50: 51.0,
                p95: 95.0,
                max: 100.0
            }
        );
    }

    #[test]
    fn split_capture_rejects_a_truncated_message() {
        assert!(split_capture(&[3, 0, 0, 0, 0xAA]).is_err());
    }
}
