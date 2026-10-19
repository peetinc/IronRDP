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

function silentBlock(): Float32Array[] {
    return [new Float32Array(BLOCK_FRAMES), new Float32Array(BLOCK_FRAMES)];
}

function blocksFor(ms: number): number {
    return Math.ceil(ms / BLOCK_MS);
}

describe('Playout', () => {
    it('burst pattern from the log plays without gaps after adaptation', () => {
        // The server produces one block every 23.2 ms. The client receives them in clumps:
        // every block produced so far, delivered at once, separated by these gaps (ms) as in
        // the captured WAN log: short gaps, then a 650 ms stall.
        const gaps = [80, 100, 650, 90, 120, 300, 650, 100, 80, 650, 110, 90, 650, 100, 650, 100];
        const rig = new Rig();
        let produced = 0;
        let delivered = 0;
        let now = 0;
        let underrunsAfterWarmup = -1;
        gaps.forEach((gap, index) => {
            now += gap;
            rig.renderUntil(now);
            produced = Math.floor(now / BLOCK_MS);
            for (; delivered < produced; delivered++) {
                // How late the block arrives: it was produced at the end of its own span.
                rig.push(silentBlock(), now - (delivered + 1) * BLOCK_MS);
            }
            if (index === 5) {
                // Warmup: the target adapts to the first late burst.
                underrunsAfterWarmup = rig.playout.stats().underruns;
            }
        });

        const stats = rig.playout.stats();
        expect(stats.droppedMs).toBe(0);
        expect(stats.underruns).toBe(underrunsAfterWarmup);
        expect(stats.targetMs).toBeLessThanOrEqual(2000);
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

    it('underrun raises target to the outage plus margin, capped at ceiling', () => {
        const rig = new Rig();
        for (let i = 0; i < blocksFor(500); i++) {
            rig.push(silentBlock());
        }
        rig.renderUntil(600); // plays the 500 ms, then runs dry
        rig.renderUntil(1600); // ~1 s of silence while buffering
        for (let i = 0; i < blocksFor(2500); i++) {
            rig.push(silentBlock());
        }
        rig.renderUntil(1700);
        // Nothing arrived for 1.6 s: that outage, plus the margin, becomes the target.
        const raised = rig.playout.stats().targetMs;
        expect(raised).toBeGreaterThanOrEqual(1650);
        expect(raised).toBeLessThanOrEqual(1750);

        rig.renderUntil(4300); // plays out, then runs dry again
        rig.renderUntil(7500); // a 3 s outage
        for (let i = 0; i < blocksFor(2500); i++) {
            rig.push(silentBlock());
        }
        rig.renderUntil(7600);
        expect(rig.playout.stats().targetMs).toBe(2000);
    });

    it('backlog above target is trimmed back to target while playing', () => {
        const rig = new Rig();
        for (let i = 0; i < blocksFor(500); i++) {
            rig.push(silentBlock());
        }
        rig.renderUntil(100);
        // Playback stalled elsewhere (hidden tab, output device switch) while audio kept
        // arriving: 3 s lands on top of what is queued.
        for (let i = 0; i < blocksFor(3000); i++) {
            rig.push(silentBlock());
        }
        rig.renderUntil(150);

        const stats = rig.playout.stats();
        expect(rig.playout.depthMs()).toBeLessThanOrEqual(stats.targetMs + 50);
        expect(stats.trimmedMs).toBeGreaterThan(2900);
        expect(stats.trimmedMs).toBeLessThan(3200);
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

    it('a late block raises the target before the ring runs dry', () => {
        const rig = new Rig();
        for (let i = 0; i < blocksFor(500); i++) {
            rig.push(silentBlock(), 0);
        }
        rig.renderUntil(100);
        rig.push(silentBlock(), 900);

        const stats = rig.playout.stats();
        expect(stats.targetMs).toBe(1000);
        expect(stats.underruns).toBe(0);
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

        expect(rig.playout.stats().targetMs).toBe(400);
    });

    it('an implausible lateness is ignored', () => {
        // A wrapped 16-bit server timestamp reads as a delay of days.
        const rig = new Rig();
        rig.push(silentBlock(), 4_294_967_000);
        expect(rig.playout.stats().targetMs).toBe(400);
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
