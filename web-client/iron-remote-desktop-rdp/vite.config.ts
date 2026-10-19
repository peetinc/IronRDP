/// <reference types="vitest" />
import { defineConfig } from 'vite';
import topLevelAwait from 'vite-plugin-top-level-await';
import dtsPlugin from 'vite-plugin-dts';
import { readFileSync } from 'node:fs';

// https://vitejs.dev/config/
export default defineConfig({
    test: {
        environment: 'jsdom',
    },
    build: {
        lib: {
            entry: './src/main.ts',
            name: 'IronRemoteDesktopRdp',
            formats: ['es'],
        },
    },
    server: {
        fs: {
            strict: false,
        },
    },
    plugins: [
        topLevelAwait(),
        {
            // Ship the RDPSND playout worklet next to the bundle, so a host page with a
            // strict CSP can serve it from its own origin (see `audioWorkletUrl`).
            name: 'playout-worklet',
            generateBundle() {
                this.emitFile({
                    type: 'asset',
                    fileName: 'playout_worklet.js',
                    source: readFileSync('../../crates/ironrdp-web/src/audio/playout_worklet.js', 'utf8'),
                });
            },
        },
        dtsPlugin({
            rollupTypes: true,
        }),
    ],
});
