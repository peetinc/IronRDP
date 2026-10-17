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

use anyhow::Context as _;
use ironrdp::pdu::geometry::ExclusiveRectangle;
use ironrdp_egfx::compositor::{OutputUpdate, SurfaceBackend};
use ironrdp_egfx::decode::DecodedFrame;
use ironrdp_egfx::pdu::{Color, Point};
use tracing::warn;
use wasm_bindgen::prelude::*;
use web_sys::{HtmlCanvasElement, WebGl2RenderingContext as Gl, WebGlFramebuffer, WebGlTexture};

pub(crate) use self::frames::insert as hold_frame;
use self::geometry::{BlitRect, clip, clip_copy, present_blit};
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
}

/// [`SurfaceBackend`] drawing with WebGL2 into a canvas; a handle to its thread-local slot.
pub(crate) struct GlBackend {
    slot: u32,
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
    allocated: usize,
    _on_lost: Closure<dyn FnMut()>,
}

impl GlBackend {
    /// A backend drawing into `canvas`. `lost` is set if the browser drops the GPU context,
    /// after which nothing is drawn and the session must end.
    pub(crate) fn new(canvas: &HtmlCanvasElement, lost: Rc<Cell<bool>>) -> anyhow::Result<Self> {
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

        let on_lost = Closure::<dyn FnMut()>::new(move || {
            warn!("GPU context lost");
            lost.set(true);
        });
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
            allocated: 0,
            _on_lost: on_lost,
        };
        let slot = NEXT_SLOT.with(|next| {
            let slot = next.get();
            next.set(slot.wrapping_add(1));
            slot
        });
        SLOTS.with(|slots| slots.borrow_mut().insert(slot, state));
        Ok(Self { slot })
    }

    /// The largest texture edge this GPU supports.
    pub(crate) fn max_texture_size(&self) -> u32 {
        self.with(|state| {
            state
                .gl
                .get_parameter(Gl::MAX_TEXTURE_SIZE)
                .ok()
                .and_then(|value| value.as_f64())
                .and_then(|value| {
                    #[expect(
                        clippy::as_conversions,
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss,
                        reason = "a texture size well below u32::MAX"
                    )]
                    let size = value as u32;
                    Some(size)
                })
                .unwrap_or(0)
        })
        .unwrap_or(0)
    }

    fn with<R>(&self, f: impl FnOnce(&mut GlState) -> R) -> Option<R> {
        SLOTS.with(|slots| {
            let mut slots = slots.borrow_mut();
            let state = slots.get_mut(&self.slot)?;
            if state.gl.is_context_lost() {
                return None;
            }
            Some(f(state))
        })
    }
}

impl Drop for GlBackend {
    fn drop(&mut self) {
        let state = SLOTS.with(|slots| slots.borrow_mut().remove(&self.slot));
        if let Some(mut state) = state {
            state.clear_surfaces();
            for (_, target) in core::mem::take(&mut state.cache) {
                target.delete(&state.gl);
            }
        }
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
        let Some(surface) = self.surfaces.get(&surface_id) else {
            return;
        };
        let Some(rect) = clip(rect, surface.target.width, surface.target.height) else {
            return;
        };
        let gl = &self.gl;
        gl.bind_texture(Gl::TEXTURE_2D, Some(&surface.target.texture));
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
        });
    }

    fn create_surface(&mut self, id: u16, width: u16, height: u16) {
        self.with(|state| {
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

    fn write_frame(&mut self, surface_id: u16, regions: &[ExclusiveRectangle], frame: &DecodedFrame) {
        let Some(handle) = frame.external_handle() else {
            // CPU pixels, a whole frame: each region comes from its own position.
            self.with(|state| {
                for region in regions {
                    state.upload(surface_id, region, frame.data(), frame.width(), (0, 0));
                }
            });
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
            for region in regions {
                let Some(region) = clip(
                    region,
                    width.min(surface.target.width),
                    height.min(surface.target.height),
                ) else {
                    continue;
                };
                blit(
                    &state.gl,
                    Some(video),
                    Some(&surface.target.framebuffer),
                    blit_rect(&region),
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

    fn end_frame(&mut self) {
        self.with(|state| state.present());
    }

    fn drain_output(&mut self) -> Vec<OutputUpdate> {
        Vec::new()
    }
}
