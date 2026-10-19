import { describe, expect, it } from 'vitest';
// The AudioWorklet module lives with the Rust audio backend, which embeds it.
import { Playout } from '../../../crates/ironrdp-web/src/audio/playout_worklet.js';

const CONTEXT_RATE = 48000;
const STREAM_RATE = 44100;
const BLOCK_FRAMES = 1024;
const QUANTUM = 128;
const BLOCK_MS = (BLOCK_FRAMES / STREAM_RATE) * 1000;

/** Drives a `Playout` in simulated time: renders quanta and records what came out. */
class Rig {
    playout = new Playout(CONTEXT_RATE);
    left = new Float32Array(QUANTUM);
    right = new Float32Array(QUANTUM);
    renderedFrames = 0;
    output: number[] = [];

    get nowMs(): number {
        return (this.renderedFrames / CONTEXT_RATE) * 1000;
    }

    renderUntil(ms: number, keep = false): void {
        while (this.nowMs < ms) {
            this.playout.render(this.left, this.right);
            this.renderedFrames += QUANTUM;
            if (keep) {
                this.output.push(...this.left);
            }
        }
    }

    push(channels: Float32Array[], lateMs?: number): void {
        this.playout.push(STREAM_RATE, channels, lateMs);
    }
}

// The worklet's latency bounds: audio stays in step with the picture, at the cost of a
// gap when a stall outlasts the ceiling.
const TARGET_FLOOR_MS = 120;
const TARGET_CEIL_MS = 300;
const GAP_MARGIN_MS = 60;
const TRIM_MARGIN_MS = 60;

function silentBlock(): Float32Array[] {
    return [new Float32Array(BLOCK_FRAMES), new Float32Array(BLOCK_FRAMES)];
}

function blocksFor(ms: number): number {
    return Math.ceil(ms / BLOCK_MS);
}

describe('Playout', () => {
    /**
     * The server produces one block every 23.2 ms. The client receives them in clumps: every
     * block produced so far, delivered at once, separated by `gaps` (ms). Returns the deepest
     * the ring got, sampled every quantum.
     */
    function playBursts(rig: Rig, gaps: number[], onDelivery?: (index: number) => void): number {
        let delivered = 0;
        let now = 0;
        let deepest = 0;
        gaps.forEach((gap, index) => {
            const end = now + gap;
            while (rig.nowMs < end) {
                rig.renderUntil(rig.nowMs + 1);
                deepest = Math.max(deepest, rig.playout.depthMs());
            }
            now = end;
            const produced = Math.floor(now / BLOCK_MS);
            for (; delivered < produced; delivered++) {
                // How late the block arrives: it was produced at the end of its own span.
                rig.push(silentBlock(), now - (delivered + 1) * BLOCK_MS);
            }
            onDelivery?.(index);
        });
        return deepest;
    }

    it('moderate jitter plays without gaps after adaptation', () => {
        const gaps = [80, 100, 200, 90, 120, 150, 200, 100, 80, 200, 110, 90, 150, 100, 200, 100];
        const rig = new Rig();
        let underrunsAfterWarmup = -1;
        playBursts(rig, gaps, (index) => {
            if (index === 2) {
                // Warmup: the target adapts to the first late burst.
                underrunsAfterWarmup = rig.playout.stats().underruns;
            }
        });

        const stats = rig.playout.stats();
        expect(stats.underruns).toBe(underrunsAfterWarmup);
        expect(stats.droppedMs).toBe(0);
        expect(stats.targetMs).toBeLessThanOrEqual(TARGET_CEIL_MS);
    });

    it('stalls beyond the ceiling cost a gap, not latency', () => {
        // The captured WAN log: short gaps, then 650 ms stalls.
        const gaps = [80, 100, 650, 90, 120, 300, 650, 100, 80, 650, 110, 90, 650, 100, 650, 100];
        const rig = new Rig();
        const deepest = playBursts(rig, gaps);

        const stats = rig.playout.stats();
        expect(stats.targetMs).toBe(TARGET_CEIL_MS);
        // Stale audio is skipped rather than played late: depth never exceeds the ceiling by
        // more than the trim margin and one block.
        expect(deepest).toBeLessThanOrEqual(TARGET_CEIL_MS + TRIM_MARGIN_MS + BLOCK_MS);
        expect(stats.trimmedMs).toBeGreaterThan(0);
        expect(stats.droppedMs).toBe(0);
    });

    it('sine has no discontinuity across blocks and rate conversion', () => {
        const rig = new Rig();
        const frequency = 1000;
        let phase = 0;
        const sineBlock = (): Float32Array[] => {
            const samples = new Float32Array(BLOCK_FRAMES);
            for (let i = 0; i < BLOCK_FRAMES; i++) {
                samples[i] = Math.sin(phase);
                phase += (2 * Math.PI * frequency) / STREAM_RATE;
            }
            return [samples, samples.slice()];
        };
        // Keep the ring ahead of the reader for the whole run.
        for (let i = 0; i < blocksFor(1500); i++) {
            rig.push(sineBlock());
        }
        rig.renderUntil(1000, true);

        // Skip the start-up fade; then the output is a resampled sine.
        const settled = rig.output.slice(Math.ceil(CONTEXT_RATE * 0.05));
        const maxStep = (2 * Math.PI * frequency) / CONTEXT_RATE;
        let worst = 0;
        for (let i = 1; i < settled.length; i++) {
            worst = Math.max(worst, Math.abs(settled[i] - settled[i - 1]));
        }
        expect(settled.length).toBeGreaterThan(CONTEXT_RATE / 2);
        expect(worst).toBeLessThanOrEqual(maxStep * 1.05);
    });

    it('without timestamps an outage raises the target, up to the ceiling', () => {
        const rig = new Rig();
        for (let i = 0; i < blocksFor(500); i++) {
            rig.push(silentBlock());
        }
        rig.renderUntil(200); // plays the newest 120 ms, then runs dry
        for (let i = 0; i < blocksFor(500); i++) {
            rig.push(silentBlock());
        }
        rig.renderUntil(210);
        // Nothing arrived for 200 ms: that outage, plus the margin, becomes the target.
        const raised = rig.playout.stats().targetMs;
        expect(raised).toBeGreaterThanOrEqual(200 + GAP_MARGIN_MS - 5);
        expect(raised).toBeLessThanOrEqual(200 + GAP_MARGIN_MS + 5);

        rig.renderUntil(2000); // runs dry: a 1.8 s outage
        for (let i = 0; i < blocksFor(500); i++) {
            rig.push(silentBlock());
        }
        rig.renderUntil(2010);
        expect(rig.playout.stats().targetMs).toBe(TARGET_CEIL_MS);
    });

    it('backlog above target is trimmed back to target while playing', () => {
        const rig = new Rig();
        for (let i = 0; i < blocksFor(500); i++) {
            rig.push(silentBlock());
        }
        rig.renderUntil(50);
        const trimmedBefore = rig.playout.stats().trimmedMs;
        // Playback stalled elsewhere (hidden tab, output device switch) while audio kept
        // arriving: 3 s lands on top of what is queued.
        for (let i = 0; i < blocksFor(3000); i++) {
            rig.push(silentBlock());
        }
        rig.renderUntil(100);

        const stats = rig.playout.stats();
        expect(rig.playout.depthMs()).toBeLessThanOrEqual(stats.targetMs + TRIM_MARGIN_MS);
        expect(stats.trimmedMs - trimmedBefore).toBeGreaterThan(2900);
        expect(stats.trimmedMs - trimmedBefore).toBeLessThan(3100);
        expect(stats.droppedMs).toBe(0);
        expect(stats.underruns).toBe(0);
    });

    it('start-up backlog plays from the newest target of audio', () => {
        const rig = new Rig();
        for (let i = 0; i < blocksFor(3000); i++) {
            rig.push(silentBlock());
        }
        rig.renderUntil(50);

        const stats = rig.playout.stats();
        expect(rig.playout.depthMs()).toBeLessThanOrEqual(stats.targetMs);
        expect(stats.trimmedMs).toBeGreaterThan(2500);
        expect(stats.underruns).toBe(0);
    });

    it('trimming fades out and back in without a click', () => {
        const rig = new Rig();
        const frequency = 1000;
        let phase = 0;
        const sineBlock = (): Float32Array[] => {
            const samples = new Float32Array(BLOCK_FRAMES);
            for (let i = 0; i < BLOCK_FRAMES; i++) {
                samples[i] = Math.sin(phase);
                phase += (2 * Math.PI * frequency) / STREAM_RATE;
            }
            return [samples, samples.slice()];
        };
        for (let i = 0; i < blocksFor(500); i++) {
            rig.push(sineBlock());
        }
        rig.renderUntil(200, true);
        for (let i = 0; i < blocksFor(2000); i++) {
            rig.push(sineBlock());
        }
        rig.renderUntil(400, true);

        expect(rig.playout.stats().trimmedMs).toBeGreaterThan(1500);
        // Skip the start-up fade. A jump at full volume would be a step of up to 2.
        const settled = rig.output.slice(Math.ceil(CONTEXT_RATE * 0.05));
        const maxStep = (2 * Math.PI * frequency) / CONTEXT_RATE;
        const fadeStep = 1000 / (5 * CONTEXT_RATE);
        let worst = 0;
        for (let i = 1; i < settled.length; i++) {
            worst = Math.max(worst, Math.abs(settled[i] - settled[i - 1]));
        }
        expect(worst).toBeLessThanOrEqual(maxStep * 1.05 + fadeStep);
    });

    it('a late block raises the target before the ring runs dry, up to the ceiling', () => {
        const rig = new Rig();
        for (let i = 0; i < blocksFor(500); i++) {
            rig.push(silentBlock(), 0);
        }
        rig.renderUntil(50);
        rig.push(silentBlock(), 150);
        expect(rig.playout.stats().targetMs).toBe(150 + GAP_MARGIN_MS);
        expect(rig.playout.stats().underruns).toBe(0);

        rig.push(silentBlock(), 900);
        expect(rig.playout.stats().targetMs).toBe(TARGET_CEIL_MS);
    });

    it('the target falls back once late blocks age out', () => {
        const rig = new Rig();
        rig.push(silentBlock(), 200);
        expect(rig.playout.stats().targetMs).toBe(200 + GAP_MARGIN_MS);
        // 31 s of on-time blocks.
        for (let block = 0; block * BLOCK_MS < 31_000; block++) {
            rig.renderUntil(block * BLOCK_MS);
            rig.push(silentBlock(), 0);
        }
        rig.renderUntil(31_100);

        expect(rig.playout.stats().targetMs).toBe(TARGET_FLOOR_MS);
        expect(rig.playout.depthMs()).toBeLessThanOrEqual(TARGET_FLOOR_MS + TRIM_MARGIN_MS);
    });

    it('a pause in the stream leaves the target alone', () => {
        // Windows stops sending while nothing plays. The first block after a pause is fresh,
        // unlike the first block after a stall.
        const rig = new Rig();
        for (let i = 0; i < blocksFor(500); i++) {
            rig.push(silentBlock(), 0);
        }
        rig.renderUntil(60_000);
        for (let i = 0; i < blocksFor(500); i++) {
            rig.push(silentBlock(), 0);
        }
        rig.renderUntil(60_100);

        expect(rig.playout.stats().targetMs).toBe(TARGET_FLOOR_MS);
    });

    it('an implausible lateness is ignored', () => {
        // A wrapped 16-bit server timestamp reads as a delay of days.
        const rig = new Rig();
        rig.push(silentBlock(), 4_294_967_000);
        expect(rig.playout.stats().targetMs).toBe(TARGET_FLOOR_MS);
    });

    it('full ring drops oldest with fade and counts it', () => {
        const rig = new Rig();
        // 5 s arrive before the reader starts: the 4 s ring keeps the newest 4 s.
        for (let i = 0; i < blocksFor(5000); i++) {
            rig.push(silentBlock());
        }
        const dropped = rig.playout.stats().droppedMs;
        expect(dropped).toBeGreaterThan(900);
        expect(dropped).toBeLessThan(1100);
    });
});
