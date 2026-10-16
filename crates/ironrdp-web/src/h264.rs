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
use std::collections::BTreeMap;
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

/// [`H264Decoder`] backed by a WebCodecs `VideoDecoder`; a handle to its thread-local slot.
pub(crate) struct WebCodecsH264Decoder {
    slot: u32,
}

impl WebCodecsH264Decoder {
    /// `wake` is called whenever a submitted frame is ready for `poll_frame`.
    pub(crate) fn new(wake: impl Fn() + 'static) -> Self {
        let slot = NEXT_SLOT.with(|next| {
            let slot = next.get();
            next.set(slot.wrapping_add(1));
            slot
        });
        SLOTS.with(|slots| slots.borrow_mut().insert(slot, Slot::new(Rc::new(wake))));
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

    fn poll_frame(&mut self) -> Option<DecoderResult<DecodedFrame>> {
        self.with_slot(|slot| slot.shared.borrow_mut().finished.take())
            .flatten()
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
#[derive(Default)]
struct Shared {
    /// `regionRects` of the frame in flight: the only pixels read back.
    regions: Vec<ExclusiveRectangle>,
    finished: Option<DecoderResult<DecodedFrame>>,
    readback: Option<Readback>,
    submitted_at: f64,
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

/// Canvas a `VideoFrame` is drawn to so its pixels can be read back as RGBA.
struct Readback {
    canvas: OffscreenCanvas,
    context: OffscreenContext2d,
}

impl Slot {
    fn new(wake: Rc<dyn Fn()>) -> Self {
        Self {
            decoder: None,
            shared: Rc::new(RefCell::new(Shared::default())),
            wake,
            timestamp: 0.0,
        }
    }

    fn submit(&mut self, data: &[u8], regions: &[ExclusiveRectangle]) {
        self.shared.borrow_mut().finished = None;
        if let Err(error) = self.try_submit(data, regions) {
            self.shared.borrow_mut().finished = Some(Err(error));
            (self.wake)();
        }
    }

    fn try_submit(&mut self, data: &[u8], regions: &[ExclusiveRectangle]) -> DecoderResult<()> {
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

        {
            let mut shared = self.shared.borrow_mut();
            shared.regions.clear();
            shared.regions.extend_from_slice(regions);
            shared.submitted_at = js_sys::Date::now();
        }
        active
            .decoder
            .decode(&chunk)
            .map_err(|error| js_error("decode H.264 chunk", &error))?;
        active.awaiting_key = false;
        self.timestamp += 1.0;
        Ok(())
    }

    fn close(&mut self) {
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
                {
                    let mut shared = shared.borrow_mut();
                    let latency = js_sys::Date::now() - shared.submitted_at;
                    if SLOW_DECODE_MS < latency {
                        warn!(latency, "Slow H.264 decode");
                    }
                    let result = shared.read_back(&frame);
                    shared.finished = Some(result);
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
                shared.borrow_mut().finished = Some(Err(DecoderError::msg("H.264 decoder error")));
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
    /// Reads the in-flight frame's changed regions (or the whole frame, if it listed none)
    /// into a frame-sized RGBA buffer. The rest of the buffer stays zero: the client copies
    /// only the regions.
    fn read_back(&mut self, frame: &VideoFrame) -> DecoderResult<DecodedFrame> {
        let (width, height) = (frame.display_width(), frame.display_height());
        let readback = match self.readback.take() {
            Some(readback) if readback.canvas.width() == width && readback.canvas.height() == height => readback,
            _ => Readback::new(width, height)?,
        };
        let result = read_regions(&readback.context, frame, width, height, &self.regions);
        self.readback = Some(readback);
        result
    }
}

impl Readback {
    fn new(width: u32, height: u32) -> DecoderResult<Self> {
        let canvas = OffscreenCanvas::new(width, height).map_err(|error| js_error("create OffscreenCanvas", &error))?;
        let options = Object::new();
        // The pixels are read back on every frame: keep the canvas in CPU memory.
        set(&options, "willReadFrequently", &JsValue::TRUE)?;
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

    for region in regions {
        let right = u32::from(region.right).min(width);
        let bottom = u32::from(region.bottom).min(height);
        let (left, top) = (u32::from(region.left), u32::from(region.top));
        if right <= left || bottom <= top {
            continue;
        }
        let image = context
            .get_image_data(
                f64::from(left),
                f64::from(top),
                f64::from(right - left),
                f64::from(bottom - top),
            )
            .map_err(|error| js_error("read back VideoFrame", &error))?;
        let pixels = image.data();

        let row_bytes = usize::try_from(right - left).map_err(|_| DecoderError::msg("region too wide"))? * 4;
        let left_bytes = usize::try_from(left).map_err(|_| DecoderError::msg("region too wide"))? * 4;
        let top = usize::try_from(top).map_err(|_| DecoderError::msg("region too tall"))?;
        for (row, source) in pixels.chunks_exact(row_bytes).enumerate() {
            let start = (top + row) * stride + left_bytes;
            if let Some(destination) = rgba.get_mut(start..start + row_bytes) {
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

    type VideoFrame;

    #[wasm_bindgen(method, getter, js_name = displayWidth)]
    fn display_width(this: &VideoFrame) -> u32;

    #[wasm_bindgen(method, getter, js_name = displayHeight)]
    fn display_height(this: &VideoFrame) -> u32;

    #[wasm_bindgen(method)]
    fn close(this: &VideoFrame);

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
