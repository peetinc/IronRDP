# Web GPU rendering — implementation plan (phases 0–2)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Measure the web client deterministically, then render EGFX on the GPU so H.264 frames are never read back.

**Architecture:**
- A wasm-exported replay bench drives the real EGFX pipeline from a capture.
- `ironrdp-egfx` gains a `SurfaceBackend` trait, with the CPU `Compositor` as the default implementation.
- `ironrdp-web` adds a WebGL2 backend that keeps surfaces as textures and draws WebCodecs `VideoFrame`s straight into them.

**Tech Stack:** Rust 2024 (wasm32), wasm-bindgen, web-sys WebGL2, WebCodecs, SvelteKit test client.

**Spec:** [2026-10-16-web-gpu-rendering-design.md](2026-10-16-web-gpu-rendering-design.md). Phases 3 (RemoteFX workers) and 4 (SIMD) get their own plan, written from the phase 0–2 bench results.

## Global Constraints

- No change for native clients: the CPU `Compositor` stays the default in `ironrdp-egfx`.
- No `SharedArrayBuffer`.
- The CPU path stays available. It is used when WebGL2 is unavailable, a surface exceeds `MAX_TEXTURE_SIZE`, or the toggle is off.
- WebGL context loss ends the session with a clear error.
- Existing bounds (`MAX_SURFACE_DIM`, `MAX_COMPOSITOR_BYTES`, per-PDU rectangle limits) apply to every backend.
- Repo style (STYLE.md):
  - Log messages capitalized, with no trailing period.
  - Error messages lowercase.
  - No single-use helpers.
  - Test modules at the end of the file.
- Captures are user screen content. They live in the git-ignored `web-client/iron-svelte-client/static/bench/` and are never committed.

## Rulings

- **Bench pacing.** Captures carry no timestamps, so the bench runs flat out and reports sustained throughput (frames/s, ms per frame). The spec's "within ±15% of live" acceptance becomes: the live update rate must not exceed the bench's sustained rate on the same capture. Cost if wrong: the bench may under-represent network-paced stalls, which live checks still cover.
- **Bench fidelity.** The bench presents CPU output with a single copy into a staging image plus `putImageData`. The live session copies twice more, through `DecodedImage`. So the bench slightly flatters the CPU path; the GPU gain it shows is a lower bound.
- **Handler hook.** For external (GPU) H.264 frames, `GraphicsPipelineHandler::on_bitmap_updated` is not called, because there are no CPU pixels. Every other codec still calls it.

## Review Focus

1. A mid-session `ResetGraphics` resize (3840x2160 → 3454x1668, seen live) must recreate textures and resize the canvas without stale content. Covered by Task 5's verify run on a capture that contains one.
2. An overlapping `SurfaceToSurface` (scrolling) must copy as if through a temporary buffer. Covered by Task 4's `overlapping_copy_needs_scratch` test and the verify run.
3. WebGL context loss must end the session with an error instead of rendering garbage. Covered by Task 5's lose-context check.
4. A surface larger than `MAX_TEXTURE_SIZE` must fall back to the CPU backend. Covered by Task 5's `exceeds_texture_limit` test.
5. Output alpha must be opaque even where surfaces hold zero alpha. Covered by Task 5's verify comparison, which compares RGB with alpha forced to 255 on both sides.

---

### Task 1: Replay bench (phase 0)

**Files:**
- Create: `crates/ironrdp-web/src/bench.rs`
- Modify: `crates/ironrdp-web/src/lib.rs` (add `mod bench;`)
- Modify: `web-client/iron-remote-desktop-rdp/src/main.ts` (export `EgfxBench`)
- Create: `web-client/iron-svelte-client/src/routes/bench/+page.svelte`
- Modify: `web-client/iron-svelte-client/.gitignore` (add `static/bench/`)

**Interfaces:**
- Produces:
  - `fn split_capture(bytes: &[u8]) -> Result<Vec<&[u8]>, &'static str>`: u32 little-endian length, then the message. Truncated input is an error.
  - `#[wasm_bindgen] pub struct EgfxBench`:
    - `#[wasm_bindgen(js_name = create)] pub async fn create(canvas: HtmlCanvasElement, h264: bool, gpu: bool) -> Result<EgfxBench, JsValue>`
    - `pub async fn run(&mut self, capture: Uint8Array) -> Result<JsValue, JsValue>`, which returns `{ messages, frames, ms, fps, frameMsP50, frameMsP95, frameMsMax, decoderWaitMs }`
    - `pub fn checkpoint(&self) -> Result<Uint8Array, JsValue>`, which returns the presented output as RGBA, for verification.
  - Page `/bench?capture=<file>&h264=1&gpu=0` stores its result as `window.__benchResult`, wrapping `run` with the JS `Profiler` (when available) to add `busyPct` and the top 10 self-time functions.

- [ ] **Step 1: Write failing tests** in `bench.rs`:
  - `split_capture_reads_length_prefixed_messages`: `[2,0,0,0,0xAA,0xBB,1,0,0,0,0xCC]` gives `[[0xAA,0xBB],[0xCC]]`.
  - `split_capture_rejects_a_truncated_message`: `[3,0,0,0,0xAA]` is an error.
- [ ] **Step 2:** Run `cargo test -p ironrdp-web --lib bench`. Expected: FAIL (`split_capture` not found).
- [ ] **Step 3: Implement** `split_capture` and `EgfxBench`.
  - `run` calls `GraphicsPipelineClient::process(0, message)` for each message.
  - While `is_awaiting_decoder()`, it awaits the decoder's wake on a `futures::channel::mpsc` receiver, then calls `resume()`.
  - After each message it presents `drain_output()`: each update is copied into a staging RGBA image the size of the output, then `putImageData` is called on its rectangle. With the GPU backend, nothing is drained.
  - Frames are timed with `performance.now()` between `on_frame_complete` callbacks, counted in a `Rc<Cell<_>>` the handler shares.
  - The handler records `ResetGraphics` sizes and resizes the canvas.
- [ ] **Step 4:** Run `cargo test -p ironrdp-web --lib bench`, then `cargo xtask wasm check -v`. Expected: PASS, and no warnings in `bench.rs`.
- [ ] **Step 5: Bench page.**
  - Fetches `/bench/<capture>`, calls `init('info')`, creates the bench, and runs it under the profiler.
  - Renders the JSON and sets `window.__benchResult`.
  - Copy today's captures into `static/bench/`:
    - `rfx-heavy.bin`: the 18:00 session, the one that hit the clipping budget.
    - `mixed-17-31.bin`
    - `fail-start.bin`
- [ ] **Step 6: Verify in Chrome.** On `/bench?capture=mixed-17-31.bin&h264=1`, the result has frames > 0, fps > 0 and busyPct reported. Record the baseline for each capture in the ledger.
- [ ] **Step 7: Commit.** `feat(web): EGFX replay bench`.

### Task 2: Speed build (phase 1)

**Files:**
- Modify: `xtask/src/web.rs:58` (`-Copt-level=s` → `-Copt-level=3`)

- [ ] **Step 1:** Record the current `ironrdp_web_bg.wasm` size and the bench baselines from Task 1.
- [ ] **Step 2:** Change the flag, then run `cargo xtask web build -v` and `node ./pre-build.js no-wasm`.
- [ ] **Step 3:** Re-run the bench on all three captures. Expected: fps does not drop on any of them. Record the fps change and the wasm size change.
- [ ] **Step 4:** Commit with the numbers in the message. If fps drops on every capture, revert and record a ruling instead.

### Task 3: `SurfaceBackend` trait (phase 2)

**Files:**
- Modify: `crates/ironrdp-egfx/src/compositor.rs` (trait plus `impl SurfaceBackend for Compositor`)
- Modify: `crates/ironrdp-egfx/src/decode.rs` (external frames)
- Modify: `crates/ironrdp-egfx/src/client.rs` (hold `Box<dyn SurfaceBackend>`; `with_surface_backend`; AVC apply branch)

**Interfaces:**
- Produces:
  - `pub trait SurfaceBackend`, exactly as in spec §A. `write_rgba` is today's `apply_bitmap`. `write_frame(surface_id, regions: &[ExclusiveRectangle], frame: &DecodedFrame)`.
  - `DecodedFrame::external(handle: u64, width: u32, height: u32) -> Self`
  - `DecodedFrame::external_handle(&self) -> Option<u64>`
  - `data()` returns an empty slice for external frames.
  - `GraphicsPipelineClient::with_surface_backend(self, backend: Box<dyn SurfaceBackend>) -> Self`
- Behaviour:
  - In `apply_avc420` and `decode_avc444`, an external frame calls `backend.write_frame` with the clipped regions and skips the per-region copies and `on_bitmap_updated`. RGBA frames are unchanged.
  - `Compositor::write_frame` with an external frame logs a warning and ignores it.

- [ ] **Step 1: Write failing tests** in `client.rs`, with a `RecordingBackend` that records calls into `Rc<RefCell<Vec<String>>>`:
  - `external_avc420_frame_goes_to_the_backend_with_its_regions`: a deferred decoder that returns `DecodedFrame::external(7, 64, 64)` produces `write_frame(surface, [regions], handle=7)` and no `write_rgba`.
  - `commands_reach_a_custom_backend_in_order`: `SolidFill`, `SurfaceToSurface` and `CacheToSurface` are recorded in PDU order.
  - `default_backend_renders_as_before`: the existing compositor tests still pass unchanged.
- [ ] **Step 2:** Run `cargo test -p ironrdp-egfx`. Expected: FAIL to compile (`with_surface_backend` not found).
- [ ] **Step 3: Implement.** `GraphicsPipelineClient` holds `backend: Box<dyn SurfaceBackend>`, defaulting to `Box::new(Compositor::default())`, and every `self.compositor.*` call goes through it.
- [ ] **Step 4:** Run `cargo test -p ironrdp-egfx` and `cargo test -p ironrdp-testsuite-core egfx`. Then run `cargo clippy -p ironrdp-egfx --all-targets`. Expected: all pass, and no new warnings.
- [ ] **Step 5:** Native replay of `splitlive` is pixel-identical to the previous build: compare PNGs with `magick compare -metric AE`, expecting 0 on all of them.
- [ ] **Step 6: Commit.** `feat(egfx): pluggable surface backend`.

### Task 4: WebGL2 backend (phase 2)

**Files:**
- Create: `crates/ironrdp-web/src/gpu/mod.rs` (backend), `crates/ironrdp-web/src/gpu/geometry.rs` (pure maths), `crates/ironrdp-web/src/gpu/frames.rs` (registry of `VideoFrame`s by handle)
- Modify: `crates/ironrdp-web/Cargo.toml` (web-sys features):
  - `WebGl2RenderingContext`
  - `WebGlTexture`
  - `WebGlFramebuffer`
  - `WebGlProgram`
  - `WebGlShader`
  - `WebGlBuffer`
  - `WebGlUniformLocation`
  - `WebGlVertexArrayObject`
  - `WebGlContextAttributes`
  - `WebGlContextEvent`
- Modify: `crates/ironrdp-web/src/h264.rs`:
  - `WebCodecsH264Decoder::new(wake, output: FrameOutput)`, with `enum FrameOutput { Readback, Gpu }`.
  - With `Gpu`, the output callback stores the `VideoFrame` in `frames` and finishes with `DecodedFrame::external(handle, w, h)`, with no readback.

**Interfaces:**
- Produces:
  - `pub(crate) struct GlBackend`, which implements `SurfaceBackend`.
  - `GlBackend::new(canvas: &HtmlCanvasElement, lost: Rc<Cell<bool>>) -> anyhow::Result<GlBackend>`
  - `fn max_texture_size(&self) -> u32`
  - `frames::insert(frame: VideoFrame) -> u64`
  - `frames::take(handle: u64) -> Option<VideoFrame>`
  - `geometry::gl_scissor(rect: &ExclusiveRectangle, target_height: u16) -> (i32, i32, i32, i32)`: GL's origin is bottom-left.
  - `geometry::overlaps(a: &ExclusiveRectangle, b: &ExclusiveRectangle) -> bool`
  - `geometry::quad_for_mapping(origin: (u32, u32), size: (u32, u32), output: (u32, u32)) -> [f32; 8]`: clip-space corners.
- Behaviour: as in spec §B.
  - Surfaces are RGBA8 textures with framebuffers.
  - `write_rgba` uses `texSubImage2D` from a `Uint8Array` view of the slice.
  - `write_frame`:
    - `texImage2D(VideoFrame)` into one reused video texture;
    - one scissored textured draw per region into the surface framebuffer;
    - then `VideoFrame.close()`.
  - `solid_fill` uses a scissored `clear`.
  - Copies use `blitFramebuffer`, going through a scratch texture when the source and destination are the same surface and overlap.
  - `end_frame` draws the mapped surfaces to the default framebuffer with a shader that forces alpha to 1.
  - `drain_output` returns an empty `Vec`.

- [ ] **Step 1: Write failing host tests** in `geometry.rs`:
  - `scissor_flips_y_to_gl_origin`: rect (10,20)-(30,50) on a 100-high target gives `(10, 50, 20, 30)`.
  - `overlapping_copy_needs_scratch`: (0,0)-(10,10) and (5,5)-(15,15) overlap; (0,0)-(10,10) and (10,0)-(20,10) don't.
  - `quad_covers_the_whole_output_for_an_origin_mapping`: origin (0,0), size equal to output, gives corners at ±1.
  - `frames_registry_hands_out_each_frame_once`: lives in `frames.rs` and is tested through a generic inner store, so it runs on the host.
- [ ] **Step 2:** Run `cargo test -p ironrdp-web --lib gpu`. Expected: FAIL (`not found`).
- [ ] **Step 3:** Implement `geometry.rs`, `frames.rs`, `GlBackend` and `FrameOutput`.
- [ ] **Step 4:** Run `cargo test -p ironrdp-web --lib gpu h264`, then `cargo xtask wasm check -v`. Expected: PASS, and no warnings in the new files.
- [ ] **Step 5: Commit.** `feat(web): WebGL2 surface backend`.

### Task 5: Wire GPU rendering into the session and bench, then verify (phase 2)

**Files:**
- Modify: `crates/ironrdp-web/src/session.rs`:
  - `gpu_rendering` builder field and `|gpu_rendering: bool|` extension;
  - pick `GlBackend` plus `FrameOutput::Gpu` when enabled and creatable;
  - otherwise `Canvas` plus `Readback`.
- Modify: `crates/ironrdp-web/src/canvas.rs`. Introduce `enum Presenter { Canvas2d(Canvas), Gl(Rc<RefCell<GlBackend>>) }`, used by the session's `GraphicsUpdate` path:
  - `Canvas2d` does what it does today.
  - `Gl` uploads the region into the output texture and presents it.
- Modify: `crates/ironrdp-web/src/bench.rs`: `gpu` selects the same backend, and `checkpoint` reads back with `readPixels` (test only).
- Modify: `web-client/iron-remote-desktop-rdp/src/main.ts` (`export function gpuRendering(enable)`) and the test client (a "GPU rendering (WebGL2)" checkbox, preset `VITE_RIG_GPU`).

**Interfaces:**
- Consumes: Task 3 `with_surface_backend` and `DecodedFrame::external`; Task 4 `GlBackend` and `FrameOutput`.
- Produces: `fn exceeds_texture_limit(width: u16, height: u16, max_texture_size: u32) -> bool`, in `gpu/geometry.rs`.

Behaviour:
- `webglcontextlost` sets `lost`, and the session loop ends with `"GPU context lost"`.
- Any surface where `exceeds_texture_limit` is true at `create_surface` ends EGFX on the GPU and logs `"Surface exceeds the GPU texture limit"`. The session then reconnects on the CPU path.

- [ ] **Step 1: Write failing tests.** `exceeds_texture_limit_rejects_larger_surfaces`: (8193, 100, 8192) is true; (3840, 2160, 8192) is false.
- [ ] **Step 2:** Run `cargo test -p ironrdp-web --lib gpu`. Expected: FAIL.
- [ ] **Step 3:** Implement the wiring.
- [ ] **Step 4: Verify mode in the bench.** `/bench?capture=X&verify=1` replays the capture twice, once on CPU and once on GPU.
  - At every 100th frame it compares `checkpoint()` RGB, with alpha forced to 255 on both sides.
  - It reports `mismatchedPixels` per checkpoint.
  - Expected: 0 on every non-video checkpoint for `mixed-17-31.bin` and `fail-start.bin`, including after the 3454x1668 reset.
- [ ] **Step 5: Lose-context check.** In the bench, calling `WEBGL_lose_context.loseContext()` mid-run makes `run` reject with `"GPU context lost"`.
- [ ] **Step 6: Bench acceptance.** GPU against CPU on each capture. Expected: no `getImageData`/`putImageData` in the GPU profile.
- [ ] **Step 7: Live acceptance.** One live 4K session with H.264 and GPU on, default host configuration: record busy %, updates/s and worst gap against the spec table.
- [ ] **Step 8: Build and commit.**
  - Run `cargo xtask web build -v` and commit the dist with `DIST_SOURCE_COMMIT`.
  - Commit `feat(web): render EGFX on the GPU`.
