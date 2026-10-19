//! H.264 (AVC420) decoding through the browser's WebCodecs `VideoDecoder`.
//!
//! WebCodecs decodes asynchronously, in hardware where the browser can, so this decoder uses the
//! EGFX client's deferred path: [`H264Decoder::submit`] queues a chunk and returns
//! [`H264Decode::Pending`], the output callback reads the changed regions back into an RGBA
//! frame, and the session loop is woken to call `ActiveStage::resume_graphics_pipeline`.
//!
//! `H264Decoder` must be `Send` and JS objects are not, so [`WebCodecsH264Decoder`] is only a
//! handle: its JS state lives in a thread-local slot keyed by the handle, one per session.

use core::cell::{Cell, RefCell};
use std::borrow::Cow;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use ironrdp::pdu::geometry::ExclusiveRectangle;
use ironrdp_egfx::decode::{DecodedFrame, DecoderError, DecoderResult, H264Decode, H264Decoder};
use js_sys::{Object, Reflect, Uint8Array};
use tracing::{debug, info, warn};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// Codec probed for support: High profile, level 5.2 (4K at 60 fps).
const PROBE_CODEC: &str = "avc1.640034";

/// A decode slower than this is logged: the EGFX pipeline waits on every frame.
const SLOW_DECODE_MS: f64 = 500.0;

/// Pictures the EGFX client may have decoding at once. Hardware decoders have few output
/// buffers, and the client holds each decoded frame until it applies.
const MAX_IN_FLIGHT: usize = 4;

/// NAL `nal_unit_type` values ([ITU-T H.264] Table 7-1).
///
/// [ITU-T H.264]: https://www.itu.int/rec/T-REC-H.264
const NAL_IDR_SLICE: u8 = 5;
const NAL_SPS: u8 = 7;

/// NAL units of an Annex B byte stream, without their start codes.
pub(crate) fn nal_units(stream: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = find_start_code(stream).and_then(|at| stream.get(at + 3..));
    core::iter::from_fn(move || {
        let current = rest?;
        let nal = match find_start_code(current) {
            Some(at) => {
                rest = current.get(at + 3..);
                current.get(..at).unwrap_or_default()
            }
            None => {
                rest = None;
                current
            }
        };
        // A 4-byte start code (or trailing_zero_8bits) leaves zeros on the previous unit; a NAL
        // unit itself never ends in a zero byte (rbsp_trailing_bits).
        let end = nal.iter().rposition(|&byte| byte != 0).map_or(0, |last| last + 1);
        Some(nal.get(..end).unwrap_or_default())
    })
    .filter(|nal| !nal.is_empty())
}

/// Offset of the next `00 00 01` start code prefix.
fn find_start_code(data: &[u8]) -> Option<usize> {
    data.windows(3).position(|window| window == [0, 0, 1])
}

/// `data` as an Annex B byte stream. AVC-format input (4-byte big-endian length before each
/// NAL unit) is rewritten with start codes; anything else is passed through.
pub(crate) fn to_annex_b(data: &[u8]) -> Cow<'_, [u8]> {
    if data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1]) {
        return Cow::Borrowed(data);
    }

    let mut converted = Vec::with_capacity(data.len());
    let mut rest = data;
    while !rest.is_empty() {
        let Some((length, after)) = rest.split_first_chunk::<4>() else {
            return Cow::Borrowed(data);
        };
        let Ok(length) = usize::try_from(u32::from_be_bytes(*length)) else {
            return Cow::Borrowed(data);
        };
        if length == 0 || after.len() < length {
            return Cow::Borrowed(data);
        }
        let (nal, after) = after.split_at(length);
        converted.extend_from_slice(&[0, 0, 0, 1]);
        converted.extend_from_slice(nal);
        rest = after;
    }
    Cow::Owned(converted)
}

fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|header| header & 0x1F)
}

/// WebCodecs codec string (`avc1.PPCCLL`) from an Annex B stream's sequence parameter set, if
/// it has one.
pub(crate) fn codec_string(stream: &[u8]) -> Option<String> {
    nal_units(stream)
        .find(|nal| nal_type(nal) == Some(NAL_SPS))
        .and_then(|sps| match sps {
            [_, profile, constraints, level, ..] => Some(format!("avc1.{profile:02X}{constraints:02X}{level:02X}")),
            _ => None,
        })
}

/// Whether an Annex B access unit holds an IDR slice, which WebCodecs calls a `key` chunk.
pub(crate) fn is_key(stream: &[u8]) -> bool {
    nal_units(stream).any(|nal| nal_type(nal) == Some(NAL_IDR_SLICE))
}

/// Whether this browser can decode H.264 through WebCodecs.
pub(crate) async fn probe_support() -> bool {
    if !Reflect::has(&js_sys::global(), &JsValue::from_str("VideoDecoder")).unwrap_or(false) {
        return false;
    }
    let answer = match VideoDecoder::is_config_supported(&decoder_config(PROBE_CODEC)) {
        Ok(promise) => JsFuture::from(promise).await,
        Err(error) => Err(error),
    };
    match answer.and_then(|support| Reflect::get(&support, &JsValue::from_str("supported"))) {
        Ok(supported) => supported.is_truthy(),
        Err(error) => {
            debug!(?error, "H.264 support probe failed");
            false
        }
    }
}

thread_local! {
    static SLOTS: RefCell<BTreeMap<u32, Slot>> = const { RefCell::new(BTreeMap::new()) };
    static NEXT_SLOT: Cell<u32> = const { Cell::new(0) };
}

/// Where a decoded picture goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FrameOutput {
    /// Read back to the CPU as RGBA, for the CPU compositor.
    Readback,
    /// Kept on the GPU for the WebGL2 backend, as an external [`DecodedFrame`].
    Gpu,
}

/// [`H264Decoder`] backed by a WebCodecs `VideoDecoder`; a handle to its thread-local slot.
pub(crate) struct WebCodecsH264Decoder {
    slot: u32,
}

impl WebCodecsH264Decoder {
    /// `wake` is called whenever a submitted frame is ready for `poll_frame`.
    pub(crate) fn new(wake: impl Fn() + 'static, output: FrameOutput) -> Self {
        let slot = NEXT_SLOT.with(|next| {
            let slot = next.get();
            next.set(slot.wrapping_add(1));
            slot
        });
        SLOTS.with(|slots| slots.borrow_mut().insert(slot, Slot::new(slot, Rc::new(wake), output)));
        Self { slot }
    }

    fn with_slot<R>(&self, f: impl FnOnce(&mut Slot) -> R) -> Option<R> {
        SLOTS.with(|slots| slots.borrow_mut().get_mut(&self.slot).map(f))
    }
}

impl Drop for WebCodecsH264Decoder {
    fn drop(&mut self) {
        let slot = SLOTS.with(|slots| slots.borrow_mut().remove(&self.slot));
        if let Some(mut slot) = slot {
            slot.close();
        }
        crate::gpu::release_frames(self.slot);
    }
}

impl H264Decoder for WebCodecsH264Decoder {
    fn decode(&mut self, _data: &[u8]) -> DecoderResult<DecodedFrame> {
        Err(DecoderError::msg("WebCodecs decodes asynchronously; use submit"))
    }

    /// Always [`H264Decode::Pending`]: a frame that cannot be decoded (no key frame yet, a
    /// decoder error) comes back as an error from `poll_frame`, which drops just that frame.
    fn submit(&mut self, data: &[u8], regions: &[ExclusiveRectangle]) -> DecoderResult<H264Decode> {
        self.with_slot(|slot| slot.submit(data, regions))
            .ok_or_else(|| DecoderError::msg("H.264 decoder slot is gone"))?;
        Ok(H264Decode::Pending)
    }

    /// Decodes an AVC444 chroma view into the same `VideoDecoder`, discarding its picture. A
    /// view that cannot be decoded yet (before the first key frame) is skipped.
    fn submit_reference(&mut self, data: &[u8]) -> DecoderResult<()> {
        self.with_slot(|slot| {
            if let Err(error) = slot.decode_chunk(data, false) {
                debug!(%error, "Skipping an H.264 reference picture");
            }
        })
        .ok_or_else(|| DecoderError::msg("H.264 decoder slot is gone"))
    }

    fn poll_frame(&mut self) -> Option<DecoderResult<DecodedFrame>> {
        self.with_slot(|slot| slot.shared.borrow_mut().in_flight.pop_ready())
            .flatten()
    }

    fn max_in_flight(&self) -> usize {
        MAX_IN_FLIGHT
    }

    fn reset(&mut self) {
        self.with_slot(Slot::close);
    }
}

/// The JS side of one decoder.
struct Slot {
    decoder: Option<ActiveDecoder>,
    shared: Rc<RefCell<Shared>>,
    wake: Rc<dyn Fn()>,
    /// Chunk timestamps must increase; their values are otherwise unused.
    timestamp: f64,
}

/// State the WebCodecs callbacks share with the slot.
struct Shared {
    /// Shown pictures submitted and not yet returned to the EGFX client, oldest first.
    in_flight: InFlight,
    readback: Option<Readback>,
    output: FrameOutput,
    /// The decoder's slot, which owns the frames it holds for the GPU backend.
    owner: u32,
    /// Per decoded chunk, oldest first: whether its picture is shown (`false` for AVC444
    /// chroma views). WebCodecs outputs pictures in decode order.
    shown: VecDeque<bool>,
}

struct ActiveDecoder {
    decoder: VideoDecoder,
    codec: String,
    /// Set until a key chunk is decoded: WebCodecs rejects delta chunks after `configure`.
    awaiting_key: bool,
    failed: Rc<Cell<bool>>,
    _on_output: Closure<dyn FnMut(VideoFrame)>,
    _on_error: Closure<dyn FnMut(JsValue)>,
}

/// Shown pictures submitted to the decoder and not yet returned, in submission order, which
/// is the order WebCodecs outputs them in.
#[derive(Default)]
struct InFlight {
    entries: VecDeque<InFlightEntry>,
}

struct InFlightEntry {
    /// `regionRects` of the picture: the only pixels read back.
    regions: Vec<ExclusiveRectangle>,
    submitted_at: f64,
    /// `None` while decoding.
    result: Option<DecoderResult<DecodedFrame>>,
}

impl InFlight {
    fn push(&mut self, regions: &[ExclusiveRectangle], submitted_at: f64) {
        self.entries.push_back(InFlightEntry {
            regions: regions.to_vec(),
            submitted_at,
            result: None,
        });
    }

    /// A submission that failed at once; its error is returned in its place.
    fn push_failed(&mut self, error: DecoderError) {
        self.entries.push_back(InFlightEntry {
            regions: Vec::new(),
            submitted_at: 0.0,
            result: Some(Err(error)),
        });
    }

    /// The regions and submit time of the oldest picture still decoding.
    fn next_waiting(&self) -> Option<(&[ExclusiveRectangle], f64)> {
        self.entries
            .iter()
            .find(|entry| entry.result.is_none())
            .map(|entry| (entry.regions.as_slice(), entry.submitted_at))
    }

    /// Completes the oldest picture still decoding; `false` when none is.
    fn fill_next(&mut self, result: DecoderResult<DecodedFrame>) -> bool {
        match self.entries.iter_mut().find(|entry| entry.result.is_none()) {
            Some(entry) => {
                entry.result = Some(result);
                true
            }
            None => false,
        }
    }

    /// Fails every picture still decoding, returning how many.
    fn fail_waiting(&mut self, message: &str) -> usize {
        let mut failed = 0;
        for entry in self.entries.iter_mut().filter(|entry| entry.result.is_none()) {
            entry.result = Some(Err(DecoderError::msg(message)));
            failed += 1;
        }
        failed
    }

    /// The oldest entry's result, once it has one.
    fn pop_ready(&mut self) -> Option<DecoderResult<DecodedFrame>> {
        if self.entries.front()?.result.is_none() {
            return None;
        }
        self.entries.pop_front().and_then(|entry| entry.result)
    }
}

/// Canvas a `VideoFrame` is drawn to so its pixels can be read back as RGBA.
struct Readback {
    canvas: OffscreenCanvas,
    context: OffscreenContext2d,
}

impl Slot {
    fn new(owner: u32, wake: Rc<dyn Fn()>, output: FrameOutput) -> Self {
        Self {
            decoder: None,
            shared: Rc::new(RefCell::new(Shared {
                in_flight: InFlight::default(),
                readback: None,
                output,
                owner,
                shown: VecDeque::new(),
            })),
            wake,
            timestamp: 0.0,
        }
    }

    fn submit(&mut self, data: &[u8], regions: &[ExclusiveRectangle]) {
        // WebCodecs never outputs during `decode`, so the entry can follow the chunk.
        match self.decode_chunk(data, true) {
            Ok(()) => self.shared.borrow_mut().in_flight.push(regions, js_sys::Date::now()),
            Err(error) => {
                self.shared.borrow_mut().in_flight.push_failed(error);
                (self.wake)();
            }
        }
    }

    /// Queues one access unit; `shown` says whether its picture is read back when it decodes.
    fn decode_chunk(&mut self, data: &[u8], shown: bool) -> DecoderResult<()> {
        let stream = to_annex_b(data);
        let key = is_key(&stream);

        if self
            .decoder
            .as_ref()
            .is_some_and(|active| active.failed.get() || active.decoder.state() == "closed")
        {
            self.close();
        }

        match (self.decoder.as_mut(), codec_string(&stream)) {
            (Some(active), Some(codec)) if active.codec != codec => {
                info!(%codec, "Reconfiguring the H.264 decoder for a new sequence parameter set");
                active
                    .decoder
                    .configure(&decoder_config(&codec))
                    .map_err(|error| js_error("configure VideoDecoder", &error))?;
                active.codec = codec;
                active.awaiting_key = true;
            }
            (Some(_), _) => {}
            (None, Some(codec)) => {
                self.decoder = Some(ActiveDecoder::new(codec, &self.shared, &self.wake)?);
            }
            (None, None) => return Err(DecoderError::msg("waiting for an H.264 sequence parameter set")),
        }
        let Some(active) = self.decoder.as_mut() else {
            return Err(DecoderError::msg("no H.264 decoder"));
        };
        if active.awaiting_key && !key {
            return Err(DecoderError::msg("waiting for an H.264 key frame"));
        }

        let init = Object::new();
        set(&init, "type", &JsValue::from_str(if key { "key" } else { "delta" }))?;
        set(&init, "timestamp", &JsValue::from_f64(self.timestamp))?;
        set(&init, "data", &Uint8Array::from(stream.as_ref()))?;
        let chunk = EncodedVideoChunk::new(&init).map_err(|error| js_error("create EncodedVideoChunk", &error))?;

        active
            .decoder
            .decode(&chunk)
            .map_err(|error| js_error("decode H.264 chunk", &error))?;
        self.shared.borrow_mut().shown.push_back(shown);
        active.awaiting_key = false;
        self.timestamp += 1.0;
        Ok(())
    }

    fn close(&mut self) {
        // A closed decoder outputs nothing more, so the pictures still decoding never arrive.
        let failed = {
            let mut shared = self.shared.borrow_mut();
            shared.shown.clear();
            shared.in_flight.fail_waiting("H.264 decoder closed")
        };
        if 0 < failed {
            (self.wake)();
        }
        if let Some(active) = self.decoder.take()
            && active.decoder.state() != "closed"
            && let Err(error) = active.decoder.close()
        {
            debug!(?error, "Failed to close VideoDecoder");
        }
    }
}

impl ActiveDecoder {
    fn new(codec: String, shared: &Rc<RefCell<Shared>>, wake: &Rc<dyn Fn()>) -> DecoderResult<Self> {
        let on_output = {
            let shared = Rc::clone(shared);
            let wake = Rc::clone(wake);
            Closure::<dyn FnMut(VideoFrame)>::new(move |frame: VideoFrame| {
                if !shared.borrow_mut().shown.pop_front().unwrap_or(true) {
                    frame.close();
                    return;
                }
                {
                    let mut shared = shared.borrow_mut();
                    let Some((regions, submitted_at)) = shared
                        .in_flight
                        .next_waiting()
                        .map(|(regions, submitted_at)| (regions.to_vec(), submitted_at))
                    else {
                        // Its entry was already failed (the decoder closed): nothing waits for it.
                        frame.close();
                        return;
                    };
                    let latency = js_sys::Date::now() - submitted_at;
                    if SLOW_DECODE_MS < latency {
                        warn!(latency, "Slow H.264 decode");
                    }
                    if shared.output == FrameOutput::Gpu {
                        let (width, height) = (frame.display_width(), frame.display_height());
                        let handle = crate::gpu::hold_frame(shared.owner, frame);
                        shared
                            .in_flight
                            .fill_next(Ok(DecodedFrame::external(handle, width, height)));
                        drop(shared);
                        wake();
                        return;
                    }
                    let result = shared.read_back(&frame, &regions);
                    shared.in_flight.fill_next(result);
                }
                frame.close();
                wake();
            })
        };

        let failed = Rc::new(Cell::new(false));
        let on_error = {
            let shared = Rc::clone(shared);
            let wake = Rc::clone(wake);
            let failed = Rc::clone(&failed);
            Closure::<dyn FnMut(JsValue)>::new(move |error: JsValue| {
                warn!(?error, "H.264 decoder error");
                failed.set(true);
                shared.borrow_mut().in_flight.fail_waiting("H.264 decoder error");
                wake();
            })
        };

        let init = Object::new();
        set(&init, "output", on_output.as_ref())?;
        set(&init, "error", on_error.as_ref())?;
        let decoder = VideoDecoder::new(&init).map_err(|error| js_error("create VideoDecoder", &error))?;
        decoder
            .configure(&decoder_config(&codec))
            .map_err(|error| js_error("configure VideoDecoder", &error))?;
        info!(%codec, "Configured the WebCodecs H.264 decoder");

        Ok(Self {
            decoder,
            codec,
            awaiting_key: true,
            failed,
            _on_output: on_output,
            _on_error: on_error,
        })
    }
}

impl Shared {
    /// Reads a frame's changed `regions` (or the whole frame, if it listed none) into a
    /// frame-sized RGBA buffer. The rest of the buffer stays zero: the client copies only the
    /// regions.
    fn read_back(&mut self, frame: &VideoFrame, regions: &[ExclusiveRectangle]) -> DecoderResult<DecodedFrame> {
        let (width, height) = (frame.display_width(), frame.display_height());
        let readback = match self.readback.take() {
            Some(readback) if readback.canvas.width() == width && readback.canvas.height() == height => readback,
            _ => Readback::new(width, height)?,
        };
        let result = read_regions(&readback.context, frame, width, height, regions);
        self.readback = Some(readback);
        result
    }
}

impl Readback {
    fn new(width: u32, height: u32) -> DecoderResult<Self> {
        let canvas = OffscreenCanvas::new(width, height).map_err(|error| js_error("create OffscreenCanvas", &error))?;
        let options = Object::new();
        // GPU-backed (no `willReadFrequently`): drawing the frame stays on the GPU, and only the
        // changed area is read back. A CPU canvas costs a full-frame readback and colour
        // conversion per draw, ~28 ms at 4K.
        set(&options, "willReadFrequently", &JsValue::FALSE)?;
        let context = canvas
            .get_context("2d", &options)
            .map_err(|error| js_error("get OffscreenCanvas 2d context", &error))?;
        if context.is_null() {
            return Err(DecoderError::msg("OffscreenCanvas has no 2d context"));
        }
        Ok(Self {
            canvas,
            context: context.unchecked_into(),
        })
    }
}

fn read_regions(
    context: &OffscreenContext2d,
    frame: &VideoFrame,
    width: u32,
    height: u32,
    regions: &[ExclusiveRectangle],
) -> DecoderResult<DecodedFrame> {
    context
        .draw_video_frame(frame, 0.0, 0.0)
        .map_err(|error| js_error("draw VideoFrame", &error))?;

    let stride = usize::try_from(width).map_err(|_| DecoderError::msg("frame too wide"))? * 4;
    let rows = usize::try_from(height).map_err(|_| DecoderError::msg("frame too tall"))?;
    let mut rgba = vec![0; stride * rows];

    let whole = [ExclusiveRectangle {
        left: 0,
        top: 0,
        right: u16::try_from(width).unwrap_or(u16::MAX),
        bottom: u16::try_from(height).unwrap_or(u16::MAX),
    }];
    let regions = if regions.is_empty() { &whole[..] } else { regions };

    // Clipped to the frame, as (left, top, right, bottom).
    let clipped: Vec<(u32, u32, u32, u32)> = regions
        .iter()
        .map(|region| {
            (
                u32::from(region.left),
                u32::from(region.top),
                u32::from(region.right).min(width),
                u32::from(region.bottom).min(height),
            )
        })
        .filter(|&(left, top, right, bottom)| left < right && top < bottom)
        .collect();
    let Some(bounds) = clipped
        .iter()
        .copied()
        .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)))
    else {
        return Ok(DecodedFrame::new(rgba, width, height));
    };

    // One readback of the regions' bounding box: each `getImageData` on a GPU canvas is a
    // separate GPU sync, and Windows sends ~100 small regions per frame.
    let image = context
        .get_image_data(
            f64::from(bounds.0),
            f64::from(bounds.1),
            f64::from(bounds.2 - bounds.0),
            f64::from(bounds.3 - bounds.1),
        )
        .map_err(|error| js_error("read back VideoFrame", &error))?;
    let pixels = image.data();
    let to_usize = |value: u32| usize::try_from(value).map_err(|_| DecoderError::msg("frame too large"));
    let bounds_stride = to_usize(bounds.2 - bounds.0)? * 4;

    for (left, top, right, bottom) in clipped {
        let row_bytes = to_usize(right - left)? * 4;
        let (left, top, bottom) = (to_usize(left)?, to_usize(top)?, to_usize(bottom)?);
        let (bounds_left, bounds_top) = (to_usize(bounds.0)?, to_usize(bounds.1)?);
        for y in top..bottom {
            let source_start = (y - bounds_top) * bounds_stride + (left - bounds_left) * 4;
            let destination_start = y * stride + left * 4;
            if let (Some(source), Some(destination)) = (
                pixels.get(source_start..source_start + row_bytes),
                rgba.get_mut(destination_start..destination_start + row_bytes),
            ) {
                destination.copy_from_slice(source);
            }
        }
    }

    Ok(DecodedFrame::new(rgba, width, height))
}

fn decoder_config(codec: &str) -> Object {
    let config = Object::new();
    // Setting a property on a fresh plain object cannot fail.
    let _ = Reflect::set(&config, &JsValue::from_str("codec"), &JsValue::from_str(codec));
    // Output each frame as soon as it decodes: the EGFX pipeline waits on it.
    let _ = Reflect::set(&config, &JsValue::from_str("optimizeForLatency"), &JsValue::TRUE);
    config
}

fn set(target: &Object, key: &str, value: &JsValue) -> DecoderResult<()> {
    Reflect::set(target, &JsValue::from_str(key), value)
        .map(drop)
        .map_err(|error| js_error(key, &error))
}

fn js_error(context: &str, error: &JsValue) -> DecoderError {
    DecoderError::msg(format!("{context}: {error:?}"))
}

// Hand-written WebCodecs and OffscreenCanvas bindings: `web-sys` only exposes the WebCodecs
// types behind `--cfg=web_sys_unstable_apis`, and `drawImage` with a `VideoFrame` with them.
#[wasm_bindgen]
extern "C" {
    type VideoDecoder;

    #[wasm_bindgen(constructor, catch)]
    fn new(init: &Object) -> Result<VideoDecoder, JsValue>;

    #[wasm_bindgen(static_method_of = VideoDecoder, js_name = isConfigSupported, catch)]
    fn is_config_supported(config: &Object) -> Result<js_sys::Promise, JsValue>;

    #[wasm_bindgen(method, catch)]
    fn configure(this: &VideoDecoder, config: &Object) -> Result<(), JsValue>;

    #[wasm_bindgen(method, catch)]
    fn decode(this: &VideoDecoder, chunk: &EncodedVideoChunk) -> Result<(), JsValue>;

    #[wasm_bindgen(method, catch)]
    fn close(this: &VideoDecoder) -> Result<(), JsValue>;

    #[wasm_bindgen(method, getter)]
    fn state(this: &VideoDecoder) -> String;

    type EncodedVideoChunk;

    #[wasm_bindgen(constructor, catch)]
    fn new(init: &Object) -> Result<EncodedVideoChunk, JsValue>;

    pub(crate) type VideoFrame;

    #[wasm_bindgen(method, getter, js_name = displayWidth)]
    pub(crate) fn display_width(this: &VideoFrame) -> u32;

    #[wasm_bindgen(method, getter, js_name = displayHeight)]
    pub(crate) fn display_height(this: &VideoFrame) -> u32;

    #[wasm_bindgen(method)]
    pub(crate) fn close(this: &VideoFrame);

    type OffscreenCanvas;

    #[wasm_bindgen(constructor, catch)]
    fn new(width: u32, height: u32) -> Result<OffscreenCanvas, JsValue>;

    #[wasm_bindgen(method, getter)]
    fn width(this: &OffscreenCanvas) -> u32;

    #[wasm_bindgen(method, getter)]
    fn height(this: &OffscreenCanvas) -> u32;

    #[wasm_bindgen(method, js_name = getContext, catch)]
    fn get_context(this: &OffscreenCanvas, kind: &str, options: &Object) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(js_name = OffscreenCanvasRenderingContext2D)]
    type OffscreenContext2d;

    #[wasm_bindgen(method, js_name = drawImage, catch)]
    fn draw_video_frame(this: &OffscreenContext2d, frame: &VideoFrame, dx: f64, dy: f64) -> Result<(), JsValue>;

    #[wasm_bindgen(method, js_name = getImageData, catch)]
    fn get_image_data(
        this: &OffscreenContext2d,
        sx: f64,
        sy: f64,
        sw: f64,
        sh: f64,
    ) -> Result<web_sys::ImageData, JsValue>;
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x33, 0xAC, 0xD9];
    const PPS: &[u8] = &[0x68, 0xEB, 0xE3, 0xCB];
    const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x21];
    const P_SLICE: &[u8] = &[0x41, 0x9A, 0x02];

    fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
        let mut stream = Vec::new();
        for (index, nal) in nals.iter().enumerate() {
            // Both start code lengths appear in the wild.
            if index % 2 == 0 {
                stream.extend_from_slice(&[0, 0, 0, 1]);
            } else {
                stream.extend_from_slice(&[0, 0, 1]);
            }
            stream.extend_from_slice(nal);
        }
        stream
    }

    fn length_prefixed(nals: &[&[u8]]) -> Vec<u8> {
        let mut stream = Vec::new();
        for nal in nals {
            stream.extend_from_slice(&u32::try_from(nal.len()).expect("small").to_be_bytes());
            stream.extend_from_slice(nal);
        }
        stream
    }

    fn rect(left: u16) -> ExclusiveRectangle {
        ExclusiveRectangle {
            left,
            top: 0,
            right: left + 1,
            bottom: 1,
        }
    }

    fn frame(value: u8) -> DecodedFrame {
        DecodedFrame::new(vec![value; 4], 1, 1)
    }

    fn frame_value(result: Option<DecoderResult<DecodedFrame>>) -> Result<u8, String> {
        match result {
            Some(Ok(frame)) => Ok(frame.data().first().copied().unwrap_or_default()),
            Some(Err(error)) => Err(error.to_string()),
            None => Err("pending".to_owned()),
        }
    }

    #[test]
    fn in_flight_entries_complete_in_submission_order() {
        let mut in_flight = InFlight::default();
        in_flight.push(&[rect(1)], 0.0);
        in_flight.push(&[rect(2)], 0.0);

        assert_eq!(
            in_flight.next_waiting().map(|(regions, _)| regions.to_vec()),
            Some(vec![rect(1)])
        );
        assert!(in_flight.fill_next(Ok(frame(1))));
        assert_eq!(
            in_flight.next_waiting().map(|(regions, _)| regions.to_vec()),
            Some(vec![rect(2)])
        );

        assert_eq!(frame_value(in_flight.pop_ready()), Ok(1));
        assert_eq!(frame_value(in_flight.pop_ready()), Err("pending".to_owned()));
        assert!(in_flight.fill_next(Ok(frame(2))));
        assert_eq!(frame_value(in_flight.pop_ready()), Ok(2));
        assert!(!in_flight.fill_next(Ok(frame(3))), "nothing is waiting");
    }

    #[test]
    fn a_failed_submission_keeps_its_place() {
        let mut in_flight = InFlight::default();
        in_flight.push(&[rect(1)], 0.0);
        in_flight.push_failed(DecoderError::msg("no key frame"));

        assert_eq!(frame_value(in_flight.pop_ready()), Err("pending".to_owned()));
        in_flight.fill_next(Ok(frame(1)));
        assert_eq!(frame_value(in_flight.pop_ready()), Ok(1));
        assert!(frame_value(in_flight.pop_ready()).is_err_and(|error| error.contains("no key frame")));
    }

    #[test]
    fn failing_all_fails_only_waiting_entries() {
        let mut in_flight = InFlight::default();
        in_flight.push(&[rect(1)], 0.0);
        in_flight.fill_next(Ok(frame(1)));
        in_flight.push(&[rect(2)], 0.0);
        in_flight.push(&[rect(3)], 0.0);

        assert_eq!(in_flight.fail_waiting("decoder closed"), 2);

        assert_eq!(frame_value(in_flight.pop_ready()), Ok(1), "a decoded frame survives");
        assert!(frame_value(in_flight.pop_ready()).is_err_and(|error| error.contains("decoder closed")));
        assert!(frame_value(in_flight.pop_ready()).is_err_and(|error| error.contains("decoder closed")));
        assert_eq!(frame_value(in_flight.pop_ready()), Err("pending".to_owned()));
    }

    #[test]
    fn nal_units_split_on_both_start_code_lengths() {
        let stream = annex_b(&[SPS, PPS, IDR]);
        assert_eq!(nal_units(&stream).collect::<Vec<_>>(), vec![SPS, PPS, IDR]);
    }

    #[test]
    fn nal_units_of_an_empty_stream_is_none() {
        assert_eq!(nal_units(&[]).count(), 0);
        assert_eq!(nal_units(&[0, 0, 1]).count(), 0);
    }

    #[test]
    fn length_prefixed_input_is_rewritten_as_annex_b() {
        let input = length_prefixed(&[SPS, IDR]);
        let converted = to_annex_b(&input);
        assert_eq!(nal_units(&converted).collect::<Vec<_>>(), vec![SPS, IDR]);
    }

    #[test]
    fn annex_b_input_passes_through() {
        let stream = annex_b(&[SPS, IDR]);
        assert!(matches!(to_annex_b(&stream), Cow::Borrowed(borrowed) if borrowed == stream.as_slice()));
    }

    #[test]
    fn codec_string_comes_from_the_sps() {
        let stream = annex_b(&[SPS, PPS, IDR]);
        assert_eq!(codec_string(&stream).as_deref(), Some("avc1.640033"));
        let stream = annex_b(&[P_SLICE]);
        assert_eq!(codec_string(&stream), None);
    }

    #[test]
    fn only_idr_access_units_are_key_chunks() {
        assert!(is_key(&annex_b(&[SPS, PPS, IDR])));
        assert!(!is_key(&annex_b(&[P_SLICE])));
    }
}
