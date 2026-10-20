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

## Stage 2: one Worker (detailed after stage 1)

**Shared names:**
- **Crate:** `crates/ironrdp-rfx-worker`, `crate-type = ["cdylib", "rlib"]`, a workspace member.
  Its pure API:
  - `pub fn pack_tiles(surface_width: u16, surface_height: u16, tiles: &[DecodedTile], pixels: &mut Vec<u8>, placements: &mut Vec<i32>)`.
    It produces placements of 5 `i32`s each, `[left, top, width, height, byte_offset]`, with
    pixels tight (row length = width). These are exactly the rectangles and pixels
    `GraphicsPipelineClient::write_progressive_tiles` writes, in the same order.
  - `pub struct WorkerDecoder`, with:
    - `submit(&mut self, surface_id, codec_context_id, width, height, data) -> Result<(), u32>`
      (it decodes and packs into its own buffers);
    - `control(&mut self, kind: u32, surface_id: u16, codec_context_id: u32)`;
    - `pixels() -> &[u8]` and `placements() -> &[i32]`.
  - `pub fn error_reason(code: u32) -> &'static str`.
- **Control kinds:** `0` StartFrame, `1` EndFrame, `2` DeleteContext, `3` DeleteSurface.
- **Error codes:** one per `ProgressiveDecodeError` variant, in declaration order, starting at
  1. The reasons are the client's existing "rfx progressive decode failed: …" strings.
- **wasm exports** (`#[cfg(target_arch = "wasm32")]`, `extern "C"`, one global `WorkerDecoder`):
  - `rfx_alloc(len) -> *mut u8`;
  - `rfx_free(ptr, len)`;
  - `rfx_submit(surface, ctx, width, height, ptr, len) -> u32` (0 = ok);
  - `rfx_control(kind, surface, ctx)`;
  - `rfx_pixels_ptr()`, `rfx_pixels_len()`, `rfx_placements_ptr()`, `rfx_placements_len()`
    (lengths in bytes).
- **Worker messages:**
  - main → worker:
    - `{type:'init', module}`;
    - `{type:'submit', job, surface, ctx, width, height, data}` (data transferred);
    - `{type:'control', kind, surface, ctx}`;
    - `{type:'return', buffer}` (a pixel buffer handed back for reuse);
  - worker → main: `{type:'result', job, error, pixels, placements}`, with `pixels`
    (ArrayBuffer, from the pool or new) and `placements` (Int32Array) transferred.
- **Web side (`crates/ironrdp-web/src/rfx_worker.rs`):** `WorkerProgressiveBackend::new(wake) -> Option<Self>`,
  a handle to thread-local state like `h264.rs`'s, implementing `ProgressiveBackend`. Its
  result handles index a thread-local store, which `GlBackend::write_external_tiles` takes
  from.
- **Extension:** `rfx_workers` (TS `rfxWorkers(enable)`), off by default, used only with
  GPU rendering. Test client: a checkbox, plus `VITE_RIG_RFX_WORKERS`.

### Task 6: `ironrdp-rfx-worker` crate: packing, decoder, ABI

- [ ] **Step 1: Write the failing tests** (native, in the crate):
  - `pack_tiles_reproduces_the_clients_tile_writes`. Decode the win2022 first pass
    (`ironrdp-graphics/tests/data`) for a 1280×720 surface. Pack it. Draw the placements onto a
    blank RGBA surface, and draw the tiles with a reference copy of the client's crop rule onto
    another. The two are identical, and so is the placement count.
  - `pack_tiles_clips_tiles_at_the_surface_edge`: one tile at x_idx 1 on a 100×100 surface
    gives a 36×64 placement whose rows are the tile's first 36 columns.
  - `worker_decoder_matches_progressive_decoder`: first pass then upgrade pass through
    `WorkerDecoder`, against `ProgressiveDecoder` plus `pack_tiles`. Byte-identical pixels and
    placements.
  - `worker_decoder_reports_decode_errors_by_code`: garbage input gives `Err(code)`, with
    `error_reason(code)` starting with "rfx progressive decode failed".
- [ ] **Step 2: Run.** `cargo test -p ironrdp-rfx-worker`. Expected: compile failure.
- [ ] **Step 3: Implement.** The crop rule moves from `client.rs`'s `write_progressive_tiles`. The
  client keeps its own copy, and the first test pins the two to each other.
- [ ] **Step 4: Run.** Tests pass. Then
  `cargo build -p ironrdp-rfx-worker --target wasm32-unknown-unknown --release` (with the
  CC/AR env). Expected: builds. Its `.wasm` imports nothing, checked with Node:
  `WebAssembly.Module.imports(new WebAssembly.Module(bytes)).length === 0`.
- [ ] **Step 5: Commit.** `feat(rfx-worker): RemoteFX Progressive decoding as a standalone wasm module`.
  `cargo xtask check locks -v` must pass: the new crate enters `Cargo.lock`, intentionally.

### Task 7: build and embed the worker module

- `xtask/src/web.rs` `build`:
  - first runs `cargo build -p ironrdp-rfx-worker --target wasm32-unknown-unknown --release`
    with the same `RUSTFLAGS` as the main module (dev: without `--release`);
  - then sets `IRONRDP_RFX_WORKER_WASM` to its output path for `wasm-pack`.
- `crates/ironrdp-web/build.rs`:
  - copies that file to `OUT_DIR/rfx_worker.wasm`, or writes an empty file when the variable
    is unset (native tests, plain `cargo check`);
  - declares `rerun-if-env-changed` and `rerun-if-changed`.
- `rfx_worker.rs` holds `include_bytes!(concat!(env!("OUT_DIR"), "/rfx_worker.wasm"))`. An
  empty module means Workers are unavailable, so decoding runs in-thread.
- [ ] Verify: `cargo xtask web build -v` succeeds. `cargo test -p ironrdp-web --lib` still builds
  without the variable. Commit: `build(web): embed the RemoteFX worker module`.

### Task 8: the Worker backend, PBO upload, extension

**Files:**
- Create: `crates/ironrdp-web/src/rfx_worker.rs` and `crates/ironrdp-web/src/rfx_worker.js`
  (embedded with `include_str!`).
- Modify: `gpu/mod.rs` (`write_external_tiles`, with the pixel-unpack buffer created lazily);
  `session.rs` (extension, wiring); `lib.rs`; `main.ts` (`rfxWorkers`); the test client's
  `login.svelte`.

**Behaviour:**
- `new` compiles the embedded module once per page (`WebAssembly.compile`, cached), starts one
  classic Worker from a Blob URL of the script, and posts `init`. `None` if the module is
  empty or `Worker` is unavailable.
- `submit` copies the data into a fresh `Uint8Array` and transfers it. `control` posts.
- On `result`: an error code becomes `Err(ProgressiveJobError { reason: error_reason(code) })`.
  Otherwise the buffers go into the store under a new handle, and the result is
  `DecodedTiles { external: Some(handle), tiles: vec![] }`. Then `wake()`, once per job.
- Worker `onerror`: every outstanding and later job yields
  `Err(ProgressiveJobError { reason: "RemoteFX worker failed" })`.
- `write_external_tiles(surface, handle)`: take the entry from the store, bind the surface
  texture and the unpack buffer, `bufferData` the pixels, then one `texSubImage2D` per
  placement at its byte offset, clipped to the texture. Unbind, then post `return` with the
  buffer. A missing handle warns.
- Drop: terminate the Worker, revoke the URL, clear the store.

**Steps:**
- [ ] **Step 1:** Native unit tests for the pure parts:
  - `placements_from_i32s_reads_five_per_placement`;
  - `results_complete_in_order_and_wake_once_per_job`, against the thread-local store with no
    Worker.
- [ ] **Step 2:** Implement. `cargo test -p ironrdp-web --lib rfx_worker` passes; wasm clippy is
  clean for the new and changed files.
- [ ] **Step 3: Commit.** `feat(web): decode RemoteFX Progressive in a Web Worker (rfxWorkers)`.

### Task 9: bench and acceptance

- [ ] **Bench:**
  - `EgfxBench.create` gains `workers: bool`. The page reads `workers=1`.
  - `verify=workers`: the backend also decodes each job in-thread, with a `WorkerDecoder` on
    the main thread, and compares the Worker's pixels and placements with it byte for byte.
    The run reports `workerJobs` and `workerMismatches`.
- [ ] **Run,** on this Mac, `h264=1&gpu=1&workers=1` for rfx-heavy, live-local-4k and mixed-17-31.
  Expected:
  - message p95 under 50 ms and none over 150 ms on rfx-heavy and live-local-4k;
  - `verify=workers` on all three: `workerMismatches` 0.

  Record everything in the ledger and append "Stage 2 results" to the spec.
- [ ] **Build the dist, commit, push `feat/web-h264`.** Then ask the user for the live test:
  ac-v4 over Tailscale, Full-screen H.264 unticked, "RemoteFX workers" ticked, 3 minutes of
  the video, no audio dropouts.

## Stage 3: four Workers (detailed after stage 2)

Stage 2 passed live on 2026-10-19 (spec, "Stage 2 live results"): the main thread is healthy
and the one Worker is pegged at 100% in mixed mode. Stage 3 shards the decoder (spec §1) so
up to 4 Workers share that work.

**Stage 3 constraints** (added to Global Constraints):
- Ownership: tile `(x_idx, y_idx)` belongs to shard `(x_idx + y_idx) % count`. `surface_id`
  is not in the rule. `ProgressiveDecoder::new()` is `sharded(0, 1)`: native clients and the
  in-thread path are unchanged.
- Worker count: `N = clamp(navigator.hardwareConcurrency − 2, 1, 4)`, 1 when unknown. The bench
  overrides it with `workers=N`.
- Each job goes to all N Workers; it completes when all N have answered. One wake per completed
  job, not per reply.
- `max_in_flight` stays 8 jobs.
- No `SharedArrayBuffer`. Workers only with GPU rendering, as in stage 2.

**Shared names:**
- `ironrdp-graphics`: `pub fn ProgressiveDecoder::sharded(index: u8, count: u8) -> Self`
  (panics if `count == 0` or `count <= index`); `FrameTile::Elsewhere` (private).
- `ironrdp-rfx-worker`: `pub fn WorkerDecoder::sharded(index: u8, count: u8) -> Self`; wasm
  export `rfx_shard(index: u32, count: u32)`, which replaces the global decoder.
- Worker messages: `init` gains `shard` and `shards`; the script calls
  `rfx_shard(shard, shards)` after instantiating, before any queued message. Nothing else
  changes.
- `ironrdp-web` (`rfx_worker.rs`):
  - `WorkerProgressiveBackend::new(count: usize, wake) -> Option<Self>`;
  - `pub(crate) struct JobResults<T>`, shared by the N slots:
    `new(shards)`, `complete(job, shard, outcome: Result<T, &'static str>) -> bool` (true when
    that job now has all N), `take(job) -> Option<Result<Vec<T>, (&'static str, Vec<T>)>>`
    (the parts in shard order; on error, the first error by shard order plus the successful
    parts, for their buffers to go back), `fail()`;
  - `take_tiles(handle) -> Option<Vec<ExternalTiles>>`; `with_tiles` likewise over the parts;
  - `pub(crate) fn worker_count(hardware_concurrency: Option<f64>) -> usize`.

**Stage 3 review focus:**
1. A Worker that dies, or never starts, must not leave the session waiting for its replies:
   the job fails and the session ends (Task 12, `a_lost_worker_fails_outstanding_jobs`).
2. One shard's error must not leak the other shards' buffers (Task 12,
   `a_failed_shard_fails_the_job_and_hands_back_the_rest`).
3. A surface deleted and recreated with the same id must not inherit first-pass bits (Task 10,
   `shards_forget_first_passes_when_a_surface_is_deleted`).
4. A tile emitted twice in one job (two REGIONs) counts twice when comparing: the comparisons
   are multisets, not sets (Tasks 10–12).
5. `count = 1` must behave exactly as stage 2 (Task 10's count-1 runs; Task 13's `workers=1`).

### Task 10: `ProgressiveDecoder::sharded`

**Files:** Modify `crates/ironrdp-graphics/src/progressive.rs` (`ProgressiveDecoder`,
`ProgressiveContext`, `FrameTile`, `decode_tile_block`, the REGION loop in `decode_bitmap`);
tests in its `mod tests`; `crates/ironrdp-graphics/tests/ms_progressive.rs`.

**Behaviour** (spec §1, plus what the code showed):
- `decode_tile_block` takes `owned: bool`. For a tile it does not own it runs the same checks,
  in the same order, that the owner runs before decoding, and decodes nothing:
  - Simple, First: bounds (`tile_index`), then the quant indices, then (First)
    `progressive_quant_for(quality)`. The difference-reference check is the owner's alone: the
    references live with it (spec: a data error).
  - Upgrade: bounds; then, if the tile has had no first pass in this context, skip it as the
    owner does (`tile_state.pass == 0`), marking nothing; otherwise
    `progressive_quant_for(quality)`.
  - It allocates no `TileState`: the memory split is the point.
- **First-pass bits:** the owner knows `pass == 0` from its tile state; the others cannot. So
  `SurfaceTiles` gains a bit per tile slot, beside `tiles`, set when a Simple or First tile is
  accepted (owned or not). Living in `SurfaceTiles`, it is cleared wherever the tile state is
  (`new`, resize, `reset`, `delete_context`, `delete_surface`) with no extra code. Without it,
  the other shards would mark the skipped tile, charge REGION clipping work the unsharded
  decoder does not, and run `progressive_quant_for` where the owner returns first.
- Accepted non-owned tiles go into `frame_tiles` as `FrameTile::Elsewhere`. The REGION loop
  charges clipping work for them exactly as for the others and emits nothing for them.
  `delete_context` keeps them as they are.
- Owned tiles come out in the same relative order as unsharded.

- [ ] **Step 1: Write the failing tests.** A test helper
  `fn assert_shards_match(calls: &[Call])` runs one unsharded decoder and, for each count in
  1..=4, `count` sharded decoders through the same calls (`decode_bitmap`, `begin_frame`,
  `end_frame`, `delete_context`, `delete_surface`). It stops at the first error, as the
  session does: after a data error the shards diverge by design. A `#[cfg(test)]
  fn frame_tile_keys(&self, surface_id: u16) -> Vec<(u16, u16)>` must be equal across the
  unsharded decoder and every shard after every call: it is the direct check of `Elsewhere`
  marking and `delete_context` retention, and of the per-frame-tile clipping charge (exactly
  one per key per REGION). Errors compare as `format!("{error:?}")`
  (`ProgressiveDecodeError` has no `PartialEq`). After each `decode_bitmap` it asserts:
  - if unsharded returned `Ok(tiles)`: every shard returned `Ok`, and the union of the shards'
    tiles, sorted by `(y_idx, x_idx, update_rectangles, pixels)`, equals `tiles` sorted the
    same way, and every tile comes from its owner;
  - if unsharded returned a structural error (`TileOutOfBounds`, `InvalidQuantIndex`,
    `MissingBlock`, `SurfaceTooLarge`, or `Pdu` for the clipping limit): every shard returned
    that same error.

  Cases, each a `#[test]`:
  - `shards_match_on_the_windows_capture`: the win2022 first pass, then the upgrade pass, on a
    1280×720 surface (`tests/data`; in `tests/ms_progressive.rs`);
  - `shards_match_on_encoder_round_trips`: the `first_tile_stream` / `upgrade_tile_stream` /
    `simple_tile_stream` builders over a 3×3 tile grid on a 150×150 surface (partial edge
    tiles), first pass then two upgrades;
  - `shards_match_across_contexts_in_one_frame`: the
    `region_repaints_frame_tiles_from_another_codec_context` sequence, including its mid-frame
    `delete_context`;
  - `shards_match_on_the_region_clipping_limit`: the `decoder_bounds_region_clipping_work`
    input: every shard fails with the same error. It exhausts the budget in the rectangle loop
    only; the per-frame-tile charge is covered by the `frame_tile_keys` equality;
  - `shards_match_on_structural_errors`: a tile out of bounds; a quant index past the table;
    no CONTEXT ever seen;
  - `shards_skip_an_upgrade_without_a_first_pass`: an upgrade of a tile that never had a first
    pass, inside a REGION that clips it: no shard emits or marks it;
  - `shards_forget_first_passes_when_a_surface_resizes`: first pass at 128×128, then an upgrade
    at 192×192 with no new first pass: every shard skips it, as unsharded does.
  - `shards_forget_first_passes_when_a_surface_is_deleted`: first pass, `delete_surface`, then
    an upgrade on the same surface id: every shard skips it.

  In these three the skipped upgrade has `quality` 3 and an empty `quantProgVals`: unsharded
  returns `Ok([])` before looking at it, and a shard with a missing or stale bit fails
  `InvalidQuantIndex`. Without that, a wrong mark would still return `Ok([])`.
- [ ] **Step 2: Run.** `cargo test -p ironrdp-graphics shards_`. Expected: compile failure
  (`sharded` missing).
- [ ] **Step 3: Implement** as above. `sharded(0, 1)` must take the same code path as today.
- [ ] **Step 4: Run.** `cargo test -p ironrdp-graphics` all pass, including every existing
  test. Then `cargo test -p ironrdp-egfx --lib` and `cargo test -p ironrdp-rfx-worker`.
- [ ] **Step 5: Commit.** `feat(graphics): ProgressiveDecoder::sharded, tiles split across decoders by position`.

### Task 11: shard the Worker module

**Files:** `crates/ironrdp-rfx-worker/src/lib.rs` and its `tests`; `crates/ironrdp-web/src/rfx_worker.js`.

- [ ] **Step 1: Failing test** `sharded_workers_pack_the_unsharded_placements`: the win2022 first
  then upgrade pass through `WorkerDecoder::new()` and through `WorkerDecoder::sharded(i, 3)`
  for i in 0..3. The shards' placements together, each with its pixel bytes, sorted by
  `(top, left, width, height)`, equal the unsharded ones sorted the same way.
- [ ] **Step 2: Run.** `cargo test -p ironrdp-rfx-worker`. Expected: compile failure.
- [ ] **Step 3: Implement** `WorkerDecoder::sharded` and `rfx_shard`. `rfx_shard` validates
  (`count` 1..=255, `index < count`) and otherwise keeps `(0, 1)`: a panic in the Worker is a
  trap, and a trap ends the session. In `rfx_worker.js`, `init` calls
  `wasm.rfx_shard(message.shard ?? 0, message.shards ?? 1)` before draining `queued`, and
  `takeBuffer(0)` returns `new ArrayBuffer(0)` rather than a pooled buffer.
- [ ] **Step 4: Run.** Tests pass; the wasm build still imports nothing (stage 2's Node check).
- [ ] **Step 5: Commit.** `feat(rfx-worker): one shard of the progressive decoder per Worker`.

### Task 12: N Workers behind one backend

**Files:** `crates/ironrdp-web/src/rfx_worker.rs`, `gpu/mod.rs` (`write_external_tiles`),
`session.rs` (count), `bench.rs` (`workers` becomes a count).

**Behaviour:**
- `new(count, wake)` starts `count` slots from the one compiled module; slot `i` gets
  `init {module, shard: i, shards: count}`. If any slot fails to start synchronously
  (`Worker::new`, `postMessage`), terminate the ones that did and return `None` (in-thread
  decoding, one WARN). A module that fails to instantiate inside a Worker reports later, as
  `onerror`, and ends the session ("RemoteFX worker failed"), as in stage 2.
- `submit` posts to every slot, each with its own copy of `data` (transferred). `control`
  posts to every slot.
- A slot's `result` becomes an `ExternalTiles` (or its error reason) and goes into the shared
  `JobResults` with the slot's shard index. `wake()` only when `complete` returns true.
- A slot's `onerror` calls `JobResults::fail()` and wakes: every outstanding and later job then
  fails "RemoteFX worker failed", as in stage 2.
- `poll(job)`: `take(job)`. On `Ok(parts)` store the parts under one handle:
  `DecodedTiles { external: Some(handle), tiles: vec![] }`. On `Err((reason, parts))` give
  every part's buffer back to its Worker, then return the error.
- `write_external_tiles`: upload every part with placements (`upload_packed`) and skip empty
  ones; return every part's buffer. The shards' placements never overlap, so their order does
  not matter.
- `JobResults` ignores a `complete` for a job already taken, or after `fail()`, and hands its
  buffer back: a slow Worker's late reply must not leave an entry nobody polls.
- Drop terminates every slot, revokes every URL, and clears the store for every owner.
- Each Worker's `result` carries `ms`, its `rfx_submit` wall time, for the bench.
- Session: `count = worker_count(navigator.hardwareConcurrency)`. Bench: `workers=N` (0 or
  absent = in-thread), and `verify=workers` compares the union of the parts with the
  in-thread `WorkerDecoder::new()` as a multiset of `(rect, pixel bytes)`.

- [ ] **Step 1: Failing native tests** in `rfx_worker.rs`:
  - `a_job_completes_when_every_shard_has_answered`: 3 shards; `complete` returns false, false,
    true; `take` gives the parts in shard order even when they arrived out of order.
  - `a_failed_shard_fails_the_job_and_hands_back_the_rest`: one shard `Err`; `take` gives that
    reason and the other two parts.
  - `a_lost_worker_fails_outstanding_jobs`: after `fail()`, `take` on a job with 2 of 3 parts is
    `Err(("RemoteFX worker failed", the 2 parts))`, and on an unanswered later job the same with
    no parts. A job complete before `fail()` still gives `Ok`.
  - `a_late_reply_after_take_is_dropped`: `complete` for a taken job returns false and keeps
    no entry; the caller gets the outcome back to return its buffer.
  - `worker_count_leaves_two_cores_and_caps_at_four`: `None` → 1, `Some(1.0)` → 1, `Some(4.0)` →
    2, `Some(10.0)` → 4.
- [ ] **Step 2: Run.** `cargo test -p ironrdp-web --lib rfx_worker`. Expected: compile failure.
- [ ] **Step 3: Implement.** Replace stage 2's per-slot `Results` with `JobResults<ExternalTiles>`.
- [ ] **Step 4: Run.** Tests pass; `cargo xtask web build -v` succeeds; wasm clippy clean on the
  changed files.
- [ ] **Step 5: Commit.** `feat(web): RemoteFX decoding across up to four Workers`.

### Task 13: bench, acceptance, live

- [ ] **Bench** (this Mac, fresh tab, vite without `VITE_RIG_DUMP_EGFX`), `h264=1&gpu=1`, on
  rfx-heavy, live-local-4k, mixed-17-31 and live-workers-fail, with `workers=1` and `workers=4`.
  Expected:
  - `verify=workers` with `workers=4`: `workerMismatches` 0 on all four;
  - message p95 under 50 ms and none over 150 ms with `workers=4`;
  - decode throughput: jobs ÷ the busiest Worker's summed `ms` (the bench reports
    `workerBusyMs` per shard and `jobsPerSecond`). With `workers=4` at least 3× `workers=1`'s
    on rfx-heavy and live-local-4k passes; 2–3× is a ruling with the numbers; under 2× fails
    the stage, and stage 3 stops for a rethink. Replay `ms` is reported too, as secondary: it
    includes H.264 and main-thread work.

  Record in the ledger and append "Stage 3 results" to the spec.
- [ ] **Build the dist, commit, push `feat/web-h264`.** Then the live test, by the user: windowed
  video in mixed mode, Full-screen H.264 unticked, RemoteFX workers ticked, 3 minutes; the
  Worker threads are sampled for CPU, and the sharp regions should be no jerkier to the eye than
  H.264.

## Stage 4 (outlined)

Only if numbers require it: a pixel-unpack buffer per Worker part, then an atlas.
