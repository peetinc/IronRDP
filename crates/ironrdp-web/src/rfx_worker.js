// RemoteFX Progressive decode Worker (classic script, loaded from a Blob URL).
//
// Runs `ironrdp-rfx-worker`'s wasm module: one progressive decoder per Worker. The main
// thread (`rfx_worker.rs`) posts every progressive input in stream order:
//   { type: 'init', module, shard, shards }                   the compiled WebAssembly.Module,
//                                                             and which tiles this Worker owns
//   { type: 'submit', job, surface, ctx, width, height, data } data: ArrayBuffer, transferred
//   { type: 'control', kind, surface, ctx }
//   { type: 'return', buffer }                                a pixel buffer back for reuse
// and gets one reply per job:
//   { type: 'result', job, error, pixels, pixelsLength, placements, ms }
// `pixels` holds the packed tiles (`pixelsLength` bytes of it), `placements` five i32s per
// placement: left, top, width, height, byte offset. Both are transferred. `error` is 0 or a
// decode error code. A wasm trap ends the Worker; the main thread sees it as an error event.
'use strict';

const POOL_MAX = 8;
let wasm = null;
const queued = [];
const pool = [];

function takeBuffer(length) {
    // An empty result (a job with no tiles of this shard) must not carry off a pooled buffer.
    if (length === 0) {
        return new ArrayBuffer(0);
    }
    for (let i = 0; i < pool.length; i++) {
        if (length <= pool[i].byteLength) {
            return pool.splice(i, 1)[0];
        }
    }
    return new ArrayBuffer(length);
}

function handle(message) {
    switch (message.type) {
        case 'submit': {
            const data = new Uint8Array(message.data);
            const started = performance.now();
            const pointer = wasm.rfx_alloc(data.length);
            new Uint8Array(wasm.memory.buffer, pointer, data.length).set(data);
            const error = wasm.rfx_submit(message.surface, message.ctx, message.width, message.height, pointer, data.length);
            wasm.rfx_free(pointer, data.length);
            const ms = performance.now() - started;
            if (error !== 0) {
                self.postMessage({ type: 'result', job: message.job, error, ms });
                return;
            }
            const pixelsLength = wasm.rfx_pixels_len();
            const pixels = takeBuffer(pixelsLength);
            new Uint8Array(pixels, 0, pixelsLength).set(new Uint8Array(wasm.memory.buffer, wasm.rfx_pixels_ptr(), pixelsLength));
            const placements = new Int32Array(wasm.rfx_placements_len() / 4);
            placements.set(new Int32Array(wasm.memory.buffer, wasm.rfx_placements_ptr(), placements.length));
            self.postMessage(
                { type: 'result', job: message.job, error: 0, pixels, pixelsLength, placements, ms },
                [pixels, placements.buffer],
            );
            return;
        }
        case 'control':
            wasm.rfx_control(message.kind, message.surface, message.ctx);
            return;
        case 'return':
            // An empty result's buffer comes back too: pooled, it would only crowd out real ones.
            if (0 < message.buffer.byteLength && pool.length < POOL_MAX) {
                pool.push(message.buffer);
            }
            return;
    }
}

self.onmessage = (event) => {
    const message = event.data;
    if (message.type === 'init') {
        WebAssembly.instantiate(message.module, {}).then((instance) => {
            wasm = instance.exports;
            wasm.rfx_shard(message.shard ?? 0, message.shards ?? 1);
            for (const early of queued.splice(0)) {
                handle(early);
            }
        }, (error) => {
            // Surface it as an error event, which the main thread treats as a lost Worker.
            setTimeout(() => {
                throw error;
            });
        });
        return;
    }
    if (wasm === null) {
        queued.push(message);
    } else {
        handle(message);
    }
};
