# Web client audio playback (RDPSND) — design

Status: phase 1 (PCM) implemented; phases 2–4 planned.

## Goal

Play remote session audio in the browser client. Phase 1 ships uncompressed
PCM behind an opt-in extension. The structure must let compressed codecs drop in
later without reworking the playback path, because PCM is bandwidth-heavy
(48 kHz/16-bit/stereo ≈ 1.5 Mbit/s over the WebSocket).

## Non-goals

- Microphone capture (MS-RDPEAI). Separate project.
- The audio-playback DVC (`AUDIO_PLAYBACK_DVC`) / UDP transports. Static
  `rdpsnd` channel only, which `Rdpsnd` already speaks.
- Pitch (`PitchPdu`). Not advertised, ignored — same as the native backend.
- A/V sync with the graphics pipeline.

## Current state

- `ironrdp-rdpsnd::client::Rdpsnd` implements the client side of MS-RDPEA:
  format negotiation (client list filtered to formats the server offered),
  training, WaveInfo/Wave and Wave2, volume, close. It confirms each wave block
  immediately after handing it to the handler.
- `ironrdp-web` attaches `Rdpsnd::new(Box::new(NoopRdpsndBackend))`
  unconditionally because Windows only drives RDPDR when RDPSND is present
  (MS-RDPEFS Appendix A<1>). `build_config` sets `enable_audio_playback: false`,
  so `INFO_NOAUDIOPLAYBACK` is set and the server plays audio locally.
- `ironrdp-rdpsnd-native` (cpal) is the reference backend. It does not build for
  `wasm32-unknown-unknown`; it is a template, not a dependency.

## Codec landscape (why the seam matters)

`Rdpsnd::client_formats` only keeps client formats the *server* also offered.
So what is worth implementing depends on what Windows offers, not what browsers
decode:

| Format | Tag | Ratio vs PCM | Browser decode path | Windows server offers? |
|---|---|---|---|---|
| PCM 16-bit | `0x0001` | 1× | trivial, in Rust | yes |
| A-law / µ-law | `0x0006`/`0x0007` | 2× | trivial, in Rust | yes (8 kHz-class) |
| MS ADPCM / IMA ADPCM | `0x0002`/`0x0011` | ~4× | small pure-Rust decoder | yes |
| AAC (MS) | `0xA106` | ~10–15× | WebCodecs `AudioDecoder` (`mp4a.40.2`) | yes on recent Windows |
| Opus | `0x704F` | ~10–15× | WebCodecs `AudioDecoder` (`opus`) | not by Windows (see the "Windows seems to be confused … opus" note in `Rdpsnd::client_formats`); yes by `ironrdp-server` |

The ~10× win against Windows hosts most likely comes from AAC, not Opus;
ADPCM is the cheap, dependency-free ~4× step. Phase 1 logs the negotiated
format so phase 2 can confirm the real server offer before committing to a
codec order.

## Architecture

New module tree in `crates/ironrdp-web/src/audio/`:

```
audio/
  mod.rs      WasmRdpsndBackend: RdpsndClientHandler impl; owns Player + volume;
              routes wave blocks to the decoder for the block's format
  codec.rs    advertised format table + decoders: bytes → DecodedAudio
  player.rs   AudioContext + jitter-buffer scheduler: DecodedAudio → speakers
```

Data flow:

```
Rdpsnd (SVC) ──wave(format, data)──▶ WasmRdpsndBackend
                                        │ codec::decode(format, data, volume)
                                        ▼
                                   DecodedAudio { sample_rate, planar f32 channels }
                                        │
                                        ▼
                                   Player::enqueue ──▶ AudioBufferSourceNode.start(t)
```

### Units

**`codec`** — pure Rust, no `web-sys`, unit-testable on the host.

- `supported_formats() -> Vec<AudioFormat>`: the advertised list, best first.
  Phase 1: PCM 16-bit at 48 k / 44.1 k / 22.05 k / 16 k stereo and
  44.1 k / 22.05 k / 16 k / 8 k mono (the native backend's PCM list).
- `DecodedAudio { sample_rate: u32, channels: Vec<Vec<f32>> }` (planar, the
  shape `AudioBuffer::copy_to_channel` wants).
- `decode(format, data, volume) -> Option<DecodedAudio>`: dispatches on
  `format.format`. Phase 1 handles `WaveFormat::PCM` with 16 bits per sample;
  anything else returns `None` (and is never advertised).
- Volume is applied during the s16→f32 conversion: per-lane linear scale
  `vol / 0xFFFF`, left on even lanes, right on odd, mono uses left — same
  semantics as `ironrdp_rdpsnd_native::cpal::apply_volume`, no extra audio
  nodes. Wave blocks are whole frames (`n_block_align`), so lane phase restarts
  at 0 per block; a trailing partial frame is dropped.

Sync decoders (PCM, A-law/µ-law, ADPCM) fit `decode` directly. Async decoders
(WebCodecs) cannot return synchronously; phase 3 adds a second path where the
backend submits the encoded chunk and the decoder's output callback calls
`Player::enqueue` itself; phase 3 moves the player behind `Rc<RefCell<_>>` so
that callback can reach it (phase 1 owns it directly — nothing else needs it
yet). `decode` stays the sync fast path.

**`player`** — owns the `AudioContext`; the only unit touching Web Audio.

- `Player::new() -> anyhow::Result<Self>` creates the context.
- `enqueue(DecodedAudio)` builds an `AudioBuffer` at the block's own sample
  rate (Web Audio resamples to the device rate) and schedules an
  `AudioBufferSourceNode` at `next_start`.
- `reset()` forgets the schedule (format change / server Close).
- Scheduling math lives in a pure `Schedule` struct, unit-tested on the host:
  - `LEAD = 0.05 s`: initial/after-underrun lead so the first block is not
    clipped and small arrival jitter is absorbed.
  - `MAX_QUEUED = 0.5 s`: if `next_start - now` already exceeds this, the block
    is **dropped**, and arrivals keep being dropped until the queue drains below
    `LOW_WATERMARK = 2 × LEAD` (hysteresis). `Rdpsnd` confirms immediately, so the
    server is not throttled by playback; without the cap a stall (tab
    backgrounded, GC pause, network burst) would grow latency unbounded, and
    without the hysteresis it would pin latency at the cap forever.
  - Underrun: if `next_start < now`, restart at `now + LEAD`.
- Autoplay policy: the context may start `suspended`. Playback asks `resume()`
  once; after that, `Session::apply_inputs` (called synchronously from DOM input
  handlers, i.e. within user activation) resumes it whenever the state is
  neither `running` nor `closed` — which also covers Safari's `interrupted`,
  a state web-sys does not model.
- `Drop` calls `AudioContext::close()`.

**`WasmRdpsndBackend`** (`audio/mod.rs`) — the `RdpsndClientHandler`.

- `get_flags`: `VOLUME` (implemented in `codec`). Not `PITCH`.
- `get_formats`: `codec::supported_formats()` computed once.
- `wave`: `codec::decode` then `player.enqueue`; logs the negotiated format at
  INFO the first time a format is seen (`Playing RDPSND format`), so we learn
  what the server picked.
- `set_volume`: store `(left, right)`; applies from the next block.
- `close`: `player.reset()`.
- `unsafe impl Send`, with the same single-thread SAFETY argument as
  `WasmDriveBackend`: `RdpsndClientHandler: Send` is a storage bound; this
  backend lives and dies on the wasm event loop.

Construction failure (no `AudioContext`, e.g. a locked-down embed) logs a
warning and falls back to `NoopRdpsndBackend` — audio failing must never fail
the connection, and RDPSND must stay attached for RDPDR.

### Configuration surface

Per the web-client API-surface rule, audio is enabled through the RDP
extension mechanism, not the protocol-agnostic `SessionBuilder` (it can be
promoted later if a second backend needs it).

- Rust: `|enable_audio_playback: bool|` in `SessionBuilder::extension`. When
  true: `config.enable_audio_playback = true` (clears `INFO_NOAUDIOPLAYBACK`)
  and `connect` attaches `Rdpsnd::new(Box::new(WasmRdpsndBackend))` instead of
  the no-op. Default false — existing embedders see no change.
- TS (`iron-remote-desktop-rdp`): `enableAudioPlayback(enable: boolean)` next
  to `displayControl` in `main.ts`.
- Svelte test client: a "Play remote audio" checkbox on the login form, wired
  to the extension.

## Error handling

- Malformed/short wave payloads: `decode` drops a trailing partial frame; an
  empty result is skipped. Never panic on server data.
- Undecodable format in `wave` (cannot happen unless negotiation and table
  disagree): visible once via the `Playing RDPSND format` line, then each
  block is dropped at TRACE.
- `AudioBuffer` creation / `start` errors: `warn!` and drop the block; do not
  tear down the player.
- All `web-sys` calls return `Result`; nothing `unwrap`s a `JsValue` error.

## Testing

- Host unit tests (`cargo test -p ironrdp-web --lib audio`):
  - s16le stereo/mono → planar f32 (range, sign, channel split).
  - volume scaling (full, zero, asymmetric L/R).
  - trailing partial frame dropped; empty input → `None`.
  - non-PCM / 8-bit → `None`.
  - `supported_formats` all PCM 16-bit with consistent `n_block_align` /
    `n_avg_bytes_per_sec`.
  - `Schedule`: first block at `now + LEAD`; contiguous blocks back-to-back;
    underrun restarts; over-cap blocks dropped.
- `cargo xtask check lints -v`, `cargo xtask wasm check -v`,
  `cargo xtask web check -v`.
- Manual: connect with audio enabled to the Windows test host, play a sound,
  confirm audio and the `Playing RDPSND format` log line; confirm RDPDR drive
  share still works with audio on and off.

Note: 19 `clipboard::tests` already fail natively at baseline (they call into
JS). Not touched by this work.

## Phases

1. **PCM** (this change): everything above.
2. **ADPCM / G.711**: add IMA ADPCM, MS ADPCM, A-law, µ-law decoders to
   `codec` (pure Rust, sync path, ~4× / 2×). Order the advertised list
   compressed-first only after confirming from phase-1 logs what Windows
   offers. Add an extension to prefer bandwidth over fidelity if useful.
3. **WebCodecs (AAC, Opus)**: feature-detect `AudioDecoder.isConfigSupported`
   at backend construction and advertise only supported configs; async decode
   path feeding `Player::enqueue` from the output callback; AAC needs the
   `AudioSpecificConfig` from the format's `data` (`cbSize` extra bytes) as the
   decoder `description`.
4. **AudioWorklet player** (optional): replace per-block source nodes with a
   ring buffer in a worklet if per-block scheduling shows glitches at small
   block sizes or drift over long sessions. `Player`'s interface stays.
