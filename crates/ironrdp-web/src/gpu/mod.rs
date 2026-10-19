//! WebGL2 surface backend: EGFX surfaces and cache slots as GPU textures.
//!
//! Every surface command is a GPU-to-GPU copy (`blitFramebuffer`) or an upload, so H.264
//! frames decoded by WebCodecs are drawn without ever being read back to the CPU. Surfaces are
//! stored top row first; presenting to the canvas, at `EndFrame`, flips them.
//!
//! [`SurfaceBackend`] must be `Send` and WebGL objects are not, so [`GlBackend`] is a handle:
//! its GL state lives in a thread-local slot, as the H.264 decoder's does.

mod frames;
mod geometry;

use core::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Context as _;
use ironrdp::pdu::geometry::{ExclusiveRectangle, InclusiveRectangle, Rectangle as _};
use ironrdp_egfx::compositor::{OutputUpdate, SurfaceBackend};
use ironrdp_egfx::decode::DecodedFrame;
use ironrdp_egfx::pdu::{Color, Point};
use tracing::warn;
use wasm_bindgen::prelude::*;
use web_sys::{HtmlCanvasElement, WebGl2RenderingContext as Gl, WebGlBuffer, WebGlFramebuffer, WebGlTexture};

pub(crate) use self::frames::{insert as hold_frame, release as release_frames};
use self::geometry::{BlitRect, clip, clip_copy, exceeds_texture_limit, present_blit};
use crate::h264::VideoFrame;

/// Upper bound on the texture bytes held across surfaces and cache slots, as the CPU
/// compositor's `MAX_COMPOSITOR_BYTES`: room for several 4K surfaces, but not for a hostile
/// peer's thousands of them.
const MAX_GPU_BYTES: usize = 256 * 1024 * 1024;

// web-sys binds `texImage2D` with a `VideoFrame` source only under its unstable APIs, so the
// same JS class is bound here with that one overload.
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = WebGL2RenderingContext)]
    type GlVideoUpload;

    #[wasm_bindgen(method, js_name = texImage2D, catch)]
    fn tex_image_2d_with_video_frame(
        this: &GlVideoUpload,
        target: u32,
        level: i32,
        internal_format: i32,
        format: u32,
        type_: u32,
        source: &VideoFrame,
    ) -> Result<(), JsValue>;
}

thread_local! {
    static SLOTS: RefCell<BTreeMap<u32, GlState>> = const { RefCell::new(BTreeMap::new()) };
    static NEXT_SLOT: Cell<u32> = const { Cell::new(0) };
    /// Whether `upload_into` times itself; only the bench turns it on.
    static UPLOAD_TIMING: Cell<bool> = const { Cell::new(false) };
    /// Time spent in `upload_into` while timing, in ms, and the number of uploads.
    static UPLOAD_STATS: Cell<(f64, u32)> = const { Cell::new((0.0, 0)) };
}

/// Starts or stops timing surface uploads (see [`take_upload_stats`]).
pub(crate) fn set_upload_timing(on: bool) {
    UPLOAD_TIMING.set(on);
}

/// The time spent uploading surface pixels, in ms, and the number of uploads, since the last
/// call.
pub(crate) fn take_upload_stats() -> (f64, u32) {
    UPLOAD_STATS.replace((0.0, 0))
}

fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|window| window.performance())
        .map_or(0.0, |performance| performance.now())
}

/// [`SurfaceBackend`] drawing with WebGL2 into a canvas; a handle to its thread-local slot.
///
/// Clones share the slot, so the EGFX client and the session's presenter draw to one
/// context; the slot is freed with the last clone.
#[derive(Clone)]
pub(crate) struct GlBackend {
    slot: Arc<SlotGuard>,
}

/// Frees a GL slot when the last [`GlBackend`] handle drops.
struct SlotGuard(u32);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let state = SLOTS.with(|slots| slots.borrow_mut().remove(&self.0));
        if let Some(mut state) = state {
            state.clear_surfaces();
            for (_, target) in core::mem::take(&mut state.cache) {
                target.delete(&state.gl);
            }
            if let Some(output) = state.output_target.take() {
                output.delete(&state.gl);
            }
            if let Some(scratch) = state.scratch.take() {
                scratch.delete(&state.gl);
            }
            if let Some(unpack) = state.unpack.take() {
                state.gl.delete_buffer(Some(&unpack));
            }
            if let Some((texture, framebuffer)) = state.video.take() {
                state.gl.delete_texture(Some(&texture));
                state.gl.delete_framebuffer(Some(&framebuffer));
            }
            // The canvas outlives the backend (a reconnect draws into it again), and the
            // listener would otherwise call into the closure dropped with this state.
            let _ = state
                .canvas
                .remove_event_listener_with_callback("webglcontextlost", state.on_lost.as_ref().unchecked_ref());
        }
    }
}

/// A texture with a framebuffer to draw into or read from it.
struct Target {
    texture: WebGlTexture,
    framebuffer: WebGlFramebuffer,
    width: u16,
    height: u16,
}

struct Surface {
    target: Target,
    /// Output origin and the size the surface is drawn at.
    mapping: Option<((u32, u32), (u32, u32))>,
}

struct GlState {
    gl: Gl,
    canvas: HtmlCanvasElement,
    output: (u32, u32),
    surfaces: BTreeMap<u16, Surface>,
    cache: BTreeMap<u16, Target>,
    /// The last H.264 frame, re-specified per frame; drawn from by region.
    video: Option<(WebGlTexture, WebGlFramebuffer)>,
    /// Staging for copies within one surface, which WebGL cannot blit directly.
    scratch: Option<Target>,
    /// Pixel-unpack buffer for RemoteFX tiles decoded in a Worker, created on first use.
    unpack: Option<WebGlBuffer>,
    allocated: usize,
    max_texture_size: u32,
    /// Why the backend stopped drawing, once it has; the session ends with this.
    failure: Rc<Cell<Option<&'static str>>>,
    /// Pixels the session draws outside EGFX (legacy bitmap updates), at canvas size.
    output_target: Option<Target>,
    on_lost: Closure<dyn FnMut()>,
}

impl GlBackend {
    /// A backend drawing into `canvas`, for a `width` by `height` desktop. Fails when the GPU
    /// cannot hold a texture that large, or will not say how large it can, so the session
    /// draws on the CPU instead. Once [`failure`](Self::failure) reports a reason (the browser
    /// dropped the GPU context, or a surface outgrew it), nothing more is drawn and the
    /// session must end.
    pub(crate) fn new(canvas: &HtmlCanvasElement, width: u16, height: u16) -> anyhow::Result<Self> {
        let attributes = web_sys::WebGlContextAttributes::new();
        attributes.set_alpha(false);
        attributes.set_antialias(false);
        attributes.set_depth(false);
        attributes.set_stencil(false);
        attributes.set_premultiplied_alpha(false);
        attributes.set_preserve_drawing_buffer(false);
        let gl: Gl = canvas
            .get_context_with_context_options("webgl2", &attributes)
            .map_err(|error| anyhow::anyhow!("get_context(\"webgl2\") failed: {error:?}"))?
            .context("WebGL2 is unavailable")?
            .dyn_into()
            .map_err(|_| anyhow::anyhow!("webgl2 context is not a WebGL2RenderingContext"))?;
        gl.pixel_storei(Gl::UNPACK_ALIGNMENT, 1);

        let failure = Rc::new(Cell::new(None));
        let on_lost = {
            let failure = Rc::clone(&failure);
            Closure::<dyn FnMut()>::new(move || {
                warn!("GPU context lost");
                failure.set(Some("GPU context lost"));
            })
        };
        let max_texture_size = gl
            .get_parameter(Gl::MAX_TEXTURE_SIZE)
            .ok()
            .and_then(|value| value.as_f64())
            .map_or(0, |value| {
                #[expect(
                    clippy::as_conversions,
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "a texture size well below u32::MAX"
                )]
                let size = value as u32;
                size
            });
        if exceeds_texture_limit(width, height, max_texture_size) {
            anyhow::bail!("a {width}x{height} desktop exceeds the GPU texture limit ({max_texture_size})");
        }
        canvas
            .add_event_listener_with_callback("webglcontextlost", on_lost.as_ref().unchecked_ref())
            .map_err(|error| anyhow::anyhow!("listen for webglcontextlost: {error:?}"))?;

        let state = GlState {
            gl,
            canvas: canvas.clone(),
            output: (canvas.width(), canvas.height()),
            surfaces: BTreeMap::new(),
            cache: BTreeMap::new(),
            video: None,
            scratch: None,
            unpack: None,
            allocated: 0,
            max_texture_size,
            failure,
            output_target: None,
            on_lost,
        };
        let slot = NEXT_SLOT.with(|next| {
            let slot = next.get();
            next.set(slot.wrapping_add(1));
            slot
        });
        SLOTS.with(|slots| slots.borrow_mut().insert(slot, state));
        Ok(Self {
            slot: Arc::new(SlotGuard(slot)),
        })
    }

    /// Why the backend stopped drawing, if it has.
    pub(crate) fn failure(&self) -> Option<&'static str> {
        SLOTS.with(|slots| slots.borrow().get(&self.slot.0).and_then(|state| state.failure.get()))
    }

    /// The canvas size, in pixels.
    pub(crate) fn size(&self) -> (u32, u32) {
        SLOTS.with(|slots| {
            slots
                .borrow()
                .get(&self.slot.0)
                .map_or((0, 0), |state| (state.canvas.width(), state.canvas.height()))
        })
    }

    /// Resizes the canvas for pixels drawn outside EGFX.
    pub(crate) fn resize_output(&self, width: u32, height: u32) {
        self.with(|state| {
            state.canvas.set_width(width);
            state.canvas.set_height(height);
            state.output = (width, height);
            if let Some(output) = state.output_target.take() {
                output.delete(&state.gl);
            }
        });
    }

    /// Draws `rgba`, the pixels of `region`, onto the canvas: the path for updates that do
    /// not come through EGFX (legacy bitmaps).
    pub(crate) fn present_rgba(&self, region: &InclusiveRectangle, rgba: &[u8]) {
        self.with(|state| {
            let (width, height) = (
                u16::try_from(state.output.0).unwrap_or(u16::MAX),
                u16::try_from(state.output.1).unwrap_or(u16::MAX),
            );
            if state.output_target.is_none() {
                state.output_target = Target::new(&state.gl, width, height);
            }
            let Some(output) = state.output_target.as_ref() else {
                return;
            };
            let rect = ExclusiveRectangle {
                left: region.left,
                top: region.top,
                right: region.right.saturating_add(1),
                bottom: region.bottom.saturating_add(1),
            };
            upload_into(
                &state.gl,
                output,
                &rect,
                rgba,
                u32::from(region.width()),
                (rect.left, rect.top),
            );
            let (source, destination) = present_blit((width, height), (0, 0), state.output, state.output.1);
            blit(
                &state.gl,
                Some(&output.framebuffer),
                None,
                source,
                destination,
                Gl::NEAREST,
            );
        });
    }

    /// Every `step`th pixel of every `step`th row of what was just presented, as RGB, top row
    /// first. For the bench's CPU-against-GPU comparison; it must run in the same task as the
    /// present, because the canvas is not preserved after compositing.
    pub(crate) fn sample_output(&self, step: u32) -> Vec<u8> {
        self.with(|state| {
            let (width, height) = state.output;
            let mut rgba = vec![0u8; usize::try_from(u64::from(width) * u64::from(height) * 4).unwrap_or(0)];
            state.gl.bind_framebuffer(Gl::READ_FRAMEBUFFER, None);
            #[expect(clippy::as_conversions, clippy::cast_possible_wrap, reason = "canvas sizes fit i32")]
            let read = state.gl.read_pixels_with_opt_u8_array(
                0,
                0,
                width as i32,
                height as i32,
                Gl::RGBA,
                Gl::UNSIGNED_BYTE,
                Some(&mut rgba),
            );
            if read.is_err() {
                return Vec::new();
            }
            // GL rows run bottom to top.
            sample_rgb(&rgba, width, height, step, true)
        })
        .unwrap_or_default()
    }

    fn with<R>(&self, f: impl FnOnce(&mut GlState) -> R) -> Option<R> {
        SLOTS.with(|slots| {
            let mut slots = slots.borrow_mut();
            let state = slots.get_mut(&self.slot.0)?;
            if state.gl.is_context_lost() {
                return None;
            }
            Some(f(state))
        })
    }
}

impl Target {
    fn new(gl: &Gl, width: u16, height: u16) -> Option<Self> {
        let texture = gl.create_texture()?;
        gl.bind_texture(Gl::TEXTURE_2D, Some(&texture));
        gl.tex_storage_2d(Gl::TEXTURE_2D, 1, Gl::RGBA8, i32::from(width), i32::from(height));
        set_nearest(gl);
        let framebuffer = gl.create_framebuffer()?;
        gl.bind_framebuffer(Gl::FRAMEBUFFER, Some(&framebuffer));
        gl.framebuffer_texture_2d(
            Gl::FRAMEBUFFER,
            Gl::COLOR_ATTACHMENT0,
            Gl::TEXTURE_2D,
            Some(&texture),
            0,
        );
        gl.clear_color(0.0, 0.0, 0.0, 0.0);
        gl.clear(Gl::COLOR_BUFFER_BIT);
        Some(Self {
            texture,
            framebuffer,
            width,
            height,
        })
    }

    fn bytes(&self) -> usize {
        usize::from(self.width) * usize::from(self.height) * 4
    }

    fn delete(&self, gl: &Gl) {
        gl.delete_framebuffer(Some(&self.framebuffer));
        gl.delete_texture(Some(&self.texture));
    }
}

fn set_nearest(gl: &Gl) {
    for (parameter, value) in [
        (Gl::TEXTURE_MIN_FILTER, Gl::NEAREST),
        (Gl::TEXTURE_MAG_FILTER, Gl::NEAREST),
        (Gl::TEXTURE_WRAP_S, Gl::CLAMP_TO_EDGE),
        (Gl::TEXTURE_WRAP_T, Gl::CLAMP_TO_EDGE),
    ] {
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_wrap,
            reason = "GL enum values fit i32"
        )]
        gl.tex_parameteri(Gl::TEXTURE_2D, parameter, value as i32);
    }
}

fn blit_rect(rect: &ExclusiveRectangle) -> BlitRect {
    (
        i32::from(rect.left),
        i32::from(rect.top),
        i32::from(rect.right),
        i32::from(rect.bottom),
    )
}

fn blit(
    gl: &Gl,
    read: Option<&WebGlFramebuffer>,
    draw: Option<&WebGlFramebuffer>,
    source: BlitRect,
    destination: BlitRect,
    filter: u32,
) {
    gl.bind_framebuffer(Gl::READ_FRAMEBUFFER, read);
    gl.bind_framebuffer(Gl::DRAW_FRAMEBUFFER, draw);
    gl.blit_framebuffer(
        source.0,
        source.1,
        source.2,
        source.3,
        destination.0,
        destination.1,
        destination.2,
        destination.3,
        Gl::COLOR_BUFFER_BIT,
        filter,
    );
}

/// Uploads the `rect` of `pixels`, an RGBA image `stride` pixels wide whose top-left is
/// `origin`, into `target`, clipped to it.
fn upload_into(gl: &Gl, target: &Target, rect: &ExclusiveRectangle, pixels: &[u8], stride: u32, origin: (u16, u16)) {
    let Some(rect) = clip(rect, target.width, target.height) else {
        return;
    };
    let started = UPLOAD_TIMING.get().then(now_ms);
    gl.bind_texture(Gl::TEXTURE_2D, Some(&target.texture));
    #[expect(clippy::as_conversions, clippy::cast_possible_wrap, reason = "image strides fit i32")]
    gl.pixel_storei(Gl::UNPACK_ROW_LENGTH, stride as i32);
    gl.pixel_storei(Gl::UNPACK_SKIP_PIXELS, i32::from(rect.left - origin.0));
    gl.pixel_storei(Gl::UNPACK_SKIP_ROWS, i32::from(rect.top - origin.1));
    if let Err(error) = gl.tex_sub_image_2d_with_i32_and_i32_and_u32_and_type_and_opt_u8_array(
        Gl::TEXTURE_2D,
        0,
        i32::from(rect.left),
        i32::from(rect.top),
        i32::from(rect.right - rect.left),
        i32::from(rect.bottom - rect.top),
        Gl::RGBA,
        Gl::UNSIGNED_BYTE,
        Some(pixels),
    ) {
        warn!(?error, "Failed to upload surface pixels");
    }
    gl.pixel_storei(Gl::UNPACK_ROW_LENGTH, 0);
    gl.pixel_storei(Gl::UNPACK_SKIP_PIXELS, 0);
    gl.pixel_storei(Gl::UNPACK_SKIP_ROWS, 0);
    if let Some(started) = started {
        let (ms, calls) = UPLOAD_STATS.get();
        UPLOAD_STATS.set((ms + now_ms() - started, calls + 1));
    }
}

/// Every `step`th pixel of every `step`th row of an RGBA image, as RGB, top row first.
/// `bottom_up` says the image's rows are stored bottom first, as GL reads them.
pub(crate) fn sample_rgb(rgba: &[u8], width: u32, height: u32, step: u32, bottom_up: bool) -> Vec<u8> {
    let (Ok(width), Ok(height), Ok(step)) = (usize::try_from(width), usize::try_from(height), usize::try_from(step))
    else {
        return Vec::new();
    };
    let mut rgb = Vec::new();
    for y in (0..height).step_by(step.max(1)) {
        let row = if bottom_up { height - 1 - y } else { y };
        for x in (0..width).step_by(step.max(1)) {
            let offset = (row * width + x) * 4;
            if let Some(pixel) = rgba.get(offset..offset + 3) {
                rgb.extend_from_slice(pixel);
            }
        }
    }
    rgb
}

impl GlState {
    fn clear_surfaces(&mut self) {
        for (_, surface) in core::mem::take(&mut self.surfaces) {
            self.allocated = self.allocated.saturating_sub(surface.target.bytes());
            surface.target.delete(&self.gl);
        }
    }

    /// A target of `width` by `height` within the GPU byte budget.
    fn allocate(&mut self, width: u16, height: u16) -> Option<Target> {
        let bytes = usize::from(width) * usize::from(height) * 4;
        if width == 0 || height == 0 || MAX_GPU_BYTES < self.allocated.saturating_add(bytes) {
            warn!(
                width,
                height,
                allocated = self.allocated,
                "Refusing a GPU surface over budget"
            );
            return None;
        }
        let target = Target::new(&self.gl, width, height)?;
        self.allocated += bytes;
        Some(target)
    }

    fn release(&mut self, target: Target) {
        self.allocated = self.allocated.saturating_sub(target.bytes());
        target.delete(&self.gl);
    }

    /// Uploads the `rect` of `pixels`, an RGBA image `stride` pixels wide whose top-left
    /// is `origin`, into the surface.
    fn upload(&self, surface_id: u16, rect: &ExclusiveRectangle, pixels: &[u8], stride: u32, origin: (u16, u16)) {
        if let Some(surface) = self.surfaces.get(&surface_id) {
            upload_into(&self.gl, &surface.target, rect, pixels, stride, origin);
        }
    }

    /// Uploads a Worker's packed RemoteFX tiles into the surface through the pixel-unpack
    /// buffer: one buffer upload, then one texture upload per placement from its offset.
    /// That costs the main thread about a sixth of uploading from client memory per tile
    /// (stage 0 of `docs/plans/2026-10-19-web-rfx-workers-design.md`).
    fn upload_packed(&mut self, surface_id: u16, tiles: &crate::rfx_worker::ExternalTiles) {
        let started = UPLOAD_TIMING.get().then(now_ms);
        if self.unpack.is_none() {
            self.unpack = self.gl.create_buffer();
        }
        let (Some(surface), Some(unpack)) = (self.surfaces.get(&surface_id), self.unpack.as_ref()) else {
            return;
        };
        let (gl, target) = (&self.gl, &surface.target);
        gl.bind_buffer(Gl::PIXEL_UNPACK_BUFFER, Some(unpack));
        let pixels = js_sys::Uint8Array::new_with_byte_offset_and_length(&tiles.pixels, 0, tiles.length);
        gl.buffer_data_with_array_buffer_view(Gl::PIXEL_UNPACK_BUFFER, &pixels, Gl::STREAM_DRAW);
        gl.bind_texture(Gl::TEXTURE_2D, Some(&target.texture));
        let mut uploads = 0u32;
        for placement in &tiles.placements {
            let rect = &placement.rect;
            let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
            let bytes = u64::from(width) * u64::from(height) * 4;
            // The Worker clipped every placement to the surface; one that does not fit was
            // not packed for this surface, and its offset would read the wrong pixels.
            let fits = clip(rect, target.width, target.height).as_ref() == Some(rect)
                && u64::from(placement.offset) + bytes <= u64::from(tiles.length);
            let Ok(offset) = i32::try_from(placement.offset) else {
                continue;
            };
            if !fits {
                warn!(surface_id, ?rect, "Skipping a RemoteFX placement outside its surface");
                continue;
            }
            if let Err(error) = gl.tex_sub_image_2d_with_i32_and_i32_and_u32_and_type_and_i32(
                Gl::TEXTURE_2D,
                0,
                i32::from(rect.left),
                i32::from(rect.top),
                i32::from(width),
                i32::from(height),
                Gl::RGBA,
                Gl::UNSIGNED_BYTE,
                offset,
            ) {
                warn!(?error, "Failed to upload RemoteFX tiles");
            }
            uploads += 1;
        }
        // Unbound, so the other uploads read client memory again rather than buffer offsets.
        gl.bind_buffer(Gl::PIXEL_UNPACK_BUFFER, None);
        if let Some(started) = started {
            let (ms, calls) = UPLOAD_STATS.get();
            UPLOAD_STATS.set((ms + now_ms() - started, calls + uploads));
        }
    }

    /// Draws a `VideoFrame` into the video texture, re-specifying it at the frame's size.
    fn upload_video(&mut self, frame: &VideoFrame) -> Option<(u16, u16)> {
        let gl = &self.gl;
        if self.video.is_none() {
            let texture = gl.create_texture()?;
            let framebuffer = gl.create_framebuffer()?;
            self.video = Some((texture, framebuffer));
        }
        let (texture, framebuffer) = self.video.as_ref()?;
        gl.bind_texture(Gl::TEXTURE_2D, Some(texture));
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_wrap,
            reason = "GL enum values fit i32"
        )]
        let rgba = Gl::RGBA as i32;
        if let Err(error) = gl.unchecked_ref::<GlVideoUpload>().tex_image_2d_with_video_frame(
            Gl::TEXTURE_2D,
            0,
            rgba,
            Gl::RGBA,
            Gl::UNSIGNED_BYTE,
            frame,
        ) {
            warn!(?error, "Failed to upload an H.264 frame");
            return None;
        }
        set_nearest(gl);
        gl.bind_framebuffer(Gl::FRAMEBUFFER, Some(framebuffer));
        gl.framebuffer_texture_2d(Gl::FRAMEBUFFER, Gl::COLOR_ATTACHMENT0, Gl::TEXTURE_2D, Some(texture), 0);
        Some((
            u16::try_from(frame.display_width()).ok()?,
            u16::try_from(frame.display_height()).ok()?,
        ))
    }

    /// A scratch target at least `width` by `height`.
    fn scratch(&mut self, width: u16, height: u16) -> Option<&Target> {
        if self
            .scratch
            .as_ref()
            .is_some_and(|scratch| scratch.width < width || scratch.height < height)
            && let Some(scratch) = self.scratch.take()
        {
            scratch.delete(&self.gl);
        }
        if self.scratch.is_none() {
            self.scratch = Some(Target::new(&self.gl, width, height)?);
        }
        self.scratch.as_ref()
    }

    /// Copies `source` of surface `src_id` to `point` on surface `dst_id`.
    fn copy(&mut self, src_id: u16, dst_id: u16, source: &ExclusiveRectangle, point: (u16, u16)) {
        let (Some(src), Some(dst)) = (self.surfaces.get(&src_id), self.surfaces.get(&dst_id)) else {
            return;
        };
        let Some(source) = clip(source, src.target.width, src.target.height) else {
            return;
        };
        let Some((source, destination)) = clip_copy(&source, point, (dst.target.width, dst.target.height)) else {
            return;
        };
        if src_id != dst_id {
            blit(
                &self.gl,
                Some(&src.target.framebuffer),
                Some(&dst.target.framebuffer),
                blit_rect(&source),
                blit_rect(&destination),
                Gl::NEAREST,
            );
            return;
        }
        // WebGL forbids blitting within one image: stage through the scratch target.
        let (width, height) = (source.right - source.left, source.bottom - source.top);
        let src_framebuffer = src.target.framebuffer.clone();
        let Some(scratch) = self.scratch(width, height).map(|scratch| scratch.framebuffer.clone()) else {
            return;
        };
        let staged = (0, 0, i32::from(width), i32::from(height));
        blit(
            &self.gl,
            Some(&src_framebuffer),
            Some(&scratch),
            blit_rect(&source),
            staged,
            Gl::NEAREST,
        );
        blit(
            &self.gl,
            Some(&scratch),
            Some(&src_framebuffer),
            staged,
            blit_rect(&destination),
            Gl::NEAREST,
        );
    }

    /// Draws every mapped surface to the canvas.
    fn present(&self) {
        let gl = &self.gl;
        gl.bind_framebuffer(Gl::DRAW_FRAMEBUFFER, None);
        gl.clear_color(0.0, 0.0, 0.0, 1.0);
        gl.clear(Gl::COLOR_BUFFER_BIT);
        for surface in self.surfaces.values() {
            let Some((origin, size)) = surface.mapping else {
                continue;
            };
            let scaled = size != (u32::from(surface.target.width), u32::from(surface.target.height));
            let (source, destination) = present_blit(
                (surface.target.width, surface.target.height),
                origin,
                size,
                self.output.1,
            );
            blit(
                gl,
                Some(&surface.target.framebuffer),
                None,
                source,
                destination,
                if scaled { Gl::LINEAR } else { Gl::NEAREST },
            );
        }
    }
}

impl SurfaceBackend for GlBackend {
    fn reset(&mut self, width: u32, height: u32) {
        self.with(|state| {
            state.clear_surfaces();
            state.canvas.set_width(width);
            state.canvas.set_height(height);
            state.output = (width, height);
            // Sized for the previous output; recreated at the new size when next drawn into.
            if let Some(output) = state.output_target.take() {
                output.delete(&state.gl);
            }
        });
    }

    fn create_surface(&mut self, id: u16, width: u16, height: u16) {
        self.with(|state| {
            if exceeds_texture_limit(width, height, state.max_texture_size) {
                warn!(
                    width,
                    height,
                    max = state.max_texture_size,
                    "Surface exceeds the GPU texture limit"
                );
                state.failure.set(Some("Surface exceeds the GPU texture limit"));
                return;
            }
            if let Some(old) = state.surfaces.remove(&id) {
                state.release(old.target);
            }
            if let Some(target) = state.allocate(width, height) {
                state.surfaces.insert(id, Surface { target, mapping: None });
            }
        });
    }

    fn delete_surface(&mut self, id: u16) {
        self.with(|state| {
            if let Some(surface) = state.surfaces.remove(&id) {
                state.release(surface.target);
            }
        });
    }

    fn map_surface(&mut self, id: u16, origin_x: u32, origin_y: u32) {
        self.with(|state| {
            if let Some(surface) = state.surfaces.get_mut(&id) {
                let size = (u32::from(surface.target.width), u32::from(surface.target.height));
                surface.mapping = Some(((origin_x, origin_y), size));
            }
        });
    }

    fn map_surface_scaled(&mut self, id: u16, origin_x: u32, origin_y: u32, target_width: u32, target_height: u32) {
        self.with(|state| {
            if let Some(surface) = state.surfaces.get_mut(&id) {
                surface.mapping = Some(((origin_x, origin_y), (target_width, target_height)));
            }
        });
    }

    fn write_rgba(&mut self, surface_id: u16, dest: &ExclusiveRectangle, rgba: &[u8]) {
        let (width, height) = (
            dest.right.saturating_sub(dest.left),
            dest.bottom.saturating_sub(dest.top),
        );
        if rgba.len() < usize::from(width) * usize::from(height) * 4 {
            warn!(
                surface_id,
                len = rgba.len(),
                width,
                height,
                "Bitmap data shorter than its rectangle"
            );
            return;
        }
        self.with(|state| state.upload(surface_id, dest, rgba, u32::from(width), (dest.left, dest.top)));
    }

    fn write_frame(
        &mut self,
        surface_id: u16,
        origin: (u16, u16),
        regions: &[ExclusiveRectangle],
        frame: &DecodedFrame,
    ) {
        let Some(handle) = frame.external_handle() else {
            warn!("Ignoring a CPU frame passed as external");
            return;
        };
        let Some(video_frame) = frames::take(handle) else {
            warn!(handle, "H.264 frame is no longer held");
            return;
        };
        self.with(|state| {
            let uploaded = state.upload_video(&video_frame);
            let (Some((width, height)), Some(surface), Some((_, video))) =
                (uploaded, state.surfaces.get(&surface_id), state.video.as_ref())
            else {
                return;
            };
            let frame_right = origin.0.saturating_add(width).min(surface.target.width);
            let frame_bottom = origin.1.saturating_add(height).min(surface.target.height);
            for region in regions {
                let Some(region) = clip(region, frame_right, frame_bottom) else {
                    continue;
                };
                if region.left < origin.0 || region.top < origin.1 {
                    continue;
                }
                let source = ExclusiveRectangle {
                    left: region.left - origin.0,
                    top: region.top - origin.1,
                    right: region.right - origin.0,
                    bottom: region.bottom - origin.1,
                };
                blit(
                    &state.gl,
                    Some(video),
                    Some(&surface.target.framebuffer),
                    blit_rect(&source),
                    blit_rect(&region),
                    Gl::NEAREST,
                );
            }
        });
        video_frame.close();
    }

    fn solid_fill(&mut self, surface_id: u16, color: &Color, rects: &[ExclusiveRectangle]) {
        self.with(|state| {
            let Some(surface) = state.surfaces.get(&surface_id) else {
                return;
            };
            let gl = &state.gl;
            gl.bind_framebuffer(Gl::DRAW_FRAMEBUFFER, Some(&surface.target.framebuffer));
            gl.clear_color(
                f32::from(color.r) / 255.0,
                f32::from(color.g) / 255.0,
                f32::from(color.b) / 255.0,
                1.0,
            );
            gl.enable(Gl::SCISSOR_TEST);
            for rect in rects {
                if let Some(rect) = clip(rect, surface.target.width, surface.target.height) {
                    gl.scissor(
                        i32::from(rect.left),
                        i32::from(rect.top),
                        i32::from(rect.right - rect.left),
                        i32::from(rect.bottom - rect.top),
                    );
                    gl.clear(Gl::COLOR_BUFFER_BIT);
                }
            }
            gl.disable(Gl::SCISSOR_TEST);
        });
    }

    fn surface_to_surface(&mut self, src_id: u16, dst_id: u16, src_rect: &ExclusiveRectangle, dst_points: &[Point]) {
        self.with(|state| {
            for point in dst_points {
                state.copy(src_id, dst_id, src_rect, (point.x, point.y));
            }
        });
    }

    fn surface_to_cache(&mut self, surface_id: u16, cache_slot: u16, src_rect: &ExclusiveRectangle) {
        self.with(|state| {
            if let Some(old) = state.cache.remove(&cache_slot) {
                state.release(old);
            }
            let Some(source) = state
                .surfaces
                .get(&surface_id)
                .and_then(|surface| clip(src_rect, surface.target.width, surface.target.height))
            else {
                return;
            };
            let (width, height) = (source.right - source.left, source.bottom - source.top);
            let Some(tile) = state.allocate(width, height) else {
                return;
            };
            if let Some(surface) = state.surfaces.get(&surface_id) {
                blit(
                    &state.gl,
                    Some(&surface.target.framebuffer),
                    Some(&tile.framebuffer),
                    blit_rect(&source),
                    (0, 0, i32::from(width), i32::from(height)),
                    Gl::NEAREST,
                );
            }
            state.cache.insert(cache_slot, tile);
        });
    }

    fn cache_to_surface(&mut self, cache_slot: u16, surface_id: u16, dst_points: &[Point]) {
        self.with(|state| {
            let (Some(tile), Some(surface)) = (state.cache.get(&cache_slot), state.surfaces.get(&surface_id)) else {
                return;
            };
            let whole = ExclusiveRectangle {
                left: 0,
                top: 0,
                right: tile.width,
                bottom: tile.height,
            };
            for point in dst_points {
                if let Some((source, destination)) = clip_copy(
                    &whole,
                    (point.x, point.y),
                    (surface.target.width, surface.target.height),
                ) {
                    blit(
                        &state.gl,
                        Some(&tile.framebuffer),
                        Some(&surface.target.framebuffer),
                        blit_rect(&source),
                        blit_rect(&destination),
                        Gl::NEAREST,
                    );
                }
            }
        });
    }

    fn evict_cache_entry(&mut self, cache_slot: u16) {
        self.with(|state| {
            if let Some(tile) = state.cache.remove(&cache_slot) {
                state.release(tile);
            }
        });
    }

    fn write_external_tiles(&mut self, surface_id: u16, handle: u64) {
        let Some(tiles) = crate::rfx_worker::take_tiles(handle) else {
            warn!(surface_id, handle, "RemoteFX tiles are gone before drawing");
            return;
        };
        self.with(|state| state.upload_packed(surface_id, &tiles));
        crate::rfx_worker::return_buffer(tiles);
    }

    fn end_frame(&mut self) {
        self.with(|state| state.present());
    }

    fn drain_output(&mut self) -> Vec<OutputUpdate> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_rgb_takes_every_step_pixel_top_row_first() {
        // 2x2 RGBA: top-left 1, top-right 2, bottom-left 3, bottom-right 4 (stored top first).
        let top_first = [1, 1, 1, 255, 2, 2, 2, 255, 3, 3, 3, 255, 4, 4, 4, 255];
        let bottom_first = [3, 3, 3, 255, 4, 4, 4, 255, 1, 1, 1, 255, 2, 2, 2, 255];

        assert_eq!(
            sample_rgb(&top_first, 2, 2, 1, false),
            vec![1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4]
        );
        assert_eq!(
            sample_rgb(&bottom_first, 2, 2, 1, true),
            vec![1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4]
        );
        assert_eq!(sample_rgb(&top_first, 2, 2, 2, false), vec![1, 1, 1]);
    }
}
