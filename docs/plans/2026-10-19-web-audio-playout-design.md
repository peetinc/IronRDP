# Web RDPSND playout: an adaptive jitter buffer on the audio thread

**Status:** approved for build after adversarial review (2026-10-19)
**Branch:** `feat/web-h264`
**Builds on:** `docs/plans/2026-10-15-web-audio-playback-design.md`

## Problem (measured)

The web client's sound chops over a WAN. Windows App, on the same Tailscale path to the same
host, plays clean audio. So the transport is not the cause; our client is.

A DEBUG log from the web client on that path (ac-v4 → Tailscale → AC-WIN-micro, AAC at
44.1 kHz stereo, 1024-frame blocks of 23.2 ms) shows:

- **Windows produces audio evenly:** `audio_timestamp` advances about 23 ms per block, every
  block.
- **It reaches the client's RDPSND handler in bursts.** For example:
  - nothing for 300-650 ms (48.306 → 48.952);
  - then about 50 blocks (1.16 s) in 9 ms (48.952-48.961);
  - the same at 36.25, 54.24, 56.93 and 00.89.
- **Our player turns each burst into a gap:**
  1. `player.rs` starts playing 50 ms after a block arrives (`LEAD_SECS`).
  2. It caps the queue at 0.5 s (`MAX_QUEUED_SECS`).
  3. Once over the cap, it drops every arriving block until the queue is below 0.1 s
     (`LOW_WATERMARK_SECS`).
  4. A 650 ms arrival gap empties the queue, and playback falls silent.
  5. The 1.16 s burst that follows fills the 0.5 s cap, and the rest, about 0.6-0.9 s, is
     dropped ("Dropping RDPSND block: playback queue is full", dozens per burst).

  Roughly half of every burst is thrown away.

The log does not say why arrival is bursty. It records when the main thread processed each
PDU, not when its bytes reached the browser. The cause is either TCP loss recovery on the WAN
or the main thread being busy with graphics. Windows App sees the same TCP stream and copes,
so a client must tolerate this jitter whatever its cause. This design makes the client
tolerate it, and measures which cause it is, so the follow-up (moving graphics decode off the
main thread) is driven by data.

## Goals

1. Continuous playback through arrival gaps of up to about 2 s. No audible gaps from arrival
   jitter alone, only from outages longer than the buffer.
2. Never discard audio in normal operation. Latency may rise to cover the jitter actually seen
   ("clean, even if latent").
3. Playback timing independent of the main thread. A busy main thread delays *delivery*, not
   *playback*.
4. Clean audio: one continuous resampler, so no ticks at block boundaries and no pitch change.
5. Diagnostics that separate network from main-thread causes.

Non-goals:
- protocol changes (WaveConfirm stays immediate);
- moving RDPSND parsing or AAC decode off the main thread;
- UDP;
- reducing latency again once raised (silence trimming is a later, separate addition);
- A/V sync (RDP has none).

## Review outcome

The first draft had an adaptive target with a ±0.5 % rate-skew controller, decay and floor.
The adversarial review showed it is dead weight on the measured pattern:
- the target pins at its ceiling after three outages;
- the skew saturates and cannot drain a 1 s burst in under a minute;
- re-buffering to the full target after an underrun adds silence to every outage.

The review also found that the client ignores `Wave2.audio_timestamp`, the server's own clock,
which is the exact end-to-end delay signal. And that `enqueue` times and `longtask` entries
cannot separate network from main-thread causes.

The design below is the review's minimal version, plus a diagnostics heartbeat.

## Design

### 1. Playout runs in an AudioWorklet

`player.rs` keeps its public shape (`Player::new`, `enqueue`, autoplay resume) and its
"context not running → drop the block" rule. Each block no longer becomes an
`AudioBufferSourceNode`:

- **Setup.** `Player::new` creates the `AudioContext`. It loads `audio/playout_worklet.js`,
  embedded with `include_str!`, through a Blob URL, then creates an `AudioWorkletNode`
  `rdpsnd-playout` with 2 output channels, connected to the destination. Loading is
  asynchronous: blocks enqueued before the node exists wait in a main-thread `VecDeque`,
  bounded at 2 s of audio (oldest dropped), and are flushed when the node is ready.
- **Sending blocks.** `enqueue` posts `{ rate, channels: [Float32Array, …] }` to the node's port
  and transfers the buffers. Volume is already applied. Mono goes as one channel and the
  worklet duplicates it.
- **No fallback path.** AudioWorklet needs the same secure context WebCodecs already needs,
  and every current browser has it. If the module fails to load, the player logs WARN once and
  plays nothing. The old scheduled-buffer code and its 0.5 s cap-and-drop are removed.

### 2. The worklet

`playout_worklet.js` is one ES module:
- a pure `Playout` class (ring, state machine, reader), exported for vitest;
- `PlayoutProcessor extends AudioWorkletProcessor`, registered only when `registerProcessor`
  exists.

`onmessage` copies each block into the ring. `process()` never allocates.

**Ring.** Per channel, Float32, at the stream rate. Capacity is 4 s. A block at a different
rate drains the ring first (format change), then switches rate.

**States and target:**

| Name | Value |
|---|---|
| `TARGET_START` | 400 ms |
| `TARGET_CEIL` | 2000 ms |
| `GAP_MARGIN` | 100 ms |
| `PREROLL_CAP` | 300 ms |

- `Buffering`: output silence. Leave when depth ≥ `min(target, PREROLL_CAP)` after an underrun,
  or ≥ `target` at start. Fade in over 5 ms, then go to `Playing`.
- `Playing`: output from the reader. When fewer than 2 ms remain, fade out to zero, count an
  underrun, and go to `Buffering`. While Buffering, count the stream-time span of silence
  output (the gap). On leaving, set `target = min(TARGET_CEIL, max(target, gap + GAP_MARGIN))`.
  The target never decays.
- **Overflow:** only when the ring is full (4 s). Discard the oldest samples needed to fit the
  block, with a 5 ms fade, and count the dropped ms. Not expected in normal operation.

**Revision after live testing (2026-10-19).** The first build left audio 4 s behind the picture:
playback stalled while blocks kept arriving (no underrun, 10 s dropped, the ring pinned full),
and nothing brought depth back to the target. And raising the target from the local arrival
gap turned a paused video, where Windows stops sending, into a 2 s target. So:

- **Trim:** bursts only refill what a gap drained, so steady play holds about `target`. When
  depth exceeds `target + 150 ms`, the reader fades out over 5 ms, skips to the newest
  `target` of audio and fades back in. A start-up backlog is skipped the same way, silently.
  Skipped audio is counted as `trimmedMs`, apart from overflow drops.
- **Lateness:** each block carries `lateMs`, its delay above the best seen this session (the
  `DelayWindow` of §3). Any block raises `target` to `lateMs + GAP_MARGIN`, capped at
  `TARGET_CEIL`, before the ring runs dry. The first block after a stall is late; the first
  after a pause is not. Blocks with no known delay (queued while the worklet loaded) and
  values over 10 s (a wrapped 16-bit timestamp) fall back to the outage rule above.

**Reader.** The read position advances by a fixed `streamRate / contextRate` per output frame,
with 4-point cubic (Catmull-Rom) interpolation. That is one continuous fractional reader, so
there are no per-block resampling ticks. There is no rate skew.

**Stats.** Every second the processor posts:
- `depthMs`: mean, min, max;
- `targetMs`;
- `underruns`;
- `gapMsMax`;
- `droppedMs`.

### 3. Diagnostics on the main thread

- **Pipeline delay.** `wave()` passes `audio_timestamp`, the server's ms clock, to the player.
  It keeps `offset = min over the session of (local_ms − audio_timestamp)` and logs per window
  the min, current and max of `(local_ms − audio_timestamp) − offset`. That is end-to-end
  delay above the best seen, which includes network and main-thread backlog.
- **Main-thread heartbeat.** A 20 ms `setInterval` records its largest lateness per window.
  The timer runs on the same event loop as the session. A heartbeat late by about the audio
  gap means the main thread is the cause. A heartbeat on time during a gap means the network
  is.
- **Log line.** Every 10 s, INFO `Audio playout stats`, with:
  - the worklet's last stats;
  - delay min, current and max;
  - `heartbeatLateMsMax`.

  The 1 s worklet stats are also logged at DEBUG.

### 4. What is unchanged

- the RDPSND protocol, formats, quality mode and immediate WaveConfirm;
- AAC decode through WebCodecs, and PCM/ADPCM in `codec.rs`;
- volume handling;
- autoplay resume on gesture.

`aac.rs`'s silent `try_borrow_mut` drop becomes a `warn!`, so it can never be invisible loss.

## Error handling

- **Worklet module load fails:** WARN once and play nothing.
- **`processorerror` event:** WARN and go silent.
- **Malformed message** (no channels, rate 0): ignored and counted.

## Testing

**vitest** (`web-client/iron-remote-desktop-rdp`, importing the module by path) against the
pure `Playout`:

- `replayed_burst_pattern_from_the_log_plays_without_gaps_after_one_adaptation`. The arrival
  times come from the pasted log. The test checks:
  - after the first adaptation, no underrun;
  - no dropped ms.
- `sine_has_no_discontinuity_across_blocks_and_rate_conversion`: a 1 kHz sine in 1024-frame
  blocks at 44.1 kHz read out at 48 kHz has no sample-to-sample delta beyond the sine's own
  maximum plus a small epsilon.
- `underrun_raises_target_to_gap_plus_margin_capped_at_ceiling`.
- `full_ring_drops_oldest_with_fade_and_counts_it`.

**Rust unit tests** (native): the pre-ready queue bound, and the delay-window statistics.

**Live:** the ac-v4 → Tailscale run with the same video, then lb-dev over QUIC.
Accept when:
- there are no audible gaps for 3 minutes;
- underruns stop after the first adaptations;
- `droppedMs` is 0;
- the stats line shows whether the delay swing tracks heartbeat lateness.

## Open risks

- **Latency only ratchets up**, to at most 2 s. A single long stall raises latency for the
  rest of the session. Trimming bounds depth to the target, not the target itself.
- **Clock drift between the server and the AudioContext** (tens of ppm) slowly grows or
  shrinks depth, about 0.2 s per hour. An underrun or the 4 s ring bounds it. Measured
  per session through the delay stats.
