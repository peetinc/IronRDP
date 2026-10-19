//! Embeds the RemoteFX decode Worker's wasm module (`ironrdp-rfx-worker`), built beforehand by
//! `cargo xtask web build`, which passes its path in `IRONRDP_RFX_WORKER_WASM`. Without it the
//! embedded module is empty and RemoteFX decodes on the main thread.

use std::path::PathBuf;
use std::{env, fs};

fn main() {
    println!("cargo:rerun-if-env-changed=IRONRDP_RFX_WORKER_WASM");
    let out_dir = env::var_os("OUT_DIR").expect("cargo sets OUT_DIR for build scripts");
    let embedded = PathBuf::from(out_dir).join("rfx_worker.wasm");
    match env::var_os("IRONRDP_RFX_WORKER_WASM") {
        Some(module) => {
            let module = PathBuf::from(module);
            println!("cargo:rerun-if-changed={}", module.display());
            fs::copy(&module, &embedded)
                .unwrap_or_else(|error| panic!("copy the RFX worker module {}: {error}", module.display()));
        }
        None => fs::write(&embedded, []).expect("write an empty RFX worker module"),
    }
}
