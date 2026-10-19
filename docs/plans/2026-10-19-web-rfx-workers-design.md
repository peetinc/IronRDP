# RemoteFX Progressive decoding in Web Workers

Phase 3 of `docs/plans/2026-10-16-web-gpu-rendering-design.md` (section C), revised against
what phases 0–2 and the audio work since have shown, and after two adversarial reviews (§
"Review outcome").

## Problem (measured)

In Windows' default mixed mode the web client is smooth while Windows sends H.264, then jerky
while it sends the sharp codecs. Audio now plays within 300 ms of the picture
(`2026-10-19-web-audio-playout-design.md`, second revision), so every main-thread stall over
about 300 ms is also an audible gap. With "Full-screen H.264" (`avcThinClient`) both problems
go away, at the cost of a soft picture. The user's verdict: works well with full-screen H.264,
drops out consistently without.

Native decode time per codec over the four recordings in `static/bench/`, replayed through
zgfx and the real decoders (release build, Apple M-series; a throwaway tool, not in the repo):

| Recording | RemoteFX Progressive | ClearCodec | zgfx | Heavy frames, RemoteFX p95 | Worst frame | Tiles/frame p95 |
|---|---|---|---|---|---|---|
| rfx-heavy | 92% | 1% | 7% | 48 ms | 98 ms | 1068 |
| mixed-17-31 | 95% | 1% | 4% | 54 ms | 94 ms | 1313 |
| live-local-4k | 93% | 1% | 6% | 46 ms | 93 ms | 1169 |
| live-lbdev | 97% | 0% | 3% | 12 ms | 575 ms | 245 |

A tile costs about 0.04 ms native, and wasm is slower: the browser bench measured frame p95
71 ms and worst 240 ms on the mixed capture. A full-screen RemoteFX frame (2,040 tiles at 4K)
is about 200 ms of main thread in wasm, plus uploading the tiles: today 9 GL calls per tile,
about 18,000 per heavy frame (`gpu/mod.rs` `upload_into`). That upload cost is not yet
measured.

So the cost is one codec, and that codec decodes in independent 64×64 tiles.

## Goals

- RemoteFX Progressive tile decoding off the main thread, across up to 4 Web Workers.
- Decoded tile pixels never enter the main thread's wasm memory: they go from a Worker's buffer
  straight to the GPU.
- No `SharedArrayBuffer`: no cross-origin isolation required of embedding pages.
- Pixel-identical output to today's decoder.
- Native clients unchanged: in-thread decoding stays the default in `ironrdp-egfx`.

**Non-goals:**
- ClearCodec, Planar and zgfx in Workers (5–8% together).
- SIMD kernels (phase 4, separate).
- Moving the session loop off the main thread.
- Workers for the 2D-canvas renderer: Workers run only with GPU rendering, and other sessions
  decode in-thread as today.

## Acceptance

Per stage (§ Stages). The end state:
1. **Bench, rfx-heavy and live-local-4k, GPU rendering:**
   - RemoteFX decode throughput at least 3× today's in-thread rate (N = 4);
   - main-thread time per task (inside `process` + `resume`) p95 under 50 ms, none over 150 ms;
   - every tile byte-identical to in-thread decoding.
2. **Live, default mixed mode, ac-v4 over Tailscale, Full-screen H.264 unticked:**
   - 3 minutes of the same video with no audible dropouts (stage 2);
   - the sharp mode no jerkier to the eye than the H.264 mode (stage 3).

## Design

### 1. Shard the decoder, not the stream

`ProgressiveDecoder` (`ironrdp-graphics/src/progressive.rs`) keeps all its stream-level logic.
It gains a shard: `(index, count)`, default `(0, 1)`.

```rust
impl ProgressiveDecoder {
    /// A decoder that owns only the tiles of shard `index` of `count`.
    pub fn sharded(index: u8, count: u8) -> Self;
}
```

A tile `(x_idx, y_idx)` belongs to shard `(x_idx + y_idx) % count`. The diagonal spreads any
rectangle of updates evenly over the shards. `surface_id` is deliberately not in the rule.

A sharded decoder:
- **Parses and validates everything.** It parses every block and runs every structural check on
  every tile, its own or not, before the ownership filter: `TileOutOfBounds`,
  `InvalidQuantIndex`, `MissingBlock`, `SurfaceTooLarge`. A malformed stream therefore fails
  identically in every shard.
- **Keeps all stream-level state.** Contexts, frame and REGION bookkeeping.
- **Marks every frame tile.** `frame_tiles` holds a marker for every tile the frame updated: its
  own tiles as `Context`/`Pixels`, other shards' as a new `FrameTile::Elsewhere`. REGION
  clipping-work charging (`charge_region_clipping_work`) and `delete_context`'s retention
  therefore count exactly what the unsharded decoder counts.
- **Decodes only its own tiles.** It decodes, stores and references only those, and returns
  `DecodedTile`s only for those. Tile state, sub-band references and `Pixels` snapshots are
  keyed by tile position, so all of a position's state, across every codec context, is in one
  shard.

**INVARIANT:** for any input the unsharded decoder accepts, the union of the `count` shards'
outputs equals the unsharded output, tile for tile and byte for byte. For any input it rejects
with a structural error, every shard rejects it with the same error.

**Data errors:** RLGR, SRL and a difference tile with no reference are visible only to the
owning shard, and the other shards have already decoded past that tile. Shards have then
diverged from what in-thread decoding would hold. Such an error is therefore fatal to the
session, as every progressive error is today (`handle_wire_to_surface2` returns `Err` up through
`ActiveStage`). Both paths keep that rule.

### 2. Workers

Each Worker holds one `ProgressiveDecoder::sharded(i, N)`.

**Count:** `N = clamp(navigator.hardwareConcurrency − 2, 1, 4)`. Stage 2 fixes `N = 1`.

**Inputs:** every Worker receives every progressive input in stream order:
- `WireToSurface2` (job id, surface id and size, context id, bitmap bytes);
- StartFrame and EndFrame;
- DeleteEncodingContext;
- DeleteSurface.

ResetGraphics is not forwarded: progressive state deliberately survives it
(`client.rs`, the ResetGraphics handling), and resetting it would make every later payload fail
`MissingBlock`. The bitmap bytes are copied once per Worker; progressive data is about
10–100 KB per command.

**Replies:** each Worker answers each job with one transferred `ArrayBuffer`.
- It holds that Worker's placements packed tightly: each update rectangle's pixels, already
  cropped from its 64×64 tile, back to back, so the crop in `client.rs` moves into the Worker.
- An `Int32Array` alongside gives each placement's destination rectangle and byte offset.
- Workers answer each job in order (a Worker's queue is FIFO). The main thread matches replies
  by job id, and a job completes when all `N` have answered.
- **Buffer pool:** after the upload the main thread transfers each buffer back to its Worker for
  reuse, so a heavy frame (about 33 MB) does not become garbage-collector pressure on the main
  thread. In flight, at most `max_in_flight` buffers per Worker.

**Worker ABI:**
- `ironrdp-rfx-worker`, a separate cdylib over `ironrdp-graphics`, with plain exports (alloc,
  submit, control, take result, free), no wasm-bindgen and no web-sys.
- Built with the same release flags as the main module (`xtask/src/web.rs`: `opt-level=3`,
  `+simd128`, `+bulk-memory`).
- Driven by a hand-written classic Worker script, `rfx_worker.js`.
- The main thread compiles the module once and posts the `WebAssembly.Module` to each Worker.

**Packaging:** the script and the module ship in `iron-remote-desktop-rdp`'s dist, beside
`playout_worklet.js`.
- **Worker URL:** a Blob URL by default.
- **`rfxWorkerUrl(url)` extension:** for pages whose CSP forbids `blob:` workers.
  - A same-origin Worker takes its CSP from its own response headers, so that response must
    allow `'wasm-unsafe-eval'`. Documented on the extension, as `audioWorkletUrl` is.
  - lithiumbridge's CSP has `worker-src 'self' blob:` and `script-src ... 'wasm-unsafe-eval'`,
    and a Blob Worker inherits the page's policy. So the default works there.

### 3. In the EGFX pipeline

`GraphicsPipelineClient` gets an asynchronous progressive path, shaped like the H.264 one
(`H264Decoder`, `max_in_flight`, the pending queue with submit-ahead):

```rust
pub trait ProgressiveBackend: Send {
    fn max_in_flight(&self) -> usize;
    fn submit(&mut self, job: u32, surface_id: u16, codec_context_id: u32, width: u16, height: u16, data: &[u8]);
    fn control(&mut self, control: ProgressiveControl); // StartFrame, EndFrame, DeleteContext, DeleteSurface
    fn poll(&mut self) -> Option<(u32, Result<DecodedTiles, ProgressiveDecodeError>)>;
}

pub struct DecodedTiles {
    /// Pixels held outside this crate (a Worker's buffer), drawn by the surface backend.
    pub external: Option<u64>,
    /// In-thread pixels, when `external` is `None`.
    pub tiles: Vec<DecodedTile>,
}
```

**Without a backend:** decoding stays synchronous through the existing `ProgressiveDecoder`.
This is the native default and the fallback.

**With one:**
- **Deferred commands:** a `WireToSurface2` becomes `Pending::Progressive(job)`. Later PDUs wait
  behind it in the pending queue, exactly as behind a deferred AVC420 frame. When the entry
  reaches the front its tiles are drawn, so drawing order is unchanged.
- **Submit-ahead:** later `WireToSurface2` commands, up to `max_in_flight`, are submitted while
  earlier ones decode.
  - **INVARIANT:** the Workers see the progressive inputs in stream order. Controls passed over
    by submit-ahead are forwarded to the backend at that moment, before the next submit. When
    their pending entries later apply, they no longer touch progressive state.
  - **Barriers:** CreateSurface and DeleteSurface stop submit-ahead, as ResetGraphics and
    CapabilitiesConfirm already do. A progressive command needs its surface's size at
    submission, and a resize queued behind a pending job would otherwise submit with the old
    size.
- **Frame acknowledgements are unchanged.** A frame is acknowledged when its EndFrame applies.
- **A failed job is a session error,** as in-thread (§1).
- **One wake per completed job:** `ironrdp-web` wakes the session loop once per completed job,
  not once per Worker reply, the way `GraphicsDecoded` works for H.264.

**Surface backend:** `SurfaceBackend::write_tiles(surface_id, &DecodedTiles)`, with a default body
that writes `tiles` through `write_rgba` and warns on `external`. This follows
`DecodedFrame::external` and `Compositor::write_frame`. The GL backend looks the handle up in
`ironrdp-web`'s thread-local store, owner-capped like `gpu/frames.rs`. It then uploads each
placement with one `texSubImage2D(…, srcData, srcOffset)` from the Worker's buffer. Per surface
that is one bind, and no `pixelStorei`: placements are packed tight. `on_bitmap_updated` is not
called for external tiles, which the web handler doesn't use.

The upload path is decided by stage 0's measurements. Per-placement `texSubImage2D(srcOffset)`
is the default. If per-call cost still exceeds the budget, a pixel-unpack buffer comes next;
an atlas texture with an instanced draw is the last resort, since the GL backend has no shaders
today.

`resume()` stays as it is. If stage 2's bench shows that draining several completed jobs in one
task exceeds 50 ms, it may stop after one progressive job. That is safe: completions are
monotonic, and every completion wakes the loop.

### 4. Failure

- **A Worker or the module fails to start:** that session decodes in-thread. One WARN.
- **A Worker errors mid-session:** its tile state is lost, so later upgrade passes and
  difference tiles would decode against nothing. The session ends with "RemoteFX worker
  failed", and the existing reconnect logic runs. That is the same rule as WebGL context loss.
- **A progressive decode error:** fatal, as today (§1).

### 5. Memory

- **Tile state:** about 37 KB per tile per context. Sharding divides it across Workers instead of
  duplicating it.
- **Main thread:** its wasm memory holds no tile pixels.
- **Pooled reply buffers:** at most `max_in_flight` per Worker.

## Stages

| Stage | What | Proves |
|---|---|---|
| 0 | **Measure.** Time `GlBackend` uploads per frame on rfx-heavy in the bench. A standalone WebGL2 page compares 2,040 placements on a 4K texture: today's 9-call pattern, `texSubImage2D(srcOffset)`, a pixel-unpack buffer, and an atlas with an instanced draw. The bench gains main-thread time per task (inside `process` + `resume`) and a drain that waits for progressive jobs. | Which upload path stage 2 builds, and the baseline numbers |
| 1 | **`ProgressiveBackend` and pending integration** in `ironrdp-egfx`, with a fake backend: late and out-of-order completion, control order across submit-ahead, Create/DeleteSurface barriers, acks, errors. No web code. | Ordering, by native tests |
| 2 | **One Worker** (N = 1), GPU only: the worker module and script, external tiles, the stage 0 upload path, pooled buffers. | Main-thread p95 under 50 ms and none over 150 ms on the bench; tile bytes equal to in-thread; live 3 minutes with no audio dropouts |
| 3 | **Sharding to N = 4:** `sharded`, structural pass, `FrameTile::Elsewhere`, union tests. | 3× throughput; byte-identical; live sharp mode as smooth as H.264 |
| 4 | Only if stage 2/3 numbers require it: pixel-unpack buffer, then atlas. | — |

Stage 2 is a checkpoint, not a release. It removes the stalls behind the audio dropouts, but at
N = 1 decode throughput equals today's, and Windows paces itself to the acknowledgements.

## Testing

- **Native, `ironrdp-graphics`:** the union of shards equals the unsharded output, byte for
  byte, for `count` in {1, 2, 3, 4}, on:
  - the existing progressive golden and encoder round-trip fixtures;
  - a mid-frame context deletion;
  - a cross-context REGION;
  - an input that trips the REGION clipping-work limit.

  A structural error fails every shard identically. Test-first.
- **Native, `ironrdp-egfx`:** stage 1's list, with the fake backend.
- **Bench (browser):** main-thread time per task p95 and max, and throughput, against today's
  in-thread run on the same machine. Plus a per-tile byte comparison of Worker output against
  in-thread decode of the same capture, inside wasm. The bench's GPU sampling (1/64 of pixels)
  is not the pixel-identity proof; the native union tests and this comparison are.
- **Live:** acceptance 2, by the user.

## Risks

- **GL upload cost** on non-Mac GPUs. Stage 0 measures it on the user's Mac and on a Windows
  machine.
- **Per-command round trip:** about 1–2 ms per `WireToSurface2`. Frames with many small
  commands depend on submit-ahead. The bench shows whether `max_in_flight` needs to exceed
  H.264's 4.
- **Duplicate parsing:** every Worker parses every command's block structure and REGION
  rectangles. That is linear in the command size and small next to RLGR and the inverse DWT;
  the bench confirms it.
- **Load balance:** a command touching one tile goes to one Worker.
- **A second wasm module:** another target in `cargo xtask web build` and a file in the dist.

## Review outcome

Two Fable adversaries, 2026-10-19. The first attacked the spec, the second attacked the first's
counter-proposal.

**Taken from the first review:**
- The invariant now covers errors.
- `FrameTile::Elsewhere`.
- ResetGraphics is not forwarded.
- Errors stay fatal, as they are today (the first draft wrongly said they were dropped).
- Create/DeleteSurface are submit-ahead barriers.
- The main-thread upload cost is budgeted, and tile pixels stay out of the main thread's wasm.
- Wakes are coalesced, jobs matched by id, and the pixel-identity proof made explicit.

**From the first reviewer's counter-proposal:**
- **Kept:** external tiles cropped in the Worker, and N = 1 as the first stage.
- **Dropped:** its atlas with instanced draw (per the second review) and time-slicing
  `resume()`.

**Taken from the second review:**
- Per-placement `texSubImage2D(srcOffset)` with no new shaders.
- Workers only with GPU rendering, so there is one result path.
- Pooled reply buffers.
- Stage 0 measurement first.
- N = 1 as a checkpoint, not a release.
- A bench metric for main-thread time.

**Rejected by both:** GPU-side RLGR/DWT (RLGR is serial bit decoding), WebGPU, and SIMD first
(it keeps the stall on the main thread).

## Stage 0 results (2026-10-19)

Measured on Chrome 154, macOS, Apple M1 Max (ANGLE Metal). A Windows run of the micro-bench is
still to come.

**Main thread today** (replay bench, `h264=1&gpu=1`, in-thread decode). Message times are the
main-thread time per capture message, the bench's stand-in for one task:

| Capture | p50 | p95 | Max | Over 50 ms | Over 150 ms | GL upload (share of run) | Upload max per message |
|---|---|---|---|---|---|---|---|
| rfx-heavy | 1.6 ms | 64.1 ms | 314.1 ms | 866 | 4 | 2.8 s of 113.7 s (2.5%) | 15.5 ms |
| live-local-4k | 0.2 ms | 25.0 ms | 234.8 ms | 202 | 6 | 1.0 s of 34.8 s (2.9%) | 19.1 ms |
| mixed-17-31 | 0.5 ms | 79.4 ms | 240.1 ms | 322 | 7 | 1.4 s of 40.1 s (3.4%) | 14.7 ms |

All three miss the acceptance (p95 under 50 ms, none over 150 ms). Uploads are at most about
19 ms of any message, so decoding is the stall, as the codec breakdown said.

**Upload micro-bench** (`tools/webgl-upload-bench.html`). 2,040 tiles into a 4K texture, p50
of 30 runs, two runs, pixels verified with `?check=1`:

| Variant | Main thread | Until the GPU is done | GL calls |
|---|---|---|---|
| A: today's per-tile pattern | 13.9 / 14.8 ms | 42.1 / 44.9 ms | 18,360 |
| B: tight `srcOffset` | 13.8 / 14.4 ms | 41.7 / 43.0 ms | 2,041 |
| C: pixel-unpack buffer | **2.2 / 2.2 ms** | **14.7 / 14.0 ms** | 2,044 |
| D: texture array + instanced draw | 12.5 / 12.5 ms | 32.1 / 32.2 ms | 16 |

The call count barely matters (A ≈ B). What costs is handing client memory to each call, and
one buffer upload avoids it.

**Decision: stage 2 uploads through a pixel-unpack buffer (C).**
- **Mechanics:** each Worker reply is written into a `PIXEL_UNPACK_BUFFER` with one
  `bufferData`, then one `texSubImage2D(…, offset)` per placement.
- **Why not B:** the rule's first choice, B, fits the 15 ms budget only at p50, and is no
  cheaper than today. C issues about 6× faster and finishes about 3× sooner.
- **What changes in the design:** §3's "per-placement `texSubImage2D(srcOffset)`" becomes the
  pixel-unpack buffer. No shaders are needed.
