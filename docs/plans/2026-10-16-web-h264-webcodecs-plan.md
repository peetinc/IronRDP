# Web client H.264 through WebCodecs — plan

Spec: [2026-10-16-web-h264-webcodecs-design.md](2026-10-16-web-h264-webcodecs-design.md)

Each task: write the failing test, watch it fail, implement, watch it pass, then commit.

## Task 1: ironrdp-egfx: deferrable decode

Files: `crates/ironrdp-egfx/src/decode.rs`, `crates/ironrdp-egfx/src/client.rs` (tests inline).

- `decode.rs`: add `H264Decode`, plus the provided methods `submit` and `poll_frame`.
- `client.rs`:
  - Add the fields `deferred_avc420: Option<DeferredAvc420>` and `backlog: VecDeque<GfxPdu>`.
  - Add `MAX_DEFERRED_BACKLOG` (4096 PDUs).
  - Split `decode_avc420` into the parse/submit part and `apply_avc420(surface_id, dest_rect, regions, frame)`.
  - `process()`: queue PDUs while a decode is deferred.
  - Add `resume()` and `is_awaiting_decoder()`; `close()` clears the deferred state.
- Tests (fake `DeferredDecoder` that returns `Pending`, then a frame from `poll_frame`):
  1. A deferred decode withholds the `EndFrame` acknowledgement and paints nothing until `resume`.
  2. `resume` with no frame ready is a no-op.
  3. `resume` paints the AVC regions before a later `SolidFill` in the same frame (ordering).
  4. A failed poll drops only that frame; the backlog still replays and acknowledges.
  5. Overflowing the backlog is an error.
- Checks: `cargo test -p ironrdp-egfx`.

## Task 2: ironrdp-session: resume entry point

File: `crates/ironrdp-session/src/active_stage.rs`.

- Add `resume_graphics_pipeline(&mut self, image)`. It encodes the replies with
  `ironrdp_dvc::encode_dvc_messages` and `DvcMessageBatch`, drains the compositor, and
  applies `widen_to_full_refresh`.
- Checks: `cargo test -p ironrdp-session`, clippy.

## Task 3: ironrdp-web: WebCodecs decoder

Files: `crates/ironrdp-web/src/h264.rs` (new), `crates/ironrdp-web/src/lib.rs`, `crates/ironrdp-web/src/session.rs`.

- Pure helpers, host-tested: `nal_units`, `to_annex_b`, `codec_string`, `is_key`.
- Hand-written `VideoDecoder` / `EncodedVideoChunk` / `VideoFrame` bindings, in the style of `audio/aac.rs`.
- Thread-local decoder slots; a `WebCodecsH264Decoder` handle; `probe_support()`.
- Session:
  - an `h264` builder extension;
  - probe before building the channels;
  - pass the decoder to `GraphicsPipelineClient`;
  - `RdpInputEvent::GraphicsDecoded` calls `resume_graphics_pipeline`.
- Checks: `cargo test -p ironrdp-web --lib` (the 19 clipboard tests are the known baseline),
  then `cargo xtask wasm check -v`.

## Task 4: TypeScript surface, test client, dist

- `web-client/iron-remote-desktop-rdp/src/main.ts`: add `h264(enable)`.
- Svelte test client: an "H.264 (WebCodecs)" checkbox, preset by `VITE_RIG_H264`.
- `cargo xtask web build -v`, then commit the dist and `DIST_SOURCE_COMMIT`.

## Task 5: live test

- Connect the Svelte client to AC-WIN-micro at 4K with H.264 on, in Claude in Chrome.
- Check the console for `EGFX capabilities confirmed` with V8_1 and for AVC420 frames.
- Play the Costa Rica 4K video and measure main-thread busy %, fps, worst gap and inbound
  Mbit/s against the baseline (87.8% busy, 17 fps, 234 ms gap, 14.9 Mbit/s).
