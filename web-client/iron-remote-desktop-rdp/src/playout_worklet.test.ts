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

    push(channels: Float32Array[]): void {
        this.playout.push(STREAM_RATE, channels);
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
                rig.push(silentBlock());
            }
            if (index === 5) {
                // Warmup: the first stall may underrun once while the target adapts.
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
