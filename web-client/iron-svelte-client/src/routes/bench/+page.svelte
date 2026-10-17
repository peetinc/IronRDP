<script lang="ts">
    // EGFX replay bench (test rig): replays a capture from /bench/<file> through the real
    // wasm pipeline, flat out, and reports the sustained frame rate and main-thread profile.
    // Usage: /bench?capture=mixed-17-31.bin&h264=1
    import { onMount } from 'svelte';
    import { init, EgfxBench } from '../../../static/iron-remote-desktop-rdp';

    let canvas: HTMLCanvasElement;
    let status = 'loading';
    let result: Record<string, unknown> | null = null;

    type ProfilerTrace = {
        samples: { stackId?: number }[];
        stacks: { frameId: number; parentId?: number }[];
        frames: { name?: string }[];
    };

    // Self time per function, as a share of all samples.
    function summarize(trace: ProfilerTrace) {
        const self = new Map<string, number>();
        let idle = 0;
        for (const sample of trace.samples) {
            if (sample.stackId === undefined) {
                idle++;
                continue;
            }
            const name = trace.frames[trace.stacks[sample.stackId].frameId]?.name || '(anonymous)';
            self.set(name, (self.get(name) ?? 0) + 1);
        }
        const total = trace.samples.length || 1;
        return {
            busyPct: +(100 - (100 * idle) / total).toFixed(1),
            top: [...self.entries()]
                .sort((a, b) => b[1] - a[1])
                .slice(0, 10)
                .map(([name, count]) => `${((100 * count) / total).toFixed(1)}% ${name.replace(/::h[0-9a-f]{16}/, '')}`),
        };
    }

    onMount(async () => {
        const params = new URLSearchParams(location.search);
        const capture = params.get('capture') ?? 'mixed-17-31.bin';
        const h264 = params.get('h264') === '1';
        try {
            await init('warn');
            status = `fetching ${capture}`;
            const bytes = new Uint8Array(await (await fetch(`/bench/${capture}`)).arrayBuffer());
            const bench = await EgfxBench.create(canvas, h264);
            status = `replaying ${capture} (${(bytes.length / 1e6).toFixed(1)} MB)`;

            const ProfilerCtor = (globalThis as { Profiler?: new (options: object) => { stop(): Promise<ProfilerTrace> } })
                .Profiler;
            const profiler = ProfilerCtor ? new ProfilerCtor({ sampleInterval: 10, maxBufferSize: 1_000_000 }) : null;
            const run = (await bench.run(bytes)) as Record<string, unknown>;
            const profile = profiler ? summarize(await profiler.stop()) : null;

            result = { capture, h264, ...run, ...(profile ?? {}) };
            status = 'done';
        } catch (error) {
            result = { capture, error: String(error) };
            status = 'failed';
        }
        (window as unknown as { __benchResult: unknown }).__benchResult = result;
    });
</script>

<p>{status}</p>
{#if result}
    <pre>{JSON.stringify(result, null, 2)}</pre>
{/if}
<canvas bind:this={canvas} style="max-width: 100%;"></canvas>
