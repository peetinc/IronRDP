# EGFX pipelined H.264 and frame-ack policy: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Decode several H.264 frames at once, and acknowledge frames as Windows App does, so
Windows stops throttling the web client.

**Architecture:**
- **ironrdp-egfx:** one ordered pending queue replaces the single deferred frame plus backlog.
  H.264 access units are submitted ahead in wire order, up to the decoder's in-flight limit.
  A `FrameAckPolicy` adds QoE acks, and suspends and resumes acknowledgements.
- **ironrdp-web:** the WebCodecs decoder keeps an ordered list of in-flight entries, and opts in
  to the new policy.

**Tech Stack:** Rust (ironrdp-egfx, ironrdp-web/wasm), WebCodecs, TypeScript (iron-remote-desktop-rdp).

**Spec:** `docs/plans/2026-10-19-egfx-pipelined-h264-acks-design.md`

## Global Constraints

- **Edition and toolchain:** Edition 2024, Rust 1.94.1. Format with `rustfmt --edition 2024
  --check <file>`, on my own hunks only. Do not run `cargo fmt -p` (it reformats unrelated
  files).
- **ironrdp-egfx stays wall-clock-free:** time arrives only through the injected
  `clock: fn() -> u64`, in milliseconds.
- **Default behaviour is unchanged:** a client without `with_frame_ack_policy` sends the same
  PDUs as before.
- **Constants:**
  - `SUSPEND_AFTER = 50` frames
  - `RESUME_AT = 32` queued entries
  - QoE receipt-time map bounded at `256`
  - WebCodecs `max_in_flight() = 4`
  - `MAX_HELD_PER_OWNER = 8`
  - `MAX_DEFERRED_BACKLOG = 4096`
- **Builds:**
  - WASM: `CC_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/clang AR_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/llvm-ar`.
  - Dist: `cargo xtask web build -v`, and the dist commit updates `web-client/DIST_SOURCE_COMMIT`.
- **Do not commit:**
  - `web-client/iron-svelte-client/vite.config.ts` (temporary profiler header);
  - `static/bench/` captures.
- **Style:**
  - log messages capitalised, no trailing period;
  - error messages lowercase;
  - tests at the end of the file;
  - no single-use helper functions.

## Review Focus

1. Wire order holds with frames in flight: a deferred decode found on replay goes to the queue
   front.
2. No access unit is submitted ahead of a queued `ResetGraphics` or `CapabilitiesConfirm`.
3. Every `VideoFrame` is closed: applied, dropped, decoder failure, session end, beyond the cap.
4. Suspended acknowledgements resume from dispatch when the queue reaches 32, so buffering stays
   bounded.
5. `EveryFrame` (the default) is byte-identical to today.

The tests for 1–5 live in Tasks 1–3.

---

### Task 1: Ordered pending queue and submit-ahead (ironrdp-egfx)

**Files:**
- Modify: `crates/ironrdp-egfx/src/decode.rs` (trait `H264Decoder`)
- Modify: `crates/ironrdp-egfx/src/client.rs`: fields `deferred_avc420` and `backlog`, and
  functions `is_awaiting_decoder`, `resume`, `dispatch_pdu`, `submit_avc420`, `decode_avc444`,
  plus the `ResetGraphics` clearing of the queue
- Test: `crates/ironrdp-egfx/src/client.rs`, `mod tests`

**Interfaces:**
- Produces:
  - `H264Decoder::max_in_flight(&self) -> usize`, default `1`.
  - `GraphicsPipelineClient::is_awaiting_decoder()`: unchanged signature, true while any
    `Pending::Avc420` is queued.
  - `GraphicsPipelineClient::queued_len(&self) -> usize`, crate-private, used by Task 2.

- [ ] **Step 1: Write the failing tests.** Use a scripted async decoder, `PipelineDecoder`
  (`max_in_flight` configurable). It records each `submit` payload, and returns queued results
  from `poll_frame` in order.
  - `two_h264_frames_decode_at_once_and_apply_in_order`: dispatch AVC(A), AVC(B) with limit 4.
    Assert 2 submissions before any poll. Provide frames A then B, `resume()`, and assert the
    surface shows B's pixels over A's region order.
  - `commands_between_h264_frames_apply_between_them`: AVC(A), fill(red), AVC(B over the same
    rect). Assert the final pixel is B's, and that the red fill applied after A (check through a
    recording handler, ordering A, fill, B).
  - `submit_ahead_stops_at_reset_graphics`: AVC(A), ResetGraphics, AVC(B). Assert 1 submission
    until A is resumed, then 2.
  - `submit_ahead_respects_the_decoders_in_flight_limit`: limit 2, AVC×3. Assert 2 submissions,
    resume one, assert 3.
  - `a_failed_frame_ahead_of_a_good_one_drops_only_itself`: AVC(A) Err, AVC(B) Ok. Assert B
    applies and A leaves no update.
- [ ] **Step 2: Run them.**
  `cargo test -p ironrdp-egfx --lib client::tests 2>&1 | tail -30`.
  Expected: compile error (no `max_in_flight`) or FAIL on the submission counts.
- [ ] **Step 3: Implement** the `Pending` enum, `pending: VecDeque<Pending>`, `submit_ahead`,
  the new `resume` and `dispatch_pdu` per spec §1, and `max_in_flight` per spec §2.
  - A deferral found during replay is pushed to the front.
  - Barriers are `ResetGraphics` and `CapabilitiesConfirm`.
  - Existing deferred tests (`deferred_avc420_*`, `deferred_backlog_is_bounded`) must still pass.
- [ ] **Step 4: Run the tests.**
  `cargo test -p ironrdp-egfx 2>&1 | grep -E "test result|FAILED|panicked"`. Expected: all pass.
- [ ] **Step 5: Lint and format.**
  `cargo clippy -p ironrdp-egfx --all-targets 2>&1 | grep -E "^(warning|error)" -A5 | head`.
  Expected: nothing from `crates/ironrdp-egfx`. Then `rustfmt --edition 2024 --check` on the two
  files, own hunks only.
- [ ] **Step 6: Commit:** `feat(egfx): decode several H.264 frames at once, applying commands in order`.

### Task 2: Frame-ack policy with QoE (ironrdp-egfx)

**Files:**
- Modify: `crates/ironrdp-egfx/src/client.rs`:
  - new `FrameAckPolicy`, `with_frame_ack_policy`;
  - the `StartFrame`/`EndFrame` receipt times in `dispatch_pdu`;
  - `handle_end_frame`, and the dispatch-time resume.
- Test: same `mod tests`.

**Interfaces:**
- Consumes: `queued_len()` from Task 1.
- Produces:
  - `pub enum FrameAckPolicy { EveryFrame, SuspendWhenCaughtUp }` (`Default` = `EveryFrame`);
  - `pub fn with_frame_ack_policy(self, policy: FrameAckPolicy, clock: fn() -> u64) -> Self`.

- [ ] **Step 1: Write the failing tests.** Use a test clock (a `static AtomicU64` read by a
  `fn`).
  - `every_frame_policy_acks_each_frame_without_qoe`: 3 frames. Assert 3 `FrameAcknowledge`, all
    `Unavailable`, and no QoE.
  - `suspend_policy_suspends_after_50_caught_up_frames`: 51 frames. Frames 1–49 are acked
    `Unavailable`, frame 50's ack is `Suspend`, frame 51 has no `FrameAcknowledge`, and every
    frame has a QoE.
  - `suspended_client_resumes_with_a_queue_depth_when_the_queue_builds`: after suspension, hold an
    AVC pending and dispatch 32 more PDUs. Assert the dispatch reply has one `FrameAcknowledge`
    with `AvailableBytes(n > 0)` and the last applied frame id. Later frames are acked again.
  - `suspend_policy_resuspends_after_catching_up`: after resume, 50 more caught-up frames, then
    `Suspend` again.
  - `qoe_ack_carries_receipt_times_from_the_clock`: the clock is 1000 at StartFrame, 1007 at
    EndFrame dispatch, and 1010 at apply (AVC pending). Assert `timestamp 1000`, `diff_se 7`,
    `diff_dr 3`.
- [ ] **Step 2: Run them.** Expected: compile error (no `FrameAckPolicy`).
- [ ] **Step 3: Implement** per spec §5. QoE is sent only when the negotiated caps version is
  10 or later. The QoE receipt-time map is bounded at 256 (oldest dropped).
- [ ] **Step 4: Run the tests.** `cargo test -p ironrdp-egfx`. Expected: all pass, including
  `frame_ack_queue_depth_is_not_a_frame_count`.
- [ ] **Step 5: Lint and format,** as in Task 1.
- [ ] **Step 6: Commit:** `feat(egfx): optional Windows App-style frame acknowledgement with QoE`.

### Task 3: WebCodecs decoder with several frames in flight (ironrdp-web)

**Files:**
- Modify: `crates/ironrdp-web/src/h264.rs`:
  - `Shared`, `Slot::submit`, `decode_chunk`, the output and error callbacks, `close`;
  - `H264Decoder` impl: `max_in_flight`, `poll_frame`.
- Modify: `crates/ironrdp-web/src/gpu/frames.rs`: `Store::insert`, plus a `MAX_HELD_PER_OWNER`
  cap.
- Test: `mod tests` in both files.

**Interfaces:**
- Consumes: `H264Decoder::max_in_flight` (Task 1).
- Produces: a crate-private `struct InFlight` with these methods:
  - `push(regions)`
  - `push_failed(err)`
  - `fill_next(result) -> bool`
  - `fail_waiting(msg)`
  - `pop_ready() -> Option<DecoderResult<DecodedFrame>>`
  - `next_regions() -> Option<&[ExclusiveRectangle]>`

- [ ] **Step 1: Write the failing tests.**
  - `in_flight_entries_complete_in_submission_order`: push A, push B, `fill_next(B frame)`.
    Asserting the fill landed in A: `pop_ready` yields A's result, then nothing until the next
    fill.
  - `a_failed_submission_keeps_its_place`: push A, `push_failed(err)`. `pop_ready` yields `None`
    until A is filled, then A, then the error.
  - `failing_all_fails_only_waiting_entries`.
  - In `frames.rs`: `an_owner_can_hold_several_frames`, `insert_closes_only_beyond_the_cap`.
    These replace `insert_hands_back_only_the_same_owners_stale_items`.
- [ ] **Step 2: Run them.**
  `cargo test -p ironrdp-web --lib h264 gpu::frames 2>&1 | tail -20`. Expected: compile errors.
- [ ] **Step 3: Implement** per spec §3–4. CPU readback reads `next_regions()`. GPU output
  fills `fill_next`. The error callback and the closed-decoder check call `fail_waiting`.
  `max_in_flight()` returns 4.
- [ ] **Step 4: Run the tests.** `cargo test -p ironrdp-web --lib`. Expected: the new tests
  pass. The 19 pre-existing clipboard failures are unchanged.
- [ ] **Step 5: Lint.** `cargo clippy -p ironrdp-web --target wasm32-unknown-unknown`, with the
  CC/AR env. Expected: no warnings in `h264.rs` or `gpu/`.
- [ ] **Step 6: Commit:** `feat(web): keep several H.264 frames in flight in WebCodecs`.

### Task 4: Wire the policy into the web client, rebuild dist, bench

**Files:**
- Modify: `crates/ironrdp-web/src/session.rs`: extension parse of `frame_ack_suspend`, and the
  EGFX client build.
- Modify: `crates/ironrdp-web/src/bench.rs`: same policy.
- Modify: `web-client/iron-remote-desktop-rdp/src/main.ts`: `frameAckSuspend(enable)` with JSDoc.
- Modify: the dist artifacts, plus `web-client/DIST_SOURCE_COMMIT`.

**Interfaces:**
- Consumes:
  - `with_frame_ack_policy(FrameAckPolicy::SuspendWhenCaughtUp, clock)` from Task 2;
  - `fn() -> u64` = `js_sys::Date::now` (truncated to whole ms).

- [ ] **Step 1: Implement** the extension (default `true`) and the client build. `bench.rs` uses
  the same policy.
- [ ] **Step 2: Build.** `cargo xtask web build -v`. Expected: exit 0.
- [ ] **Step 3: Bench.** `/bench?capture=mixed-17-31.bin&h264=1&gpu=1`, plus
  `live-local-4k.bin`, plus `&verify=1`. Expected:
  - 0 errors;
  - fps ≥ 41 (mixed) and ≥ 70 (live-local-4k), today's figures;
  - verify max diff ≤ 1.
- [ ] **Step 4: Commits:**
  - `feat(web): acknowledge EGFX frames the way Windows App does`;
  - `chore(web-client): rebuild dist artifacts (pipelined H.264, frame-ack policy)`.

### Task 5: Live acceptance

- [ ] **Step 1: Live run.** Connect on the local rig at 3840x2160 with the same video, and read
  the host's RemoteFX counters for 3 minutes.
- [ ] **Step 2: Expected:**
  - sent rate ≥ 12 Mbit/s average;
  - frame quality ≥ 99;
  - client-skipped frames < 1/s;
  - no crash, no queue overflow;
  - "Suspended frame acknowledgements" logged.
- [ ] **Step 3: Record** the numbers in the ledger. If they are not met, debug with
  superpowers:systematic-debugging before any change.
