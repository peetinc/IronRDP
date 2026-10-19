// RDPSND playout, run on the audio rendering thread as an AudioWorklet.
//
// The main thread posts decoded planar f32 blocks (`{ type: 'block', rate, channels, lateMs }`);
// this module keeps them in a ring buffer and plays them through one continuous fractional
// reader. Blocks reach the main thread in bursts on a jittery WAN link, so playback waits until
// the ring holds `target` of audio. `target` covers the latest arrival of the last
// `LATE_WINDOW_SECS`: `lateMs`, how far behind its server timestamp a block arrived. A pause in
// the stream (Windows stops sending while nothing plays) resumes with fresh blocks and raises
// nothing. Without `lateMs`, the outage that emptied the ring counts instead.
//
// Audio stays in step with the picture before it plays without gaps: `target` is at most
// `TARGET_CEIL_MS`, so a longer stall costs a gap, and the audio it held back is skipped. Bursts only refill what a gap drained, so the ring holds about
// `target` in steady play. More than that means playback stopped while audio kept arriving
// (a hidden tab, an output device switch): the reader then fades out, skips to the newest
// `target` of audio and fades back in, so sound stays in step with the picture.
//
// `Playout` is pure (no AudioWorklet globals) so it can be unit-tested; the processor at the
// bottom only adapts it to the AudioWorklet API. See
// `docs/plans/2026-10-19-web-audio-playout-design.md`.

const TARGET_FLOOR_MS = 120;
const TARGET_CEIL_MS = 300;
// Added to the latest arrival when setting the target.
const GAP_MARGIN_MS = 60;
// How long a late arrival keeps the target raised.
const LATE_WINDOW_SECS = 30;
const RING_SECS = 4;
// A larger `lateMs` is a broken server clock (a wrapped 16-bit timestamp), not a stall.
const LATE_VALID_MS = 10000;
const FADE_IN_MS = 5;
// Playback fades out once less than this much audio remains, so running dry never clicks.
const FADE_OUT_MS = 2;
// Depth beyond `target` that makes the reader skip the excess, and the fade around the skip.
const TRIM_MARGIN_MS = 60;
const TRIM_FADE_MS = 5;
const STATS_INTERVAL_SECS = 1;

const BUFFERING = 0;
const PLAYING = 1;

export class Playout {
    constructor(contextRate) {
        this.contextRate = contextRate;
        this.rate = 0;
        this.capacity = 0;
        this.channels = [];
        // Absolute frame counts at the stream rate. `head` is the fractional read position.
        this.written = 0;
        this.head = 0;
        this.state = BUFFERING;
        this.targetMs = TARGET_FLOOR_MS;
        // The latest arrival in each of the last `LATE_WINDOW_SECS` seconds, by context clock.
        this.lateBySecond = new Float64Array(LATE_WINDOW_SECS);
        this.lateSecond = 0;
        this.nextSecondClock = contextRate;
        this.gain = 0;
        // Context frames rendered so far: the module's clock.
        this.clock = 0;
        this.lastPushClock = -1;
        this.awaitingRefill = false;
        this.underruns = 0;
        this.gapMsMax = 0;
        this.droppedMs = 0;
        this.trimming = false;
        this.trimmedMs = 0;
        this.ignored = 0;
        this.resetStats();
    }

    /**
     * Adds one block of planar samples at `rate`. A mono block plays on both outputs. `lateMs`,
     * when known, is how much later than the earliest block seen this one arrived.
     */
    push(rate, channels, lateMs) {
        if (!(rate > 0) || !Array.isArray(channels) || channels.length === 0 || channels[0].length === 0) {
            this.ignored++;
            return;
        }
        if (rate !== this.rate) {
            this.switchRate(rate);
        }

        if (Number.isFinite(lateMs) && 0 <= lateMs && lateMs < LATE_VALID_MS) {
            this.noteLate(lateMs);
            this.awaitingRefill = false;
        } else if (this.awaitingRefill && this.lastPushClock >= 0) {
            // The outage that emptied the ring: from the last arrival before it to this one.
            this.noteLate(((this.clock - this.lastPushClock) / this.contextRate) * 1000);
            this.awaitingRefill = false;
        }
        this.lastPushClock = this.clock;

        const frames = channels[0].length;
        const overflow = this.written + frames - this.head - this.capacity;
        if (0 < overflow) {
            // Ring full: keep the newest audio. The reader jumps forward, then fades back in.
            this.head += overflow;
            this.droppedMs += (overflow / this.rate) * 1000;
            this.gain = 0;
        }

        for (let c = 0; c < this.channels.length; c++) {
            const source = channels[Math.min(c, channels.length - 1)];
            const ring = this.channels[c];
            for (let i = 0; i < frames; i++) {
                ring[(this.written + i) % this.capacity] = source[i];
            }
        }
        this.written += frames;
    }

    /** Fills one render quantum of the two outputs. Never allocates. */
    render(outLeft, outRight) {
        const length = outLeft.length;
        const step = this.rate / this.contextRate;
        const fadeInStep = 1000 / (FADE_IN_MS * this.contextRate);
        const fadeOutFrames = (FADE_OUT_MS / 1000) * this.rate;
        const trimFadeStep = 1000 / (TRIM_FADE_MS * this.contextRate);
        if (this.state === PLAYING && this.targetMs + TRIM_MARGIN_MS < this.depthMs()) {
            this.trimming = true;
        }

        for (let i = 0; i < length; i++) {
            if (this.state === BUFFERING && this.rate > 0 && this.targetMs <= this.depthMs()) {
                this.state = PLAYING;
                this.gain = 0;
                // Output is silent here, so a backlog is skipped without a fade.
                this.skipToTarget();
            }

            const remaining = this.written - this.head;
            if (this.state !== PLAYING || remaining < 3) {
                if (this.state === PLAYING) {
                    this.state = BUFFERING;
                    this.underruns++;
                    this.awaitingRefill = true;
                }
                outLeft[i] = 0;
                outRight[i] = 0;
                continue;
            }

            let gain;
            if (this.trimming) {
                gain = Math.max(0, this.gain - trimFadeStep);
                if (gain === 0) {
                    this.skipToTarget();
                    this.trimming = false;
                }
            } else {
                gain = Math.min(1, this.gain + fadeInStep);
            }
            if (remaining < fadeOutFrames) {
                gain = Math.min(gain, remaining / fadeOutFrames);
            }
            this.gain = gain;

            const left = this.sample(0) * gain;
            outLeft[i] = left;
            outRight[i] = this.channels.length > 1 ? this.sample(1) * gain : left;
            this.head += step;
        }

        this.clock += length;
        if (this.nextSecondClock <= this.clock) {
            this.nextSecondClock += this.contextRate;
            this.lateSecond = (this.lateSecond + 1) % LATE_WINDOW_SECS;
            this.lateBySecond[this.lateSecond] = 0;
            this.updateTarget();
        }
        const depth = this.depthMs();
        this.depthSum += depth;
        this.depthCount++;
        this.depthMin = Math.min(this.depthMin, depth);
        this.depthMax = Math.max(this.depthMax, depth);
    }

    /** Catmull-Rom interpolation of channel `c` at the read position. */
    sample(c) {
        const ring = this.channels[c];
        const base = Math.floor(this.head);
        const t = this.head - base;
        const oldest = Math.max(0, this.written - this.capacity);
        const at = (index) => ring[Math.max(index, oldest) % this.capacity];
        const p0 = at(base - 1);
        const p1 = at(base);
        const p2 = at(base + 1);
        const p3 = at(base + 2);
        return (
            p1 +
            0.5 * t * (p2 - p0 + t * (2 * p0 - 5 * p1 + 4 * p2 - p3 + t * (3 * (p1 - p2) + p3 - p0)))
        );
    }

    noteLate(lateMs) {
        this.gapMsMax = Math.max(this.gapMsMax, lateMs);
        this.lateBySecond[this.lateSecond] = Math.max(this.lateBySecond[this.lateSecond], lateMs);
        this.updateTarget();
    }

    updateTarget() {
        let late = 0;
        for (let i = 0; i < LATE_WINDOW_SECS; i++) {
            late = Math.max(late, this.lateBySecond[i]);
        }
        this.targetMs = Math.min(TARGET_CEIL_MS, Math.max(TARGET_FLOOR_MS, late + GAP_MARGIN_MS));
    }

    /** Moves the read position to the newest `target` of audio, counting what it skips. */
    skipToTarget() {
        const keep = Math.ceil((this.targetMs / 1000) * this.rate);
        const skip = this.written - keep - this.head;
        if (0 < skip) {
            this.head += skip;
            this.trimmedMs += (skip / this.rate) * 1000;
        }
    }

    depthMs() {
        return this.rate > 0 ? (Math.max(0, this.written - this.head) / this.rate) * 1000 : 0;
    }

    /** Starts a new stream at `rate`; audio still queued at the old rate is discarded. */
    switchRate(rate) {
        if (this.rate > 0) {
            this.droppedMs += this.depthMs();
        }
        this.rate = rate;
        this.capacity = Math.ceil(rate * RING_SECS);
        this.channels = [new Float32Array(this.capacity), new Float32Array(this.capacity)];
        this.written = 0;
        this.head = 0;
        this.state = BUFFERING;
        this.gain = 0;
        this.trimming = false;
    }

    stats() {
        return {
            depthMsMean: this.depthCount > 0 ? this.depthSum / this.depthCount : this.depthMs(),
            depthMsMin: this.depthCount > 0 ? this.depthMin : this.depthMs(),
            depthMsMax: this.depthCount > 0 ? this.depthMax : this.depthMs(),
            targetMs: this.targetMs,
            underruns: this.underruns,
            gapMsMax: this.gapMsMax,
            droppedMs: this.droppedMs,
            trimmedMs: this.trimmedMs,
            ignored: this.ignored,
        };
    }

    /** Starts a new window for the depth statistics. The counters are session totals. */
    resetStats() {
        this.depthSum = 0;
        this.depthCount = 0;
        this.depthMin = Infinity;
        this.depthMax = 0;
    }
}

if (typeof registerProcessor === 'function') {
    class PlayoutProcessor extends AudioWorkletProcessor {
        constructor() {
            super();
            // `sampleRate` and `currentTime` are AudioWorkletGlobalScope globals.
            this.playout = new Playout(sampleRate);
            this.nextStats = currentTime + STATS_INTERVAL_SECS;
            this.port.onmessage = (event) => {
                const message = event.data;
                if (message && message.type === 'block') {
                    this.playout.push(message.rate, message.channels, message.lateMs);
                }
            };
        }

        process(_inputs, outputs) {
            const output = outputs[0];
            if (output && output.length > 0) {
                this.playout.render(output[0], output.length > 1 ? output[1] : output[0]);
            }
            if (this.nextStats <= currentTime) {
                this.port.postMessage({ type: 'stats', ...this.playout.stats() });
                this.playout.resetStats();
                this.nextStats = currentTime + STATS_INTERVAL_SECS;
            }
            return true;
        }
    }

    registerProcessor('rdpsnd-playout', PlayoutProcessor);
}
