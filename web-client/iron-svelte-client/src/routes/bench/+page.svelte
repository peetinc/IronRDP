<script lang="ts">
    // EGFX replay bench (test rig): replays a capture from /bench/<file> through the real
    // wasm pipeline, flat out, and reports the sustained frame rate and main-thread profile.
    // Usage: /bench?capture=mixed-17-31.bin&h264=1&gpu=1
    // verify=1 replays on the CPU and then the GPU and compares sampled pixels every 100 frames.
    // lose=1 (with gpu=1) drops the GPU context mid-run.
    import { onMount } from 'svelte';
    import { init, EgfxBench } from '../../../static/iron-remote-desktop-rdp';

    let canvas: HTMLCanvasElement;
    let verifyCanvas: HTMLCanvasElement;
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
        const gpu = params.get('gpu') === '1';
        const verify = params.get('verify') === '1';
        try {
            await init('warn');
            status = `fetching ${capture}`;
            const bytes = new Uint8Array(await (await fetch(`/bench/${capture}`)).arrayBuffer());
            if (verify) {
                status = `verifying ${capture}: CPU pass`;
                const cpu = (await (await EgfxBench.create(verifyCanvas, h264, false)).run(bytes, 100)) as {
                    samples: Uint8Array[];
                };
                status = `verifying ${capture}: GPU pass`;
                const gpuRun = (await (await EgfxBench.create(canvas, h264, true)).run(bytes, 100)) as {
                    samples: Uint8Array[];
                };
                // Per checkpoint: sampled pixels that differ at all, and by more than 2 per channel.
                const checkpoints = cpu.samples.map((a, index) => {
                    const b = gpuRun.samples[index];
                    if (!b || a.length !== b.length) return { index, sizeMismatch: [a.length, b?.length ?? 0] };
                    let exact = 0;
                    let loose = 0;
                    let maxDiff = 0;
                    for (let i = 0; i < a.length; i += 3) {
                        const d = Math.max(Math.abs(a[i] - b[i]), Math.abs(a[i + 1] - b[i + 1]), Math.abs(a[i + 2] - b[i + 2]));
                        if (d > 0) exact++;
                        if (d > 2) loose++;
                        maxDiff = Math.max(maxDiff, d);
                    }
                    return { index, pixels: a.length / 3, exact, loose, maxDiff };
                });
                result = { capture, h264, verify: true, cpuCheckpoints: cpu.samples.length, gpuCheckpoints: gpuRun.samples.length, checkpoints };
                status = 'done';
                (window as unknown as { __benchResult: unknown }).__benchResult = result;
                return;
            }

            const bench = await EgfxBench.create(canvas, h264, gpu);
            status = `replaying ${capture} (${(bytes.length / 1e6).toFixed(1)} MB)`;
            // lose=1 drops the GPU context 2 s in (it fires while the pipeline awaits a
            // decoder), to check the run ends with "GPU context lost".
            if (params.get('lose') === '1') {
                setTimeout(() => canvas.getContext('webgl2')?.getExtension('WEBGL_lose_context')?.loseContext(), 2000);
            }

            const ProfilerCtor = (globalThis as { Profiler?: new (options: object) => { stop(): Promise<ProfilerTrace> } })
                .Profiler;
            const profiler = ProfilerCtor ? new ProfilerCtor({ sampleInterval: 10, maxBufferSize: 1_000_000 }) : null;
            const run = (await bench.run(bytes, 0)) as Record<string, unknown>;
            delete run.samples;
            const profile = profiler ? summarize(await profiler.stop()) : null;

            result = { capture, h264, gpu, ...run, ...(profile ?? {}) };
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
<canvas bind:this={verifyCanvas} style="max-width: 100%;"></canvas>
