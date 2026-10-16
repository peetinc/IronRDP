//! Summarises the H.264 quantization of a captured EGFX session.
//!
//! Reads a capture file of length-prefixed messages (`u32` little-endian length, then the raw
//! ZGFX-compressed EGFX message) and prints, per AVC codec, the QP and quality values the
//! server put in each `RFX_AVC420_METABLOCK`, plus H.264 bytes and region area. A high QP
//! means the server's encoder is compressing hard.
//!
//! Usage: egfx_qp <capture.bin>

use std::collections::BTreeMap;

use ironrdp_core::{Decode as _, ReadCursor, decode_cursor};
use ironrdp_egfx::pdu::{Avc420BitmapStream, Avc444BitmapStream, Codec1Type, GfxPdu};
use ironrdp_graphics::zgfx::Decompressor;

#[derive(Default)]
struct Stats {
    updates: u64,
    qp: BTreeMap<u8, u64>,
    quality_sum: u64,
    rects: u64,
    area: u64,
    h264_bytes: u64,
}

impl Stats {
    fn add(&mut self, stream: &Avc420BitmapStream<'_>) {
        for q in &stream.quant_qual_vals {
            *self.qp.entry(q.quantization_parameter).or_default() += 1;
            self.quality_sum += u64::from(q.quality);
        }
        for r in &stream.rectangles {
            self.rects += 1;
            self.area += u64::from(r.right - r.left) * u64::from(r.bottom - r.top);
        }
        self.h264_bytes += u64::try_from(stream.data.len()).unwrap_or(0);
    }
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: egfx_qp <capture.bin>");
    let capture = std::fs::read(&path).expect("read capture");

    let mut decompressor = Decompressor::new();
    let mut buffer = Vec::new();
    let mut stats: BTreeMap<String, Stats> = BTreeMap::new();
    let mut frames = 0u64;
    let mut rest = capture.as_slice();

    while let Some((length, after)) = rest.split_first_chunk::<4>() {
        let length = usize::try_from(u32::from_le_bytes(*length)).expect("length");
        let (payload, after) = after.split_at(length.min(after.len()));
        rest = after;

        buffer.clear();
        if decompressor.decompress(payload, &mut buffer).is_err() {
            continue;
        }
        let mut cursor = ReadCursor::new(&buffer);
        while !cursor.is_empty() {
            let Ok(pdu) = decode_cursor::<GfxPdu>(&mut cursor) else { break };
            match pdu {
                GfxPdu::EndFrame(_) => frames += 1,
                GfxPdu::WireToSurface1(pdu) => {
                    let mut data = ReadCursor::new(&pdu.bitmap_data);
                    match pdu.codec_id {
                        Codec1Type::Avc420 => {
                            if let Ok(stream) = Avc420BitmapStream::decode(&mut data) {
                                let entry = stats.entry("AVC420".to_owned()).or_default();
                                entry.updates += 1;
                                entry.add(&stream);
                            }
                        }
                        Codec1Type::Avc444 | Codec1Type::Avc444v2 => {
                            if let Ok(stream) = Avc444BitmapStream::decode(&mut data) {
                                let name = format!("{:?} LC={}", pdu.codec_id, stream.encoding.bits());
                                let entry = stats.entry(format!("{name} luma/main")).or_default();
                                entry.updates += 1;
                                entry.add(&stream.stream1);
                                if let Some(chroma) = &stream.stream2 {
                                    let entry = stats.entry(format!("{name} chroma")).or_default();
                                    entry.updates += 1;
                                    entry.add(chroma);
                                }
                            }
                        }
                        other => {
                            *stats.entry(format!("{other:?}")).or_default() = Stats {
                                updates: stats.get(&format!("{other:?}")).map_or(0, |s| s.updates) + 1,
                                ..Stats::default()
                            };
                        }
                    }
                }
                _ => {}
            }
        }
    }

    println!("frames: {frames}");
    for (name, s) in &stats {
        let qp_total: u64 = s.qp.values().sum();
        let qp_mean = if qp_total == 0 {
            0.0
        } else {
            s.qp.iter().map(|(qp, n)| f64::from(*qp) * *n as f64).sum::<f64>() / qp_total as f64
        };
        println!(
            "{name}: updates={} rects={} mean_rect_px={} h264_kb={} qp_mean={qp_mean:.1} quality_mean={:.1}",
            s.updates,
            s.rects,
            s.area / s.rects.max(1),
            s.h264_bytes / 1024,
            s.quality_sum as f64 / qp_total.max(1) as f64,
        );
        if !s.qp.is_empty() {
            println!("    qp histogram: {:?}", s.qp);
        }
    }
}
