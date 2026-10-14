//! Prints every PDU of a captured EGFX session, one per line.
//!
//! Companion to `egfx_replay`: the replay shows WHAT went wrong on screen, this
//! shows which commands touched a region. Input is the same directory of
//! `egfx-*.bin` captures.
//!
//! Usage: egfx_trace <dump-dir>

use ironrdp_core::{ReadCursor, decode_cursor};
use ironrdp_egfx::pdu::GfxPdu;
use ironrdp_graphics::zgfx::Decompressor;

fn main() {
    let dump_dir = std::env::args().nth(1).expect("usage: egfx_trace <dump-dir>");

    let mut files: Vec<_> = std::fs::read_dir(&dump_dir)
        .expect("read dump dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("egfx-") && n.ends_with(".bin"))
        })
        .collect();
    files.sort();

    let mut decompressor = Decompressor::new();
    let mut buffer = Vec::new();

    for (i, path) in files.iter().enumerate() {
        let payload = std::fs::read(path).expect("read message");
        buffer.clear();
        if let Err(error) = decompressor.decompress(&payload, &mut buffer) {
            println!("{i:05} ZGFX error: {error}");
            continue;
        }
        let mut cursor = ReadCursor::new(&buffer);
        while !cursor.is_empty() {
            match decode_cursor::<GfxPdu>(&mut cursor) {
                Ok(pdu) => println!("{i:05} {pdu:?}"),
                Err(error) => {
                    println!("{i:05} decode error: {error}");
                    break;
                }
            }
        }
    }
}
