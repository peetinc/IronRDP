use core::num::NonZeroU32;

#[cfg(target_arch = "wasm32")]
use anyhow::anyhow;
use ironrdp::pdu::geometry::InclusiveRectangle;
#[cfg(target_arch = "wasm32")]
use ironrdp::pdu::geometry::Rectangle as _;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::{Clamped, JsCast as _};
#[cfg(target_arch = "wasm32")]
use web_sys::ImageData;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};

/// Web render surface: blits each dirty region to the canvas with `put_image_data`.
pub(crate) struct Canvas {
    canvas: HtmlCanvasElement,
    ctx: CanvasRenderingContext2d,
}

impl Canvas {
    pub(crate) fn new(render_canvas: HtmlCanvasElement, width: NonZeroU32, height: NonZeroU32) -> anyhow::Result<Self> {
        render_canvas.set_width(width.get());
        render_canvas.set_height(height.get());
        let ctx = context_2d(&render_canvas)?;

        Ok(Self {
            canvas: render_canvas,
            ctx,
        })
    }

    /// Current backing store dimensions, in pixels.
    pub(crate) fn size(&self) -> (u32, u32) {
        (self.canvas.width(), self.canvas.height())
    }

    /// Resizes the backing store. Note: this also clears the canvas and resets 2D context state;
    /// the cached `ctx` stays valid.
    pub(crate) fn resize(&mut self, width: NonZeroU32, height: NonZeroU32) {
        self.canvas.set_width(width.get());
        self.canvas.set_height(height.get());
    }

    /// Every `step`th pixel of every `step`th row, as RGB, top row first (the bench's
    /// CPU-against-GPU comparison).
    pub(crate) fn sample(&self, step: u32) -> Vec<u8> {
        #[cfg(target_arch = "wasm32")]
        {
            let (width, height) = self.size();
            self.ctx
                .get_image_data(0.0, 0.0, f64::from(width), f64::from(height))
                .map(|image| crate::gpu::sample_rgb(&image.data(), width, height, step, false))
                .unwrap_or_default()
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (&self.ctx, step);
            unimplemented!("web canvas is only available on wasm32")
        }
    }

    /// Blits a dirty region with `put_image_data`. Forces alpha opaque first: the framebuffer isn't
    /// guaranteed opaque (zero-init columns, QOI-RGBA) and `put_image_data` stores alpha verbatim.
    pub(crate) fn draw(&self, buffer: &mut [u8], region: InclusiveRectangle) -> anyhow::Result<()> {
        for pixel in buffer.chunks_exact_mut(4) {
            pixel[3] = 0xFF;
        }

        #[cfg(target_arch = "wasm32")]
        {
            let image = ImageData::new_with_u8_clamped_array_and_sh(
                Clamped(&*buffer),
                u32::from(region.width()),
                u32::from(region.height()),
            )
            .map_err(|err| anyhow!("ImageData::new failed: {err:?}"))?;
            self.ctx
                .put_image_data(&image, f64::from(region.left), f64::from(region.top))
                .map_err(|err| anyhow!("put_image_data failed: {err:?}"))
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (&self.ctx, buffer, region);
            unimplemented!("web canvas is only available on wasm32")
        }
    }
}

/// Where the session draws: the 2D canvas, or the WebGL2 backend when the session renders
/// EGFX on the GPU (a canvas carries one kind of context, so the choice is made once).
pub(crate) enum Presenter {
    Canvas2d(Canvas),
    Gl(crate::gpu::GlBackend),
}

impl Presenter {
    /// Current backing store dimensions, in pixels.
    pub(crate) fn size(&self) -> (u32, u32) {
        match self {
            Self::Canvas2d(canvas) => canvas.size(),
            Self::Gl(gl) => gl.size(),
        }
    }

    pub(crate) fn resize(&mut self, width: NonZeroU32, height: NonZeroU32) {
        match self {
            Self::Canvas2d(canvas) => canvas.resize(width, height),
            Self::Gl(gl) => gl.resize_output(width.get(), height.get()),
        }
    }

    /// Draws a dirty region that did not come through the GPU backend.
    pub(crate) fn draw(&self, buffer: &mut [u8], region: InclusiveRectangle) -> anyhow::Result<()> {
        match self {
            Self::Canvas2d(canvas) => canvas.draw(buffer, region),
            Self::Gl(gl) => {
                gl.present_rgba(&region, buffer);
                Ok(())
            }
        }
    }

    /// Why GPU rendering stopped, if it has: the session cannot continue.
    pub(crate) fn failure(&self) -> Option<&'static str> {
        match self {
            Self::Canvas2d(_) => None,
            Self::Gl(gl) => gl.failure(),
        }
    }
}

/// Acquires the canvas 2D context (wasm only; panics on other targets).
fn context_2d(canvas: &HtmlCanvasElement) -> anyhow::Result<CanvasRenderingContext2d> {
    #[cfg(target_arch = "wasm32")]
    {
        canvas
            .get_context("2d")
            .map_err(|err| anyhow!("get_context(\"2d\") failed: {err:?}"))?
            .ok_or_else(|| anyhow!("canvas has no 2d context"))?
            .dyn_into::<CanvasRenderingContext2d>()
            .map_err(|_| anyhow!("2d context is not a CanvasRenderingContext2d"))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = canvas;
        unimplemented!("web canvas is only available on wasm32")
    }
}
