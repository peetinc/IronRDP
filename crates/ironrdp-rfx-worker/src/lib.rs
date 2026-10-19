#![cfg_attr(doc, doc = include_str!("../README.md"))]

use ironrdp_graphics::progressive::{DecodedTile, ProgressiveDecodeError, ProgressiveDecoder, TILE_DIM};

/// [`WorkerDecoder::control`] kind: `RDPGFX_START_FRAME_PDU`.
pub const CONTROL_START_FRAME: u32 = 0;
/// [`WorkerDecoder::control`] kind: `RDPGFX_END_FRAME_PDU`.
pub const CONTROL_END_FRAME: u32 = 1;
/// [`WorkerDecoder::control`] kind: `RDPGFX_DELETE_ENCODING_CONTEXT_PDU`.
pub const CONTROL_DELETE_CONTEXT: u32 = 2;
/// [`WorkerDecoder::control`] kind: `RDPGFX_DELETE_SURFACE_PDU`.
pub const CONTROL_DELETE_SURFACE: u32 = 3;

/// Bytes per RGBA pixel.
const BYTES_PER_PIXEL: usize = 4;

/// Crops decoded tiles to what they paint on a `surface_width` by `surface_height` surface,
/// packed for upload: each placement is `[left, top, width, height, byte_offset]` into
/// `pixels`, whose rows are `width` pixels long with no padding.
///
/// The placements are exactly the rectangles `GraphicsPipelineClient` writes for in-thread
/// decoding, in the same order: each tile clipped to the surface, then to its update
/// rectangles. Both outputs are cleared first.
pub fn pack_tiles(
    surface_width: u16,
    surface_height: u16,
    tiles: &[DecodedTile],
    pixels: &mut Vec<u8>,
    placements: &mut Vec<i32>,
) {
    pixels.clear();
    placements.clear();
    let tile_stride = usize::from(TILE_DIM) * BYTES_PER_PIXEL;
    for tile in tiles {
        let tile_left = tile.x_idx.saturating_mul(TILE_DIM);
        let tile_top = tile.y_idx.saturating_mul(TILE_DIM);
        if surface_width.saturating_sub(tile_left) == 0 || surface_height.saturating_sub(tile_top) == 0 {
            continue;
        }
        for rect in &tile.update_rectangles {
            let (Some(x), Some(y)) = (rect.left.checked_sub(tile_left), rect.top.checked_sub(tile_top)) else {
                continue;
            };
            let width = rect.right.saturating_sub(rect.left);
            let height = rect.bottom.saturating_sub(rect.top);
            let Ok(offset) = i32::try_from(pixels.len()) else {
                return;
            };
            let row_bytes = usize::from(width) * BYTES_PER_PIXEL;
            let start = pixels.len();
            for row in 0..usize::from(height) {
                let source = (usize::from(y) + row) * tile_stride + usize::from(x) * BYTES_PER_PIXEL;
                let Some(row_pixels) = tile.pixels.get(source..source + row_bytes) else {
                    pixels.truncate(start);
                    break;
                };
                pixels.extend_from_slice(row_pixels);
            }
            if pixels.len() == start && 0 < row_bytes * usize::from(height) {
                // The rectangle reaches outside its tile: nothing to paint.
                continue;
            }
            placements.extend([
                i32::from(rect.left),
                i32::from(rect.top),
                i32::from(width),
                i32::from(height),
                offset,
            ]);
        }
    }
}

/// The code [`WorkerDecoder::submit`] reports for `error`: one per variant, from 1.
fn error_code(error: &ProgressiveDecodeError) -> u32 {
    match error {
        ProgressiveDecodeError::Pdu(_) => 1,
        ProgressiveDecodeError::Rlgr(_) => 2,
        ProgressiveDecodeError::Srl(_) => 3,
        ProgressiveDecodeError::MissingBlock(_) => 4,
        ProgressiveDecodeError::TileOutOfBounds { .. } => 5,
        ProgressiveDecodeError::InvalidQuantIndex { .. } => 6,
        ProgressiveDecodeError::MissingTileReference { .. } => 7,
        ProgressiveDecodeError::SurfaceTooLarge { .. } => 8,
    }
}

/// What an error code from [`WorkerDecoder::submit`] means, worded as the in-thread client
/// reports it.
pub fn error_reason(code: u32) -> &'static str {
    match code {
        1 => "rfx progressive decode failed: PDU parse",
        2 => "rfx progressive decode failed: RLGR",
        3 => "rfx progressive decode failed: SRL (upgrade tile)",
        4 => "rfx progressive decode failed: missing block",
        5 => "rfx progressive decode failed: tile out of surface bounds",
        6 => "rfx progressive decode failed: invalid quant index",
        7 => "rfx progressive decode failed: difference tile with no reference",
        8 => "rfx progressive decode failed: surface too large",
        _ => "rfx progressive decode failed",
    }
}

/// A progressive decoder for one Worker: decodes each bitmap and keeps its packed result
/// ([`pack_tiles`]) until the next.
#[derive(Default)]
pub struct WorkerDecoder {
    decoder: ProgressiveDecoder,
    pixels: Vec<u8>,
    placements: Vec<i32>,
}

impl WorkerDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decodes a `WireToSurface2` bitmap for a `width` by `height` surface and packs its
    /// tiles. On error, returns a code for [`error_reason`] and leaves no result.
    pub fn submit(
        &mut self,
        surface_id: u16,
        codec_context_id: u32,
        width: u16,
        height: u16,
        data: &[u8],
    ) -> Result<(), u32> {
        self.pixels.clear();
        self.placements.clear();
        let tiles = self
            .decoder
            .decode_bitmap(surface_id, codec_context_id, width, height, data)
            .map_err(|error| error_code(&error))?;
        pack_tiles(width, height, &tiles, &mut self.pixels, &mut self.placements);
        Ok(())
    }

    /// Applies a stream event (`CONTROL_*`) at its place among the bitmaps. Unknown kinds are
    /// ignored.
    pub fn control(&mut self, kind: u32, surface_id: u16, codec_context_id: u32) {
        match kind {
            CONTROL_START_FRAME => self.decoder.begin_frame(),
            CONTROL_END_FRAME => self.decoder.end_frame(),
            CONTROL_DELETE_CONTEXT => self.decoder.delete_context(surface_id, codec_context_id),
            CONTROL_DELETE_SURFACE => self.decoder.delete_surface(surface_id),
            _ => {}
        }
    }

    /// The last result's pixels.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// The last result's placements, five `i32`s each (see [`pack_tiles`]).
    pub fn placements(&self) -> &[i32] {
        &self.placements
    }
}

/// The module's exports, for `rfx_worker.js`. One decoder per instance.
#[cfg(target_arch = "wasm32")]
mod abi {
    use core::cell::RefCell;

    use super::WorkerDecoder;

    thread_local! {
        static DECODER: RefCell<WorkerDecoder> = RefCell::new(WorkerDecoder::new());
    }

    /// Reserves `len` bytes for the caller to fill, released with [`rfx_free`].
    #[unsafe(no_mangle)]
    pub(crate) extern "C" fn rfx_alloc(len: usize) -> *mut u8 {
        let mut buffer = core::mem::ManuallyDrop::new(Vec::<u8>::with_capacity(len));
        buffer.as_mut_ptr()
    }

    /// Releases memory from [`rfx_alloc`].
    ///
    /// # Safety
    ///
    /// `ptr` and `len` must be those of one earlier [`rfx_alloc`], released once.
    #[unsafe(no_mangle)]
    pub(crate) unsafe extern "C" fn rfx_free(ptr: *mut u8, len: usize) {
        // SAFETY: the caller passes back an allocation from `rfx_alloc` of capacity `len`.
        drop(unsafe { Vec::from_raw_parts(ptr, 0, len) });
    }

    /// Decodes the `len` bytes at `ptr`; 0 on success, otherwise an error code.
    ///
    /// # Safety
    ///
    /// `ptr` must point to `len` initialized bytes, as from [`rfx_alloc`] filled by the caller.
    #[unsafe(no_mangle)]
    pub(crate) unsafe extern "C" fn rfx_submit(surface: u32, context: u32, width: u32, height: u32, ptr: *const u8, len: usize) -> u32 {
        // SAFETY: the caller guarantees `len` initialized bytes at `ptr`.
        let data = unsafe { core::slice::from_raw_parts(ptr, len) };
        let narrow = |value: u32| u16::try_from(value).unwrap_or(u16::MAX);
        DECODER.with_borrow_mut(|decoder| {
            match decoder.submit(narrow(surface), context, narrow(width), narrow(height), data) {
                Ok(()) => 0,
                Err(code) => code,
            }
        })
    }

    /// Applies a stream event (`CONTROL_*`).
    #[unsafe(no_mangle)]
    pub(crate) extern "C" fn rfx_control(kind: u32, surface: u32, context: u32) {
        let surface = u16::try_from(surface).unwrap_or(u16::MAX);
        DECODER.with_borrow_mut(|decoder| decoder.control(kind, surface, context));
    }

    #[unsafe(no_mangle)]
    pub(crate) extern "C" fn rfx_pixels_ptr() -> *const u8 {
        DECODER.with_borrow(|decoder| decoder.pixels().as_ptr())
    }

    /// The last result's pixel bytes.
    #[unsafe(no_mangle)]
    pub(crate) extern "C" fn rfx_pixels_len() -> usize {
        DECODER.with_borrow(|decoder| decoder.pixels().len())
    }

    #[unsafe(no_mangle)]
    pub(crate) extern "C" fn rfx_placements_ptr() -> *const i32 {
        DECODER.with_borrow(|decoder| decoder.placements().as_ptr())
    }

    /// The last result's placement bytes (4 per `i32`).
    #[unsafe(no_mangle)]
    pub(crate) extern "C" fn rfx_placements_len() -> usize {
        DECODER.with_borrow(|decoder| size_of_val(decoder.placements()))
    }
}

#[cfg(test)]
mod tests;
