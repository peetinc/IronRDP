# Web client: GPU rendering and parallel decode — design

Status: draft for review (2026-10-16). Follows [the WebCodecs H.264 design](2026-10-16-web-h264-webcodecs-design.md).

## Problem

At 3840x2160 the web client's main thread is saturated, so video stutters and audio gaps.
Windows App on the same host and network is smooth. Measured in Chrome against AC-WIN-micro
(Dell OptiPlex Micro Plus 7010, Intel UHD 770), with the sampling profiler, 10–12 s per sample:

| Host configuration | Busy | Screen updates/s | Page fps | Worst gap | Top cost |
|---|---|---|---|---|---|
| Default (mixed codecs), RemoteFX-heavy moment | 94% | 8.4 | 17.7 | 400 ms | RemoteFX tile decode ~65% |
| Default (mixed codecs), H.264-heavy moment | 92% | 25.6 | 49.7 | — | H.264 readback (`getImageData`) 45% |
| `AVC444ModePreferred=1` + `AVCHardwareEncodePreferred=1` (full-screen H.264) | 93% | 24.9 | 35 | 67 ms | H.264 readback 62% |

Two independent costs, both on the one main thread:

1. **Readback.** WebCodecs decodes H.264 on the GPU. The client then reads every frame back to
   the CPU, because the desktop image lives in wasm memory.
2. **RemoteFX Progressive decode.** In its default mode Windows sends most of the screen as
   RemoteFX Progressive (about 360 changed 64x64 tiles per 4K frame), and the client decodes
   all of it on the main thread. Only 0.4% of tile decodes are redundant, so this is real work.

The host policies remove cost 2 but not cost 1, and customer hosts run the default mode. The client
has to handle both.

## How Windows draws the screen

In the default mode Windows picks a codec per area of the screen, every frame:

| Content | Command |
|---|---|
| Video, fast-changing areas | `WireToSurface1` AVC420 / AVC444 (H.264) |
| Photos, gradients, wallpaper | `WireToSurface2` RemoteFX Progressive (blurry pass first, sharper passes after) |
| Text, window chrome, icons | `WireToSurface1` ClearCodec (lossless) |
| Flat rectangles | `SolidFill` |
| Scrolling, moved windows | `SurfaceToSurface` |
| Repeated bitmaps | `SurfaceToCache` / `CacheToSurface` |

All of them write into the same surfaces, in order, and `EndFrame` commits the frame.

## Today's pipeline

```
H.264 ──► WebCodecs (GPU) ──► canvas readback ──┐
RemoteFX ──► wasm decode (main thread) ─────────┼─► egfx Compositor (CPU surfaces)
ClearCodec, fills, copies, caches ──────────────┘        │ drain_output: copy dirty regions
                                                         ▼
                                     ActiveStage: copy into DecodedImage
                                                         │ extract_partial_image: copy
                                                         ▼
                                                 putImageData (2D canvas)
```

Every output pixel is copied four times on the CPU after it is decoded.

## Goals

- 4K at the video's frame rate (about 30 fps) for both host configurations, with the main thread
  mostly idle and audio uninterrupted.
- No change for native clients: the CPU compositor stays the default in `ironrdp-egfx`.
- No `SharedArrayBuffer`, so no cross-origin-isolation requirement on embedding pages
  (lithiumbridge).
- A fallback to today's path when WebGL2 or Workers are unavailable.

Non-goals: full AVC444 4:4:4 chroma reconstruction (the main view is shown, as today); WebGPU;
moving networking or the session loop off the main thread.

## Threads

| Thread | Owner | Work |
|---|---|---|
| Main | us | Network, input, EGFX parsing and ordering, ClearCodec/Planar, GL calls, audio scheduling |
| RemoteFX workers ×N | us | Progressive tile decode (RLGR, SRL, dequantize, inverse DWT, colour) |
| Audio rendering | browser | Plays the scheduled `AudioBufferSourceNode`s |
| Video decode | browser | WebCodecs H.264, in hardware where available |
| GPU | browser | Compositing, presentation |

N = clamp(`navigator.hardwareConcurrency` − 2, 1, 4). That is **up to 5 threads of ours**: the
main thread plus 4 decode workers. Four because a RemoteFX-heavy 4K frame takes about 4× what
one thread delivers today (8.4 updates/s measured; about 30 needed). SIMD (phase 4) adds headroom on top.

## Design

### A. Surface backend in `ironrdp-egfx`

`Compositor` becomes the default implementation of a new trait. `GraphicsPipelineClient` calls the
trait, not the CPU type:

```rust
pub trait SurfaceBackend {
    fn reset(&mut self, width: u32, height: u32);
    fn create_surface(&mut self, id: u16, width: u16, height: u16);
    fn delete_surface(&mut self, id: u16);
    fn map_surface(&mut self, id: u16, origin_x: u32, origin_y: u32);
    fn map_surface_scaled(&mut self, id: u16, origin_x: u32, origin_y: u32, target_width: u32, target_height: u32);
    fn write_rgba(&mut self, surface_id: u16, dest: &ExclusiveRectangle, rgba: &[u8]);
    fn write_frame(&mut self, surface_id: u16, regions: &[ExclusiveRectangle], frame: &DecodedFrame);
    fn solid_fill(&mut self, surface_id: u16, color: &Color, rects: &[ExclusiveRectangle]);
    fn surface_to_surface(&mut self, src: u16, dst: u16, src_rect: &ExclusiveRectangle, dst_points: &[Point]);
    fn surface_to_cache(&mut self, surface_id: u16, cache_slot: u16, src_rect: &ExclusiveRectangle);
    fn cache_to_surface(&mut self, cache_slot: u16, surface_id: u16, dst_points: &[Point]);
    fn evict_cache_entry(&mut self, cache_slot: u16);
    fn end_frame(&mut self);
    /// CPU backends return the frame's changed output regions; GPU backends present and return none.
    fn drain_output(&mut self) -> Vec<OutputUpdate>;
}
```

- `DecodedFrame` gains an external-pixels form: an opaque handle to a frame the backend owns
  (a WebCodecs `VideoFrame`). The CPU compositor never receives one, because the web session
  configures the GPU backend and the WebCodecs decoder together.
- The existing bounds (`MAX_SURFACE_DIM`, `MAX_COMPOSITOR_BYTES`, per-PDU rectangle limits)
  stay in `GraphicsPipelineClient` and apply to every backend.
- `ActiveStage` keeps draining CPU updates into `DecodedImage`. With a GPU backend there is
  nothing to drain for EGFX.

### B. WebGL2 backend in `ironrdp-web`

Module `gpu/` owns a WebGL2 context on the render canvas.

| Command | GPU operation |
|---|---|
| Surface, cache slot | RGBA8 texture with a framebuffer |
| `write_rgba` (ClearCodec, Planar, Uncompressed, RemoteFX tiles) | `texSubImage2D` from a view of wasm memory |
| `write_frame` (H.264) | `texImage2D(VideoFrame)` into a video texture, then one scissored draw per region into the surface; `VideoFrame.close()` afterwards |
| `solid_fill` | scissored `clear` per rectangle |
| `surface_to_surface` | `blitFramebuffer` (through a scratch texture when source and destination overlap) |
| `surface_to_cache`, `cache_to_surface` | `blitFramebuffer` |
| `end_frame` | draw each mapped surface to the canvas as a textured quad (scaled mappings scale), alpha forced opaque |

- No readback anywhere in steady state.
- The non-EGFX path (fast-path bitmaps into `DecodedImage`) uploads its dirty regions into an
  output texture with `texSubImage2D`, replacing `putImageData`. One canvas carries one context
  type, so this path moves to WebGL2 too.
- A surface larger than `MAX_TEXTURE_SIZE`, or a failed context creation, selects the CPU backend
  for the session.
- **Context loss** (`webglcontextlost`) destroys every surface and cache texture, and the server
  still believes its cache slots are valid. The session ends with a clear error, and the existing
  reconnect logic runs.

### C. RemoteFX Progressive in workers

`ProgressiveDecoder` splits along its natural seam:

- **Main thread (stream side):** block parsing, contexts, frame and REGION bookkeeping, and tile
  clipping (`TileCoverage`).
- **Workers (tile side):** `TileState` and difference references, first and upgrade passes, and
  `reconstruct_to_rgba`.

Flow:

1. Each tile goes to worker `hash(surface_id, x_idx, y_idx) % N`. The assignment is stable, so a tile
   and its difference reference always live in the same worker, and each worker's FIFO keeps
   per-tile order. Messages carry the tile bytes plus its quantization values and flags, which are
   small. Context and surface deletion and reset are broadcast to every worker.
2. Workers return each tile's RGBA as a transferred `ArrayBuffer` (16 KiB).
3. The `WireToSurface2` command is deferred until all its tiles are back, using the mechanism
   H.264 uses today. `DeferredAvc420` generalizes to a deferred command, so later PDUs wait in the
   backlog and frame acknowledgement paces the server.
4. The main thread keeps the RGBA of tiles decoded in the current frame. A later REGION that
   repaints them (the cross-context case) needs no worker round trip.

Packaging and failure:

- Each worker instantiates the same compiled `WebAssembly.Module`, posted from the main thread,
  with its own linear memory.
- The worker script ships in the `iron-remote-desktop-rdp` package and is loaded with
  `new Worker(new URL(...), { type: 'module' })`.
- If no worker can be created, decoding runs in-thread, as today.
- A worker error or a tile decode error drops that command's update and logs it, instead of ending
  the session.

### D. Build and kernels

- The wasm release build uses `-Copt-level=s` (`xtask/src/web.rs`), which limits inlining and
  auto-vectorization. It switches to `-Copt-level=3`; the bundle size change is measured and
  reported.
- SIMD kernels, written against FreeRDP's SSE2/NEON versions:
  - YCbCr→RGB in i16 fixed point; today it is scalar i64, one pixel at a time.
  - Row-major vertical inverse DWT across 8 columns; today it is per column, strided.
- Coefficients are stored fully dequantized, so `reconstruct_to_rgba` stops repeating the
  dequantize. This removes the separate reference copy, as FreeRDP does.
- Every kernel must be bit-exact against the scalar code (`ms_progressive` golden tests plus capture
  replays).

### E. Audio

Audio blocks are decoded and scheduled on the main thread, so a 400 ms main-thread stall
becomes an audible gap. Freeing the main thread is the fix. Phase 2 measures underruns before and
after, and only then considers a larger scheduling lead or an `AudioWorklet`.

## Measurement first: a replay bench

Every result so far needed a live login. Phase 0 makes measurement deterministic:

- A test-client page `/bench` loads a capture (the length-prefixed EGFX messages the rig already
  records).
- It drives the real wasm EGFX pipeline and renderer at real-time pacing or flat out.
- It reports ms per frame, main-thread busy %, frames/s, and the profiler's top functions.
- H.264 decodes for real through WebCodecs.

The captures from 2026-10-16 are the fixtures:

- 4K mixed, RemoteFX-heavy
- 4K mixed, H.264-heavy
- 4K full-screen AVC444
- the two sessions that hit the clipping budget

## Phases and acceptance

| Phase | Deliverable | Accepted when |
|---|---|---|
| 0 | Replay bench | Reproduces the live numbers above within ±15% |
| 1 | `opt-level=3` | Bench delta and bundle size reported |
| 2 | Surface backend trait + WebGL2 backend | Full-screen AVC444 capture: main thread under 25% busy at the capture's frame rate; no `getImageData`/`putImageData` in the profile; replayed non-video frames pixel-identical to the CPU compositor (read back in the test only) |
| 3 | RemoteFX workers | RemoteFX-heavy capture: tile decode throughput at least 3× single-thread; main thread under 40% busy; output pixel-identical |
| 4 | SIMD kernels and in-place coefficients | Kernels bit-exact; at least 2× on the RemoteFX profile slice |

Each phase ships separately behind the existing test-client toggles. A live session confirms each
phase after the bench does.

## Risks

- **`texImage2D(VideoFrame)` cost.** It is zero-copy in Chrome on macOS. Elsewhere it may copy on
  the GPU, which is still far cheaper than reading back to the CPU. Phase 2 measures it.
- **Worker round-trip latency** adds about 1–2 ms per RemoteFX command. Frames with many
  commands may need several commands in flight. v1 waits per command, and the bench shows
  whether that is enough.
- **Packaging.** The worker script in the npm package and inside lithiumbridge's bundler and CSP
  (`worker-src`).
- **API surface.** `SurfaceBackend` and external `DecodedFrame` are public API changes in
  `ironrdp-egfx`. They stay in this fork until proven.

## Decisions (2026-10-16)

1. WebGL context loss ends the session with a clear error, and the existing reconnect logic runs.
2. The two host policies were removed from AC-WIN-micro, so captures and live checks use the
   default configuration.
3. Workers are capped at 4.
