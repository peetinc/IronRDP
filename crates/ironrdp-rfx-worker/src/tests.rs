use ironrdp_pdu::geometry::ExclusiveRectangle;

use super::*;

const FIRST_PASS: &[u8] = include_bytes!("../../ironrdp-graphics/tests/data/win2022-progressive-first-pass.bin");
const UPGRADE_PASS: &[u8] = include_bytes!("../../ironrdp-graphics/tests/data/win2022-progressive-upgrade-pass.bin");
const WIDTH: u16 = 1280;
const HEIGHT: u16 = 720;

/// Draws `pixels` (tight, `width` wide) at `(left, top)` onto an RGBA surface.
fn draw(surface: &mut [u8], surface_width: u16, left: u16, top: u16, width: u16, height: u16, pixels: &[u8]) {
    let row = usize::from(width) * 4;
    for y in 0..usize::from(height) {
        let start = ((usize::from(top) + y) * usize::from(surface_width) + usize::from(left)) * 4;
        surface[start..start + row].copy_from_slice(&pixels[y * row..(y + 1) * row]);
    }
}

/// The client's rule (`GraphicsPipelineClient::write_progressive_tiles`): each tile clipped to
/// the surface, then to its update rectangles. Returns the rectangles in write order.
fn draw_tiles_as_the_client_does(
    surface: &mut [u8],
    surface_width: u16,
    surface_height: u16,
    tiles: &[DecodedTile],
) -> Vec<ExclusiveRectangle> {
    let mut written = Vec::new();
    for tile in tiles {
        let left = tile.x_idx * TILE_DIM;
        let top = tile.y_idx * TILE_DIM;
        let width = surface_width.saturating_sub(left).min(TILE_DIM);
        let height = surface_height.saturating_sub(top).min(TILE_DIM);
        if width == 0 || height == 0 {
            continue;
        }
        for rect in &tile.update_rectangles {
            let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
            let mut crop = Vec::new();
            for y in 0..usize::from(h) {
                let start =
                    ((usize::from(rect.top - top) + y) * usize::from(TILE_DIM) + usize::from(rect.left - left)) * 4;
                crop.extend_from_slice(&tile.pixels[start..start + usize::from(w) * 4]);
            }
            draw(surface, surface_width, rect.left, rect.top, w, h, &crop);
            written.push(rect.clone());
        }
    }
    written
}

/// Draws packed placements onto an RGBA surface; returns their rectangles.
fn draw_placements(
    surface: &mut [u8],
    surface_width: u16,
    pixels: &[u8],
    placements: &[i32],
) -> Vec<ExclusiveRectangle> {
    placements
        .chunks_exact(5)
        .map(|p| {
            let [left, top, width, height, offset] =
                [p[0], p[1], p[2], p[3], p[4]].map(|v| usize::try_from(v).unwrap());
            let [left, top, width, height] = [left, top, width, height].map(|v| u16::try_from(v).unwrap());
            draw(surface, surface_width, left, top, width, height, &pixels[offset..]);
            ExclusiveRectangle {
                left,
                top,
                right: left + width,
                bottom: top + height,
            }
        })
        .collect()
}

#[test]
fn pack_tiles_reproduces_the_clients_tile_writes() {
    let tiles = ProgressiveDecoder::new()
        .decode_bitmap(0, 1, WIDTH, HEIGHT, FIRST_PASS)
        .expect("first pass decodes");
    let (mut pixels, mut placements) = (Vec::new(), Vec::new());
    pack_tiles(WIDTH, HEIGHT, &tiles, &mut pixels, &mut placements);

    let surface_bytes = usize::from(WIDTH) * usize::from(HEIGHT) * 4;
    let (mut packed, mut reference) = (vec![0u8; surface_bytes], vec![0u8; surface_bytes]);
    let packed_rects = draw_placements(&mut packed, WIDTH, &pixels, &placements);
    let reference_rects = draw_tiles_as_the_client_does(&mut reference, WIDTH, HEIGHT, &tiles);

    assert!(!reference_rects.is_empty());
    assert_eq!(packed_rects, reference_rects, "same rectangles, same order");
    assert!(packed == reference, "same pixels");
}

#[test]
fn pack_tiles_clips_tiles_at_the_surface_edge() {
    let pixels_of_tile: Vec<u8> = (0..64u32 * 64)
        .flat_map(|i| [u8::try_from(i % 64).unwrap(), u8::try_from(i / 64).unwrap(), 7, 0xFF])
        .collect();
    let tile = DecodedTile {
        x_idx: 1,
        y_idx: 0,
        pixels: pixels_of_tile.clone(),
        update_rectangles: vec![ExclusiveRectangle {
            left: 64,
            top: 0,
            right: 100,
            bottom: 64,
        }],
    };
    let (mut pixels, mut placements) = (Vec::new(), Vec::new());
    pack_tiles(100, 100, &[tile], &mut pixels, &mut placements);

    assert_eq!(placements, [64, 0, 36, 64, 0]);
    assert_eq!(pixels.len(), 36 * 64 * 4);
    for y in 0..64 {
        assert_eq!(
            &pixels[y * 36 * 4..(y + 1) * 36 * 4],
            &pixels_of_tile[y * 64 * 4..y * 64 * 4 + 36 * 4]
        );
    }
}

#[test]
fn worker_decoder_matches_progressive_decoder() {
    let mut worker = WorkerDecoder::new();
    let mut reference = ProgressiveDecoder::new();
    for pass in [FIRST_PASS, UPGRADE_PASS] {
        worker.control(CONTROL_START_FRAME, 0, 0);
        worker.submit(0, 1, WIDTH, HEIGHT, pass).expect("worker decodes");
        worker.control(CONTROL_END_FRAME, 0, 0);

        reference.begin_frame();
        let tiles = reference
            .decode_bitmap(0, 1, WIDTH, HEIGHT, pass)
            .expect("reference decodes");
        reference.end_frame();
        let (mut pixels, mut placements) = (Vec::new(), Vec::new());
        pack_tiles(WIDTH, HEIGHT, &tiles, &mut pixels, &mut placements);

        assert!(!placements.is_empty());
        assert_eq!(worker.placements(), placements.as_slice());
        assert!(worker.pixels() == pixels.as_slice());
    }
}

#[test]
fn worker_decoder_reports_decode_errors_by_code() {
    let mut worker = WorkerDecoder::new();
    let code = worker
        .submit(0, 1, WIDTH, HEIGHT, &[0xFF; 32])
        .expect_err("garbage does not decode");
    assert!(
        error_reason(code).starts_with("rfx progressive decode failed"),
        "{}",
        error_reason(code)
    );
    assert!(worker.placements().is_empty(), "a failed job leaves no result");
}

/// The placements of the last result, each with its pixel bytes, sorted by rectangle.
fn placed(decoder: &WorkerDecoder) -> Vec<([i32; 4], Vec<u8>)> {
    let mut placed: Vec<([i32; 4], Vec<u8>)> = decoder
        .placements()
        .chunks_exact(5)
        .map(|p| {
            let start = usize::try_from(p[4]).expect("offset");
            let len = usize::try_from(p[2] * p[3] * 4).expect("length");
            ([p[1], p[0], p[2], p[3]], decoder.pixels()[start..start + len].to_vec())
        })
        .collect();
    placed.sort();
    placed
}

#[test]
fn sharded_workers_pack_the_unsharded_placements() {
    let mut whole = WorkerDecoder::new();
    let mut shards: Vec<WorkerDecoder> = (0..3).map(|index| WorkerDecoder::sharded(index, 3)).collect();
    for pass in [FIRST_PASS, UPGRADE_PASS] {
        whole.submit(0, 1, WIDTH, HEIGHT, pass).expect("unsharded decode");
        let mut union = Vec::new();
        for shard in &mut shards {
            shard.submit(0, 1, WIDTH, HEIGHT, pass).expect("sharded decode");
            union.extend(placed(shard));
        }
        union.sort();
        assert!(!union.is_empty());
        assert!(
            union == placed(&whole),
            "the shards' placements differ from the unsharded ones"
        );
    }
}
