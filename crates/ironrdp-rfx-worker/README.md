# ironrdp-rfx-worker

RemoteFX Progressive ([MS-RDPEGFX] 2.2.4.2) decoding built as a small, standalone WebAssembly
module with a plain C-style ABI (no wasm-bindgen), for the web client's decode Workers.

Each Worker owns one decoder. The main thread posts it every progressive input in stream order;
the Worker decodes, crops each tile to its update rectangles and packs the pixels tightly, so the
main thread can upload them to the GPU without copying them into its own memory.

The pure parts (`pack_tiles`, `WorkerDecoder`) are an `rlib` too, tested natively.

[MS-RDPEGFX]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/
