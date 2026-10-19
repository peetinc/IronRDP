# Web audio playout on the audio thread: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Gapless RDPSND playback over a jittery WAN: an AudioWorklet ring buffer whose target
rises to cover the gaps it sees, read by one continuous cubic resampler.

**Architecture:**
- **Player:** `player.rs` loads `playout_worklet.js` (embedded) into an `AudioWorkletNode` and
  posts decoded planar f32 blocks to it.
- **Worklet:** a pure `Playout` class (ring, Buffering/Playing states, target ratchet, reader)
  inside a thin `AudioWorkletProcessor`.
- **Diagnostics:** a server-timestamp delay window and a 20 ms main-thread heartbeat, logged
  every 10 s.

**Tech Stack:** Rust/wasm-bindgen (ironrdp-web), JavaScript ES module (AudioWorklet), vitest
(iron-remote-desktop-rdp).

**Spec:** `docs/plans/2026-10-19-web-audio-playout-design.md`

## Global Constraints

- **Constants:**
  - `TARGET_START` 400 ms
  - `TARGET_CEIL` 2000 ms
  - `GAP_MARGIN` 100 ms
  - `PREROLL_CAP` 300 ms
  - ring 4 s
  - fade in 5 ms
  - fade out when < 2 ms remain
  - overflow fade 5 ms
  - pre-ready queue 2 s
  - heartbeat 20 ms
  - stats every 1 s from the worklet, logged every 10 s at INFO
- **Unchanged:** the RDPSND protocol, WaveConfirm timing, formats, codecs and volume.
- **`process()` never allocates.** Blocks are copied into the ring in `onmessage`.
- **No new web-sys features.** Bind the browser APIs with `#[wasm_bindgen] extern` blocks, as
  `aac.rs` and `h264.rs` do.
- **Build:** WASM needs
  `CC_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/clang AR_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/llvm-ar`.
  Run `cargo xtask web build -v`; the dist commit updates `web-client/DIST_SOURCE_COMMIT`.
- **Formatting:** `rustfmt --edition 2024` on my hunks only.
- **Do not commit** `vite.config.ts` or `static/bench/`.

## Review Focus

1. Burst after a gap: no audio is discarded, and underruns stop after adaptation.
2. Block boundaries and the 44.1→48 kHz conversion produce no discontinuity.
3. A suspended context drops blocks; it doesn't queue them unboundedly.
4. Format change: the ring drains, then the rate switches without a glitch or crash.
5. A failed worklet load, or a processor error, goes silent with one WARN. No panic, no busy
   loop.

---

### Task 1: The worklet module and its tests

**Files:**
- Create: `crates/ironrdp-web/src/audio/playout_worklet.js`
- Create: `web-client/iron-remote-desktop-rdp/src/playout_worklet.test.ts`

**Interfaces:**
- **Produces:** `export class Playout`, with:
  - `constructor(contextRate: number)`;
  - `push(rate: number, channels: Float32Array[]): void`;
  - `render(outL: Float32Array, outR: Float32Array): void`, which fills one quantum;
  - `stats(): {depthMsMean, depthMsMin, depthMsMax, targetMs, underruns, gapMsMax, droppedMs, ignored}`;
  - `resetStats()`.
- **Messages:** `{type:'block', rate, channels}` from the main thread, and `{type:'stats', ...}`
  to it.

- [ ] **Step 1: Write the failing tests.** In `playout_worklet.test.ts`:
  - `burst pattern from the log plays without gaps after adaptation`. Feed 23.2 ms stereo
    blocks per the log's pattern, simulating time by rendering 128-frame quanta at 48 kHz
    between pushes: steady blocks, a 650 ms gap, then 50 blocks at once, repeated 5 times.
    Assert:
    - `droppedMs === 0`;
    - no underrun after the second cycle.
  - `sine has no discontinuity across blocks and rate conversion`: a 1 kHz sine at 44.1 kHz
    in 1024-frame blocks, read at 48 kHz. The max |Δsample| is ≤ the sine's max step × 1.05.
  - `underrun raises target to gap plus margin, capped at ceiling`.
  - `full ring drops oldest with fade and counts it`.
- [ ] **Step 2: Run them.**
  `cd web-client/iron-remote-desktop-rdp && npx vitest run src/playout_worklet.test.ts`.
  Expected: FAIL (module missing).
- [ ] **Step 3: Implement** `playout_worklet.js` per spec §2. Register the processor guarded by
  `typeof registerProcessor === 'function'`.
- [ ] **Step 4: Run them again.** Expected: 4 pass.
- [ ] **Step 5: Commit:** `feat(web): AudioWorklet playout ring for RDPSND`.

### Task 2: Wire the player to the worklet, plus diagnostics

**Files:**
- Modify: `crates/ironrdp-web/src/audio/player.rs`: rewrite around the worklet; remove
  `Schedule` and the buffer-source path.
- Modify: `crates/ironrdp-web/src/audio/mod.rs`: `wave()` passes `ts` through,
  `enqueue(audio, ts)`.
- Modify: `crates/ironrdp-web/src/audio/aac.rs`: the `try_borrow_mut` drop becomes `warn!`.
  The AAC path carries `ts`: blocks keep their ts in decode order.

**Interfaces:**
- **Consumes:** the worklet messages from Task 1.
- **Produces:**
  - `Player::enqueue(&mut self, audio: &DecodedAudio, server_ts: Option<u32>)`;
  - pure `DelayWindow { observe(local_ms, server_ts), take() -> (min, cur, max) }`;
  - pure `PendingBlocks` (bounded at 2 s).

- [ ] **Step 1: Write the failing tests.** Native, in `player.rs`:
  - `pending_blocks_keep_at_most_two_seconds`;
  - `delay_window_reports_delay_above_best_seen`, which also covers ts wrap at u32.
- [ ] **Step 2: Run them.** `cargo test -p ironrdp-web --lib audio::player`. Expected: compile
  failure.
- [ ] **Step 3: Implement.**
  1. Blob URL from `include_str!("playout_worklet.js")`.
  2. `audioWorklet.addModule` (async, `spawn_local`).
  3. Create `AudioWorkletNode('rdpsnd-playout', {numberOfOutputs:1, outputChannelCount:[2]})`.
  4. Wire `port.onmessage` for stats, and `onprocessorerror` (WARN, go silent).
  5. Flush the pending blocks once ready.
  6. Keep the suspended-state drop and the gesture resume.
  7. Add the heartbeat `setInterval(20)`, recording the max lateness.
  8. Log every 10 s: INFO `Audio playout stats` with the worklet stats, delay min/current/max
     and `heartbeatLateMsMax`.
- [ ] **Step 4: Run the tests.** `cargo test -p ironrdp-web --lib`. Expected: new tests pass;
  only the 19 known clipboard failures remain. Then lint:
  `cargo clippy -p ironrdp-web --target wasm32-unknown-unknown` with the CC/AR env must be
  clean for `audio/`.
- [ ] **Step 5: Commit:** `feat(web): play RDPSND through the AudioWorklet ring, with WAN diagnostics`.

### Task 3: Build, dist, and live acceptance

- [ ] **Step 1: Build.** `cargo xtask web build -v`, then commit the dist with
  `DIST_SOURCE_COMMIT`.
- [ ] **Step 2: Local smoke.** On the local rig, connect with audio. The console must show
  `Audio playout stats` every 10 s, with no WARN about the worklet.
- [ ] **Step 3: User's ac-v4 → Tailscale run with the same video, then lb-dev over QUIC.**
  Accept when:
  - there are no audible gaps for 3 minutes;
  - `droppedMs` is 0;
  - underruns stop after the first adaptations.

  Record the stats lines in the ledger.
