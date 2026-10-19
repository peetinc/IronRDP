# EGFX: pipelined H.264 decoding and Windows App-style frame acknowledgement

**Status:** approved for build (2026-10-19)
**Branch:** `feat/web-h264`

## Why

On the same host (AC-WIN-micro, default RDP policies, mixed mode) Windows sends Windows App about
3x the data our web client gets, at full quality:

| Video playing | Windows App | Our web client |
|---|---|---|
| Session size | 2880x1664 | 3840x2160 |
| Server sent rate (RemoteFX Network counter) | 18.6 Mbit/s avg, 48.9 peak | 5.9-7.5 Mbit/s avg |
| H.264 QP the server chose | 25 | 41 |

Both negotiate EGFX 10.7 and the same initial profile. A FreeRDP proxy capture of Windows App's
graphics channel shows the difference is how frames are acknowledged:

- Windows App acknowledges the first ~50 frames, then sends `FrameAcknowledge` with
  `queueDepth = SUSPEND_FRAME_ACKNOWLEDGEMENT` (0xFFFFFFFF). From then on it sends only
  `QoeFrameAcknowledge` for every frame (4,330 QoE acks against 73 frame acks). When its queue
  builds it sends a few `FrameAcknowledge` with a real queue depth (9,158 and 40,258 bytes), then
  suspends again.
- Our client acknowledges every frame, and only once the frame has been applied. An H.264 frame is
  applied only after WebCodecs returns it, and every command behind it waits. So the server
  sees a slow client and lowers its rate.

Suspending acknowledgements alone (tested live) makes Windows send at full rate, and our client
then falls behind: 1,471 PDUs queued behind the H.264 decoder within ten seconds, then the tab died.
The queue builds entirely behind the H.264 decoder, because the client decodes one H.264
frame at a time: `deferred_avc420` holds one frame and everything after it waits, including the
next H.264 frame.

## Goals

1. Several H.264 frames decode at once. Commands still apply in wire order.
2. The web client acknowledges frames the way Windows App does: QoE every frame, acknowledgements
   suspended while it keeps up, resumed with a queue depth when it falls behind.
3. Clients that do not opt in behave exactly as today.

Non-goals: AVC444 chroma (still decoded as reference only), RemoteFX in workers, UDP transport.

## Design

### 1. Ordered pending queue in `GraphicsPipelineClient` (ironrdp-egfx)

`deferred_avc420: Option<DeferredAvc420>` plus `backlog: VecDeque<GfxPdu>` become one queue,
`pending: VecDeque<Pending>`:

```rust
enum Pending {
    /// An H.264 picture submitted to the decoder, applied when the decoder returns it.
    Avc420(DeferredAvc420),
    /// A command held because something ahead of it is still decoding.
    Pdu(GfxPdu),
}
```

Rules:

- **Dispatch.** An empty queue handles the PDU at once, as today. A deferred decode goes to the
  queue. A non-empty queue appends the PDU as `Pending::Pdu`, then calls `submit_ahead`.
- **`submit_ahead`.** Walks the queue from the front. Each `Pdu` that is an H.264 `WireToSurface1`
  (AVC420, AVC444, AVC444v2) has its bitstream submitted now and is replaced in place by its
  `Avc420` placeholder. An AVC444 chroma-only update leaves no placeholder. The walk stops at:
  - a decode barrier, a PDU that resets the decoder when applied: `ResetGraphics` and
    `CapabilitiesConfirm`;
  - the in-flight limit, `H264Decoder::max_in_flight()`, counted as the queued `Avc420`
    placeholders.

  Submitting in queue order keeps decode order equal to wire order. The decoder sees exactly the
  same sequence of access units as today, only earlier.
- **Resume.** Pops from the front. An `Avc420` applies when `poll_frame()` yields; otherwise
  resume stops. A `Pdu` is handled; if handling defers a new decode, its placeholder goes to the
  front. Then `submit_ahead` runs again.
- **Bounds.** `MAX_DEFERRED_BACKLOG` (4096) bounds the whole queue, as it bounds `backlog` today.
- **Synchronous decoders** (`max_in_flight() == 1`, for example OpenH264) never defer, so nothing
  changes for them.

Apply-time checks stay where they are. Surface existence, bounds warnings and region clipping
run when the placeholder applies, so a `CreateSurface` queued ahead still takes effect first.

### 2. `H264Decoder` trait (ironrdp-egfx)

- New: `fn max_in_flight(&self) -> usize { 1 }`. This is how many `Pending` submissions the
  client may have outstanding.
- `poll_frame` is re-documented to yield results in submission order, one per `Pending`
  submission. A submission that failed yields its error in its own place.

Existing implementors keep compiling and keep their behaviour.

### 3. WebCodecs decoder (ironrdp-web `h264.rs`)

- `Shared.finished: Option<…>` and the single `regions` become an ordered list of entries:
  `{ regions, submitted_at, result: Option<DecoderResult<DecodedFrame>> }`. The list is a small
  pure struct, so it can be tested natively.
- **Submit** pushes an entry. A synchronous decode error pushes an entry that already holds
  its `Err`.
- **Output callback.** Shown pictures fill the oldest waiting entry. CPU readback uses that
  entry's regions.
- **Decoder failure** (the error callback, or a closed decoder found at submit) fails every
  waiting entry.
- **`poll_frame`** pops the front entry once it has a result.
- **Limit:** `max_in_flight()` returns `4`. Hardware decoders have few output buffers, and the
  client holds at most that many frames before applying them.

### 4. GPU frame store (ironrdp-web `gpu/frames.rs`)

`insert` stops closing an owner's earlier frames. With several frames in flight they are live.
Instead it closes the owner's oldest frames beyond `MAX_HELD_PER_OWNER = 8`, which still catches
leaks. `release(owner)` on decoder drop is unchanged.

### 5. Acknowledgement policy (ironrdp-egfx)

```rust
pub enum FrameAckPolicy {
    /// Acknowledge every frame once applied (MS-RDPEGFX 3.3.5.12). Default.
    EveryFrame,
    /// What Windows App does: QoE every frame, suspend acknowledgements while caught up.
    SuspendWhenCaughtUp,
}
```

Set with `GraphicsPipelineClient::with_frame_ack_policy(policy, clock)`. `clock: fn() -> u64` is
in milliseconds, injected because `ironrdp-egfx` must not read a wall clock (wasm has none in
`std`).

`SuspendWhenCaughtUp`:

- **Startup:** acknowledge every frame (queue depth `Unavailable`) until `SUSPEND_AFTER = 50`
  frames are applied.
- **Suspend:** after that, at an `EndFrame` with an empty queue, send the acknowledgement with
  `QueueDepth::Suspend`, then send no more `FrameAcknowledge`.
- **Resume:** while suspended, if `RESUME_AFTER_FRAMES = 8` whole frames (queued
  `EndFrame`s) are queued at dispatch, or `RESUME_AT = 512` entries as a backstop, send a
  `FrameAcknowledge` immediately. It carries the last applied frame id and
  `QueueDepth::AvailableBytes(queued bytes)`. Acknowledgements then resume, and the client
  suspends again after another 50 applied frames with an empty queue.
- **No repeated acknowledgement.** The server rejects a second acknowledgement of a frame: it
  logs `RDP_GFX … PipeEventOnError` and disconnects with "graphics subsystem in an error
  state". So the immediate resume acknowledgement goes out only when the last applied frame has
  not been acknowledged yet. Otherwise the next frame's acknowledgement carries the queue depth.
- **Dispatch-time resume.** The resume check runs when PDUs arrive, not when they apply. The live
  test showed that checking at apply time is too late.
- **QoE:** each applied `EndFrame` also sends `QoeFrameAcknowledge` (MS-RDPEGFX 2.2.2.21), when
  the negotiated version is 10 or later:
  - `timestamp` = clock at `StartFrame` receipt;
  - `timeDiffSE` = `EndFrame` receipt minus `StartFrame` receipt;
  - `timeDiffEDR` = applied minus `EndFrame` receipt.

  Receipt times are taken at dispatch and kept per frame id (bounded at 256).
- **Re-arm:** `ResetGraphics` resets the startup counter. Suspension persists, as on the wire.

`EveryFrame` is the default and is byte-for-byte today's behaviour.

### 6. Web client wiring (ironrdp-web)

`session.rs` builds the EGFX client with `SuspendWhenCaughtUp` and a `Date.now` clock. A new
extension, `frame_ack_suspend` (default `true`), with factory `frameAckSuspend(enable)` in
`iron-remote-desktop-rdp`, is the kill switch. With it off, the client uses `EveryFrame`. The
bench page uses the same policy as a session.

## Error handling

- A failed H.264 decode drops that frame only, in its place, as today.
- A decoder that fails outright fails every waiting entry. The client drops those frames and
  carries on. WebCodecs reconfigures on the next key frame, as today.
- Queue overflow (4096) is a session error, as today. With resume at 32, overflow only happens if
  the server ignores acknowledgements.

## Testing

ironrdp-egfx unit tests, with a scripted async decoder that records submissions and is given
results:

- `two_h264_frames_decode_at_once_and_apply_in_order`
- `commands_between_h264_frames_apply_between_them`
- `submit_ahead_stops_at_reset_graphics`
- `submit_ahead_respects_the_decoders_in_flight_limit`
- `a_failed_frame_ahead_of_a_good_one_drops_only_itself`
- `sync_decoder_behaviour_is_unchanged`, which the existing deferred tests also cover
- `every_frame_policy_acks_each_frame_without_qoe`
- `suspend_policy_suspends_after_50_caught_up_frames`
- `suspended_client_resumes_with_a_queue_depth_when_the_queue_builds`
- `suspend_policy_resuspends_after_catching_up`
- `qoe_ack_carries_receipt_times_from_the_clock`

ironrdp-web native tests:

- the decode entry list fills in order, a failure keeps its place, and failing all fails only
  the waiting entries;
- the frame store keeps several frames per owner and closes past the cap.

Bench page: `mixed-17-31.bin` and `live-local-4k.bin` still replay without errors at no less than
today's frame rate. `verify=1` still matches the CPU path.

## Acceptance (live, AC-WIN-micro, default policies, same video as Windows App)

At 3840x2160, local rig, for at least 3 minutes:

- the server's sent rate averages ≥ 12 Mbit/s, twice the 5.9 Mbit/s baseline;
- frame quality ≥ 99;
- no tab crash, and no queue overflow;
- frames skipped for insufficient client resources average < 1/s.

## Review focus

- **Order.** Commands must apply in wire order even with frames in flight. Risk: a placeholder
  inserted at the wrong position on replay.
- **Barriers.** No H.264 access unit may be submitted ahead of a queued `ResetGraphics`. Otherwise
  the decoder reset would discard it, or decode it against the old sequence.
- **Leaks.** `VideoFrame`s are closed on every path: applied, dropped, decoder failure, session
  end, beyond the cap.
- **Flow control.** With acknowledgements suspended, the server must not be able to make the
  client buffer without bound. Resume must come from dispatch, not apply.
- **Default unchanged.** Native clients that never call `with_frame_ack_policy` send the same
  bytes as before.
