//! Replay of progressive RemoteFX streams captured from a real Windows host.
//!
//! Captured 2026-08-28 from a Windows Server (1280x720 surface, EGFX V8, no
//! AVC) via the `IRONRDP_DUMP_PROGRESSIVE` hook in `ironrdp-egfx`: frame 0 is
//! the initial full pass, frame 1 the first upgrade pass over the same codec
//! context.
//!
//! These are the first fixtures produced by an encoder other than this crate's
//! own. The decoder round-trips its own encoder cleanly while carrying framing
//! conventions the spec does not define, so self-round-trip coverage proved
//! nothing about interop: the upgrade pass here failed two different ways
//! (a demanded SRL terminator byte, then aggregated zero-run reads) before
//! rendering correctly. Any change to the progressive decoder must keep these
//! replays green.

use ironrdp_graphics::progressive::ProgressiveDecoder;

const FIRST_PASS: &[u8] = include_bytes!("data/win2022-progressive-first-pass.bin");
const UPGRADE_PASS: &[u8] = include_bytes!("data/win2022-progressive-upgrade-pass.bin");

#[test]
fn decodes_windows_first_pass() {
    let mut decoder = ProgressiveDecoder::new();
    let tiles = decoder
        .decode_bitmap(0, 1, 1280, 720, FIRST_PASS)
        .expect("Windows first-pass stream must decode");
    assert!(!tiles.is_empty(), "first pass must produce tiles");
}

#[test]
fn decodes_windows_upgrade_pass_after_first() {
    let mut decoder = ProgressiveDecoder::new();
    decoder
        .decode_bitmap(0, 1, 1280, 720, FIRST_PASS)
        .expect("Windows first-pass stream must decode");
    let tiles = decoder
        .decode_bitmap(0, 1, 1280, 720, UPGRADE_PASS)
        .expect("Windows upgrade-pass stream must decode");
    assert!(!tiles.is_empty(), "upgrade pass must produce tiles");
}
