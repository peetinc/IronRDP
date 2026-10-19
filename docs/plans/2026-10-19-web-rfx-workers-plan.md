# RemoteFX Progressive in Web Workers: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Measure before building (stage 0). Stage 0 settles the main-thread baseline and which
GPU upload path stage 2 builds. Stages 1–4 are outlined only, and get detailed tasks once stage 0's
numbers are in.

**Architecture:**
- **Replay bench** (`crates/ironrdp-web/src/bench.rs`): gains per-message main-thread time and
  GL upload time.
- **Standalone WebGL2 page** (`tools/webgl-upload-bench.html`): times four ways of uploading
  2,040 tiles to a 4K texture.
- **Results** decide the upload path, recorded in the spec.

**Tech Stack:** Rust/wasm-bindgen (ironrdp-web), WebGL2, plain HTML/JS.

**Spec:** `docs/plans/2026-10-19-web-rfx-workers-design.md`

## Global Constraints

- **Acceptance targets:**
  - main-thread time per task (inside `process` + `resume`) p95 under 50 ms, none over 150 ms;
  - RemoteFX decode throughput at least 3× today's in-thread rate (N = 4);
  - every tile byte-identical to in-thread decoding.
- **Heavy frame:** 2,040 tiles of 64×64 at 3840×2160.
- **Upload path rule:** per-placement `texSubImage2D(srcOffset)` is the default. A pixel-unpack
  buffer is next if per-call cost exceeds the budget; an atlas with an instanced draw is the
  last resort.
- **Build:** WASM needs
  `CC_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/clang AR_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/llvm-ar`;
  `cargo xtask web build -v`; dist commits update `web-client/DIST_SOURCE_COMMIT`.
- **Formatting:** `rustfmt --edition 2024` on my hunks only.
- **Never commit:** `web-client/iron-svelte-client/vite.config.ts` (TEMP profiler header) or
  `static/bench/` (captures).

## Review Focus

1. Timing must not change what it measures. The upload timer is off unless the bench turns it on,
   so sessions pay nothing.
2. A message that awaits the H.264 decoder must not count the wait as main-thread time.
3. The micro-bench must time issue cost (what the main thread pays), and separately the
   completion time. In Chrome the GPU work happens in another process.
4. Placement data in the micro-bench must be realistic (full 64×64 tiles, distinct per tile), so
   a driver cannot dedupe the uploads.
5. Results must name the machine and browser. A Mac number is not a Windows number.

---

### Task 1: Bench reports main-thread time per message and GL upload time

**Files:**
- Modify: `crates/ironrdp-web/src/bench.rs`
- Modify: `crates/ironrdp-web/src/gpu/mod.rs` (`upload_into`)
- Modify: `web-client/iron-svelte-client/src/routes/bench/+page.svelte` (comment only, if the
  usage line changes)

**Interfaces:**
- **Produces:** `bench.run` result fields:
  - `messageMsP50`, `messageMsP95`, `messageMsMax`;
  - `messagesOver50`, `messagesOver150`;
  - `uploadMs`, `uploadCalls`, `uploadMsMaxPerMessage`.

  "Message" means one capture message, the bench's stand-in for one WebSocket message, which is
  one task in a session. A message's time runs from before `client.process` to the end of its
  draws, minus any time awaiting the H.264 decoder.
- **Produces:** `pub(crate) fn percentiles(values: &mut [f64]) -> Percentiles` in `bench.rs`,
  with `struct Percentiles { p50: f64, p95: f64, max: f64 }`. It replaces the frame-time
  closure, by nearest rank on the sorted values, as today, and is all zeros for no values.
- **Produces:** in `gpu/mod.rs`:
  - `pub(crate) fn set_upload_timing(on: bool)`;
  - `pub(crate) fn take_upload_stats() -> (f64 /* ms */, u32 /* calls */)`.

  Both work on a thread-local. `upload_into` adds `performance.now()` deltas only while timing
  is on.

- [ ] **Step 1: Write the failing tests** in `bench.rs`'s test module:
  - `percentiles_of_nothing_are_zero`: `percentiles(&mut [])` is `{0,0,0}`.
  - `percentiles_use_nearest_rank_on_sorted_values`: for `[5.0, 1.0, 3.0, 2.0, 4.0]`, p50 3.0,
    p95 5.0, max 5.0.
  - `percentiles_ignore_input_order`: for 1.0..=100.0 shuffled, p50 51.0 (round((100-1)·0.5) =
    50, so the value at index 50), p95 95.0, max 100.0.
- [ ] **Step 2: Run them.** `cargo test -p ironrdp-web --lib bench`. Expected: compile failure,
  `percentiles` missing.
- [ ] **Step 3: Implement.**
  - `percentiles`, then use it for the frame times.
  - Per message: time `process` plus the resume/draw loop, minus `decoder_wait_ms` accrued
    within it.
  - Per message: upload ms, via `take_upload_stats()` after each message.
  - Turn timing on at `run` start and off at the end.
- [ ] **Step 4: Run.** `cargo test -p ironrdp-web --lib bench`. Expected: the new tests pass.
  Then
  `cargo clippy -p ironrdp-web --target wasm32-unknown-unknown` (with the CC/AR env). Expected:
  no new warnings in `bench.rs` or `gpu/mod.rs`.
- [ ] **Step 5: Commit.** `feat(web): bench reports main-thread time per message and GL upload time`.

### Task 2: Standalone WebGL2 upload micro-bench

**Files:**
- Create: `tools/webgl-upload-bench.html` (one self-contained file, no dependencies; opens from
  `file://` or any static server).

**Interfaces:**
- **Produces:** a page that runs on load and prints, then exposes as `window.__uploadBench`,
  `{ userAgent, renderer, variants: { A, B, C, D } }`.
  - Each variant has `{ issueMsP50, issueMsP95, doneMsP50, doneMsP95, calls }`, over 30 runs
    after 5 warm-up runs.
  - `renderer` comes from `WEBGL_debug_renderer_info` when available.
  - Query `?placements=N` (default 2040) and `?size=64`.

The setup and four variants, written exactly:
- **Setup:**
  - an RGBA8 3840×2160 texture via `texStorage2D`, attached to a framebuffer;
  - per run, 2,040 distinct 64×64 placements tiling the screen (row-major, 60×34);
  - pixel data from a seeded PRNG, regenerated per run outside the timed region.
- **A, today's pattern:** for each placement, `bindTexture`, `pixelStorei` ×3
  (`UNPACK_ROW_LENGTH` 64, `SKIP_PIXELS` 0, `SKIP_ROWS` 0), `texSubImage2D` from its own
  `Uint8Array`, then `pixelStorei` ×3 reset. 9 calls each.
- **B, tight srcOffset:** all placements in one `Uint8Array`, back to back. One `bindTexture`,
  then per placement `texSubImage2D(target, 0, x, y, 64, 64, RGBA, UNSIGNED_BYTE, buffer, offset)`.
- **C, pixel-unpack buffer:**
  - `bufferData` the whole packed buffer once into a `PIXEL_UNPACK_BUFFER` (`STREAM_DRAW`);
  - per placement, the `texSubImage2D(..., offset)` PBO overload;
  - unbind after.
- **D, atlas + instanced draw:**
  - Upload the packed buffer as one 64 × (64·2040)-tall texture: two uploads if over
    `MAX_TEXTURE_SIZE`, wrapping columns.
  - Then one instanced draw of 2,040 quads into the framebuffer. Per-instance destination and
    atlas offset come from an attribute buffer, with `vertexAttribDivisor` 1.

**Timing:** `issueMs` is `performance.now()` around the variant's calls. `doneMs` is issue plus
waiting for a `fenceSync` with `clientWaitSync` polled at 0 timeout, after `flush`.

- [ ] **Step 1: Write the page.**
- [ ] **Step 2: Verify that it measures what it claims.** Open it in Chrome with
  `?placements=4`, then read back the texture: `readPixels` from the framebuffer, a debug mode
  `?check=1`.
  - Expected: for every variant, the four placements' pixels equal the source bytes.
  - This check is the page's correctness test; record it in the ledger.
- [ ] **Step 3: Commit.** `chore(tools): WebGL2 tile-upload micro-bench`.

### Task 3: Measure and decide

- [ ] **Step 1: Build and serve.**
  1. Run `cargo xtask web build -v` with the CC/AR env.
  2. Copy `web-client/iron-remote-desktop-rdp/dist/.` into
     `web-client/iron-svelte-client/static/iron-remote-desktop-rdp/`.
  3. The vite dev server already runs on :5173.
- [ ] **Step 2: Bench baseline,** on this Mac in Chrome. Each URL is
  `/bench?capture=<c>&h264=1&gpu=1` for `c` in rfx-heavy.bin, live-local-4k.bin and
  mixed-17-31.bin. Record every result field in the ledger.
- [ ] **Step 3: Micro-bench,** on this Mac in Chrome, at `?placements=2040`. Record the variants
  in the ledger.
- [ ] **Step 4: Decide the upload path, by this rule:**
  - **Budget:** 15 ms of issue time for a 2,040-tile frame (p50). That leaves 35 ms of the
    50 ms p95 for zgfx, parsing and ClearCodec, which the codec breakdown puts well under 10 ms
    native.
  - **B** if B's `issueMsP50` ≤ 15 ms. Else **C** if ≤ 15 ms. Else **D**.
  - Record it as a Ruling in the ledger.
- [ ] **Step 5: Write up.** Append "Stage 0 results" to the spec: a table of the baseline and
  micro-bench numbers with the machine and browser, and the decision. Then commit:
  `docs: RemoteFX workers stage 0 results`.
- [ ] **Step 6: Ask the user for a Windows run.** Ask them to open `tools/webgl-upload-bench.html` in
  Chrome on a Windows machine, if convenient, and paste `window.__uploadBench`. If a Windows
  result changes the decision, re-rule.

---

## Stage 1 (detailed after stage 0)

Shared names, in `crates/ironrdp-egfx/src/decode.rs`:

```rust
pub enum ProgressiveControl { StartFrame, EndFrame, DeleteContext { surface_id: u16, codec_context_id: u32 }, DeleteSurface { surface_id: u16 } }
pub struct DecodedTiles { pub external: Option<u64>, pub tiles: Vec<ironrdp_graphics::progressive::DecodedTile> }
pub struct ProgressiveJobError { pub reason: &'static str }
pub trait ProgressiveBackend: Send {
    fn max_in_flight(&self) -> usize;
    fn submit(&mut self, job: u32, surface_id: u16, codec_context_id: u32, width: u16, height: u16, data: &[u8]);
    fn control(&mut self, control: ProgressiveControl);
    /// The result of `job` once complete; `None` while it decodes. Jobs complete in submission order.
    fn poll(&mut self, job: u32) -> Option<Result<DecodedTiles, ProgressiveJobError>>;
}
```

The client:
- **Builder:** `GraphicsPipelineClient::with_progressive_backend(self, Box<dyn ProgressiveBackend>) -> Self`.
- **Pending entries:**
  - `Pending::Progressive(DeferredProgressive { job, surface_id })`;
  - `Pending::Forwarded(GfxPdu)`: a progressive control already given to the backend by
    submit-ahead. When it applies, it is handled without forwarding it again.
- **Surface backend:** `SurfaceBackend::write_external_tiles(&mut self, surface_id: u16, handle: u64)`,
  with a default that warns.

### Task 4: Asynchronous progressive path, applied in stream order

**Files:**
- Modify: `crates/ironrdp-egfx/src/decode.rs` (the types above);
- Modify: `crates/ironrdp-egfx/src/compositor.rs` (`write_external_tiles` default);
- Modify: `crates/ironrdp-egfx/src/client.rs`;
- Tests: `client.rs` tests, with a `FakeProgressive` backend recording `submit`/`control` calls
  as strings and completing jobs the test chooses.

- [ ] **Step 1: Write the failing tests.**
  - `progressive_jobs_apply_in_stream_order_when_completed_out_of_order`. Two W2S2 commands
    for the 64×64 deferred surface. Job 2 completes first: nothing applies. Then job 1
    completes: both apply, and the pixel is job 2's.
  - `commands_behind_a_progressive_job_wait_for_it`. A W2S2 then a red SolidFill: while the
    job is pending the output is empty. After it, the fill is on top.
  - `a_failed_progressive_job_is_a_session_error`: `resume()` returns `Err`.
  - `frame_ack_waits_for_the_frames_progressive_job`: no `FrameAcknowledge` until the job
    completes.
  - `controls_reach_the_backend_when_applied`. StartFrame, DeleteEncodingContext and EndFrame
    are forwarded, and `ProgressiveDecoder` is unused. ResetGraphics is never forwarded.
- [ ] **Step 2: Run.** `cargo test -p ironrdp-egfx --lib progressive_`. Expected: compile
  failure (missing types).
- [ ] **Step 3: Implement** the Task 4 half: no submit-ahead yet, so a W2S2 submits when it
  reaches handling. Then:
  - `is_awaiting_decoder` covers progressive jobs;
  - the backlog cap counts only `Pdu`/`Forwarded` entries.
- [ ] **Step 4: Run.** `cargo test -p ironrdp-egfx`. Expected: all pass, the existing 80 included.
- [ ] **Step 5: Commit.** `feat(egfx): asynchronous RemoteFX Progressive backend, applied in stream order`.

### Task 5: Progressive submit-ahead, with controls forwarded in stream order

**Files:** `crates/ironrdp-egfx/src/client.rs` and its tests.

- [ ] **Step 1: Write the failing tests.**
  - `progressive_commands_submit_ahead_up_to_the_limit`: with limit 2 and three queued
    behind a pending H.264 frame, two are submitted.
  - `controls_reach_the_backend_in_stream_order_across_submit_ahead`. The backend's call log is
    exactly:

    ```
    start, submit 1, delete_context, submit 2, end, start, submit 3, end
    ```

    That is for: frame 1 with W2S2, DeleteEncodingContext, W2S2, EndFrame; then frame 2 with
    W2S2, EndFrame. All queued behind a pending job, then everything completes. No call is
    repeated after the jobs apply.
  - `create_and_delete_surface_stop_progressive_submit_ahead`: a W2S2 queued after a
    CreateSurface or DeleteSurface is not submitted until that applies, and then with the new
    size.
  - `reset_graphics_stops_progressive_submit_ahead`.
- [ ] **Step 2: Run.** `cargo test -p ironrdp-egfx --lib progressive_`. Expected: the new tests fail
  (submission happens late).
- [ ] **Step 3: Implement.** `submit_ahead` also walks for progressive:
  - submit W2S2 while under `max_in_flight`;
  - forward StartFrame, EndFrame and DeleteEncodingContext, replacing them with `Forwarded`;
  - stop at ResetGraphics, CapabilitiesConfirm, CreateSurface and DeleteSurface, and at a W2S2
    over the limit;
  - `bench.rs`'s drain needs nothing more, since `is_awaiting_decoder` covers it.
- [ ] **Step 4: Run.** `cargo test -p ironrdp-egfx`. Expected: all pass. Then
  `cargo clippy -p ironrdp-egfx --tests`. Expected: clean.
- [ ] **Step 5: Commit.** `feat(egfx): submit RemoteFX Progressive commands ahead, controls in stream order`.

## Stages 2–4 (outlined)

- **Stage 2:** one Worker, GPU only. Covers:
  - the `ironrdp-rfx-worker` cdylib and `rfx_worker.js`;
  - Blob URL and `rfxWorkerUrl`;
  - external tiles and `SurfaceBackend::write_tiles`;
  - the stage 0 upload path, and pooled buffers;
  - the wasm-side per-tile byte comparison.

  Proves:
  - bench message p95 under 50 ms and none over 150 ms;
  - tile bytes equal to in-thread;
  - the user's live 3 minutes with no audio dropouts.
- **Stage 3:** `ProgressiveDecoder::sharded`, with its structural pass, `FrameTile::Elsewhere`
  and union tests for count 1–4, then N = 4. Proves:
  - 3× throughput, byte-identical;
  - the live sharp mode as smooth as H.264.
- **Stage 4:** only if numbers require it: a pixel-unpack buffer, then an atlas.
