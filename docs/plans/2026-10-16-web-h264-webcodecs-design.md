# Web client H.264 (AVC420) through WebCodecs — design

## Problem

On a 4K desktop playing video, the web client's main thread is ~80–90% busy and the page
drops to ~17–20 fps while the link carries only 11–15 Mbit/s (measured 2026-10-16 against
AC-WIN-micro, local relay, no tunnel). Nearly all of that time is software decoding in wasm:
`putImageData` took 78 ms of 15 s; the codec work took ~13.3 s. Windows sends video as
RemoteFX Progressive and ClearCodec because the web client advertises no AVC capability.
Audio stutters because it shares the main thread. Windows App on the same network is smooth:
it negotiates H.264 and decodes it in hardware.

## Goal

Let the web client negotiate AVC420 and decode it with the browser's WebCodecs
`VideoDecoder` (hardware where available), so Windows can encode the desktop as H.264.

Out of scope: AVC444/AVC444v2 (keep filtered out), moving decoding to a Worker, SIMD for
the CPU codecs, lithiumbridge UI (a follow-up once the client proves out).

## Constraint: WebCodecs is asynchronous

`H264Decoder::decode` is synchronous and the EGFX client applies each command in order:
later commands in the same frame (`SolidFill`, `SurfaceToSurface`, `SurfaceToCache`,
`CacheToSurface`, other codecs) read and write the same surface. A browser decoder returns
frames through a callback, so an AVC420 command cannot complete inside `process()`.
Applying the H.264 frame late, out of order, would corrupt the surface, which is exactly
the class of stale-artifact bug fixed earlier today.

## Design

### ironrdp-egfx: deferrable H.264 decode

`H264Decoder` gains two provided methods; existing decoders (OpenH264) are unaffected:

```rust
pub enum H264Decode { Frame(DecodedFrame), Pending }

fn submit(&mut self, data: &[u8], regions: &[ExclusiveRectangle]) -> DecoderResult<H264Decode> {
    let _ = regions;
    self.decode(data).map(H264Decode::Frame)
}
fn poll_frame(&mut self) -> Option<DecoderResult<DecodedFrame>> { None }
```

`regions` are the AVC420 metablock's `regionRects` (frame coordinates equal surface
coordinates); a decoder may fill only those pixels, which lets the web decoder read back
just the changed areas instead of the full 4K frame.

`GraphicsPipelineClient`:

- `decode_avc420` calls `submit`. `Frame` is applied as today. `Pending` stores the surface,
  destination rectangle and region rects as the deferred command.
- While a command is deferred, `process()` queues every later PDU, in order, in a backlog
  instead of handling it. ZGFX decompression still runs on arrival (its state is sequential).
- New `resume() -> PduResult<Vec<DvcMessage>>`: asks the decoder for the frame with
  `poll_frame`. `None` keeps waiting. `Some(Ok)` applies the frame exactly as a synchronous
  decode would have; `Some(Err)` logs and drops that one frame. Either way the backlog then
  replays in order until it is empty or another decode defers. It returns the replies
  (for example `FrameAcknowledge`) the replayed PDUs produced.
- New `is_awaiting_decoder()`.
- The backlog is capped (hostile-input surface). Overflowing it is a decode error. In
  practice the server's frame-acknowledgement window bounds it, since `EndFrame` is
  acknowledged only after it replays.
- `close()` drops the deferred command and the backlog.

Frame acknowledgement then naturally paces the server to the decoder's speed.

### ironrdp-session

`ActiveStage::resume_graphics_pipeline(&mut self, image) -> SessionResult<Vec<ActiveStageOutput>>`
calls `resume()`, encodes the replies on the EGFX channel as a `ResponseFrame`, and drains
the compositor into `image` as a `GraphicsUpdate`, the same as after a network read.

### ironrdp-web

New module `h264` with a `WebCodecsH264Decoder: H264Decoder`.

- `H264Decoder: Send`, and JS objects are not, so the decoder is a handle. Its JS state
  (decoder, closures, readback canvas, finished frame) lives in a thread-local slot keyed
  by handle id, which also allows several sessions per page.
- Configuration is lazy, from the first SPS: the codec string is `avc1.PPCCLL` (profile,
  constraint flags, level), with `optimizeForLatency: true` and Annex B chunks (no
  `description`). Length-prefixed (AVC format) input is converted to Annex B.
- A chunk is `key` when it holds an IDR slice. Before the first key chunk, or after a
  decoder error until the next key chunk, `submit` fails and that frame is dropped.
- Output: the `VideoFrame` is drawn to an `OffscreenCanvas` and each region rect is read
  back with `getImageData` into an RGBA frame-sized `DecodedFrame`. The frame is stored as
  the completed result, and the session loop is woken with
  `RdpInputEvent::GraphicsDecoded`, which calls `resume_graphics_pipeline`.
- `reset()` (on `ResetGraphics`) closes the decoder. The next SPS reconfigures it.
- Support probe: `VideoDecoder.isConfigSupported` for High profile level 5.2. Unsupported
  means no decoder, and the existing filter drops the AVC capability sets.
- Enabled by a new `h264` extension, off by default (`h264(true)` in
  `iron-remote-desktop-rdp`). The Svelte test client gets a checkbox.

### Capabilities

With a decoder present, the web handler keeps advertising `V8_1 { AVC420_ENABLED | SMALL_CACHE }`
plus `V8`, so Windows can choose AVC420. AVC444 stays unadvertised.

## Risks

- **Decoder holding frames.** If a browser decoder buffers a frame until it sees the next
  one (reordering), the pipeline would stall waiting on it. Windows' RDP encoder doesn't
  reorder frames, and Windows App (VideoToolbox) decodes it with no lookahead. A watchdog
  logs when a decode takes over 500 ms, so a stall is visible. `flush()` isn't a fallback,
  because WebCodecs requires a key frame after it.
- **Readback cost.** A full-screen 4K change reads back about 33 MB. Reading back only the
  changed regions keeps typical frames small; full-screen video is the worst case and still
  far cheaper than software decoding.
- **Missing key frames.** After a decoder error, nothing is drawn until Windows sends an IDR frame.

## Testing

- ironrdp-egfx unit tests with a fake decoder that defers:
  - Later PDUs wait, and `EndFrame` isn't acknowledged until the frame resumes.
  - The frame is applied before PDUs that came after it.
  - A failed decode skips only that frame.
  - Overflowing the backlog is an error.
  - Existing synchronous behaviour is unchanged.
- ironrdp-web host tests for the pure helpers: NAL splitting, SPS to codec string,
  key-frame detection, length-prefix to Annex B.
- Live: the Svelte test client at 4K on AC-WIN-micro with H.264 on, measured with the same
  main-thread, fps and inbound probe used for the baseline.
