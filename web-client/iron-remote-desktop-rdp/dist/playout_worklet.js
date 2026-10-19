// RDPSND playout, run on the audio rendering thread as an AudioWorklet.
//
// The main thread posts decoded planar f32 blocks (`{ type: 'block', rate, channels }`); this
// module keeps them in a ring buffer and plays them through one continuous fractional reader.
// Blocks reach the main thread in bursts on a jittery WAN link, so playback waits until the
// ring holds `target` of audio and, when the ring runs dry, raises `target` to cover the
// outage that emptied it. Audio is never discarded except when the ring is full.
//
// `Playout` is pure (no AudioWorklet globals) so it can be unit-tested; the processor at the
// bottom only adapts it to the AudioWorklet API. See
// `docs/plans/2026-10-19-web-audio-playout-design.md`.

const TARGET_START_MS = 400;
const TARGET_CEIL_MS = 2000;
// Added to the outage that caused an underrun when raising the target.
const GAP_MARGIN_MS = 100;
const RING_SECS = 4;
const FADE_IN_MS = 5;
// Playback fades out once less than this much audio remains, so running dry never clicks.
const FADE_OUT_MS = 2;
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
        this.targetMs = TARGET_START_MS;
        this.gain = 0;
        // Context frames rendered so far: the module's clock.
        this.clock = 0;
        this.lastPushClock = -1;
        this.awaitingRefill = false;
        this.underruns = 0;
        this.gapMsMax = 0;
        this.droppedMs = 0;
        this.ignored = 0;
        this.resetStats();
    }

    /** Adds one block of planar samples at `rate`. A mono block plays on both outputs. */
    push(rate, channels) {
        if (!(rate > 0) || !Array.isArray(channels) || channels.length === 0 || channels[0].length === 0) {
            this.ignored++;
            return;
        }
        if (rate !== this.rate) {
            this.switchRate(rate);
        }

        if (this.awaitingRefill && this.lastPushClock >= 0) {
            // The outage that emptied the ring: from the last arrival before it to this one.
            const outageMs = ((this.clock - this.lastPushClock) / this.contextRate) * 1000;
            this.gapMsMax = Math.max(this.gapMsMax, outageMs);
            this.targetMs = Math.min(TARGET_CEIL_MS, Math.max(this.targetMs, outageMs + GAP_MARGIN_MS));
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

        for (let i = 0; i < length; i++) {
            if (this.state === BUFFERING && this.rate > 0 && this.targetMs <= this.depthMs()) {
                this.state = PLAYING;
                this.gain = 0;
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

            let gain = Math.min(1, this.gain + fadeInStep);
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
                    this.playout.push(message.rate, message.channels);
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
