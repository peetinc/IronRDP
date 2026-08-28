//! Offline replay of a captured EGFX session.
//!
//! Feed it a directory of `egfx-*.bin` files captured with
//! `IRONRDP_DUMP_EGFX=<dir>` (raw, still-ZGFX-compressed server→client
//! messages — compressor state is sequential, so replaying them in order
//! through a fresh client reproduces the live session exactly: capabilities,
//! surface management, caches, fills, every codec).
//!
//! Every completed frame is composited into an RGBA canvas and written as a
//! PNG, so rendering bugs are visible — and bisectable — without a live
//! session, a browser, or credentials.
//!
//! Usage: egfx_replay <dump-dir> <out-dir> [--every N]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use ironrdp_dvc::DvcProcessor as _;
use ironrdp_egfx::client::{GraphicsPipelineClient, GraphicsPipelineHandler};

#[derive(Default)]
struct ReplayHandler {
    width: Arc<AtomicU32>,
    height: Arc<AtomicU32>,
    frames: Arc<AtomicU32>,
}

impl GraphicsPipelineHandler for ReplayHandler {
    fn on_reset_graphics(&mut self, width: u32, height: u32) {
        println!("ResetGraphics {width}x{height}");
        self.width.store(width, Ordering::Relaxed);
        self.height.store(height, Ordering::Relaxed);
    }

    fn on_frame_complete(&mut self, _frame_id: u32) {
        self.frames.fetch_add(1, Ordering::Relaxed);
    }
}

struct Canvas {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl Canvas {
    fn resize(&mut self, width: u32, height: u32) {
        if self.width != width || self.height != height {
            self.width = width;
            self.height = height;
            // Magenta base so "never painted" is unmistakable in the PNGs
            // (black is a common legitimate pixel value; magenta is not).
            self.rgba = vec![0; (width * height * 4) as usize];
            for px in self.rgba.chunks_exact_mut(4) {
                px.copy_from_slice(&[255, 0, 255, 255]);
            }
        }
    }

    fn apply(&mut self, region: &ironrdp_pdu::geometry::ExclusiveRectangle, data: &[u8]) {
        let (rw, rh) = (
            u32::from(region.right - region.left),
            u32::from(region.bottom - region.top),
        );
        for row in 0..rh {
            let dst_y = u32::from(region.top) + row;
            if dst_y >= self.height {
                break;
            }
            let dst_x = u32::from(region.left);
            let copy_w = rw.min(self.width.saturating_sub(dst_x));
            if copy_w == 0 {
                continue;
            }
            let src_off = (row * rw * 4) as usize;
            let dst_off = ((dst_y * self.width + dst_x) * 4) as usize;
            self.rgba[dst_off..dst_off + (copy_w * 4) as usize]
                .copy_from_slice(&data[src_off..src_off + (copy_w * 4) as usize]);
        }
    }

    fn save_png(&self, path: &std::path::Path) {
        let file = std::fs::File::create(path).expect("create png");
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), self.width, self.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&self.rgba).expect("png data");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dump_dir = args.get(1).expect("usage: egfx_replay <dump-dir> <out-dir> [--every N]");
    let out_dir = args.get(2).expect("usage: egfx_replay <dump-dir> <out-dir> [--every N]");
    let every: u32 = args
        .iter()
        .position(|a| a == "--every")
        .and_then(|i| args.get(i + 1))
        .and_then(|n| n.parse().ok())
        .unwrap_or(1);
    std::fs::create_dir_all(out_dir).expect("create out dir");

    let mut files: Vec<_> = std::fs::read_dir(dump_dir)
        .expect("read dump dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("egfx-") && n.ends_with(".bin"))
        })
        .collect();
    files.sort();
    println!("{} messages", files.len());

    let handler = ReplayHandler::default();
    let (width, height, frames) = (
        Arc::clone(&handler.width),
        Arc::clone(&handler.height),
        Arc::clone(&handler.frames),
    );
    let mut client = GraphicsPipelineClient::new(Box::new(handler), None);
    let _ = client.start(0).expect("start");

    let mut canvas = Canvas {
        width: 0,
        height: 0,
        rgba: Vec::new(),
    };
    let mut saved_after_frame = 0u32;

    for (i, path) in files.iter().enumerate() {
        let payload = std::fs::read(path).expect("read message");
        if let Err(error) = client.process(0, &payload) {
            // Keep going: a live session dies here, but for diagnostics one pass
            // should surface EVERY failure, and ZGFX state stays valid because
            // decompression happens before PDU handling.
            // Print the full source chain — the PduError Display alone hides the codec-level cause.
            let mut chain = format!("{error}");
            let mut source = core::error::Error::source(&error);
            while let Some(cause) = source {
                chain.push_str(&format!(" -> {cause}"));
                source = cause.source();
            }
            eprintln!("message {i} ({}): {chain}", path.display());
        }

        canvas.resize(width.load(Ordering::Relaxed), height.load(Ordering::Relaxed));
        for update in client.drain_output() {
            canvas.apply(&update.region, &update.data);
        }

        let completed = frames.load(Ordering::Relaxed);
        if completed > saved_after_frame && completed % every == 0 && canvas.width > 0 {
            canvas.save_png(&std::path::Path::new(out_dir).join(format!("frame-{completed:05}.png")));
            saved_after_frame = completed;
        }
    }

    if canvas.width > 0 {
        canvas.save_png(&std::path::Path::new(out_dir).join("final.png"));
    }
    println!(
        "{} frames completed, canvas {}x{}",
        frames.load(Ordering::Relaxed),
        canvas.width,
        canvas.height
    );
}
