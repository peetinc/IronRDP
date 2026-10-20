//! RemoteFX Progressive decoding in a Web Worker.
//!
//! [`WorkerProgressiveBackend`] runs `ironrdp-rfx-worker`'s wasm module in a classic Worker
//! (`rfx_worker.js`, from a Blob URL) and implements [`ProgressiveBackend`] over it. The
//! Worker decodes each `WireToSurface2` bitmap, crops and packs its tiles, and transfers the
//! pixels back in its own buffer. The buffer goes into a thread-local store under a handle;
//! the GPU backend uploads it straight from there ([`take_tiles`]) and hands it back to the
//! Worker for reuse ([`return_buffer`]). Tile pixels never enter this module's memory.
//!
//! Like `h264.rs`, the backend is a `Send` handle to thread-local JS state. See
//! `docs/plans/2026-10-19-web-rfx-workers-design.md`.

use core::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use ironrdp::pdu::geometry::ExclusiveRectangle;
use ironrdp_egfx::decode::{DecodedTiles, ProgressiveBackend, ProgressiveControl, ProgressiveJobError};
use js_sys::{Array, ArrayBuffer, Int32Array, Object, Reflect, Uint8Array};
use tracing::{debug, info, warn};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::*;

/// The Worker's wasm module, embedded by `build.rs`; empty when the build did not provide it.
const WORKER_MODULE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/rfx_worker.wasm"));
const WORKER_SCRIPT: &str = include_str!("rfx_worker.js");

/// Jobs a Worker may hold at once. More than H.264's 4: a frame often carries several small
/// progressive commands.
const MAX_IN_FLIGHT: usize = 8;

/// Every job's outcome once the Worker is lost: its tile state went with it.
const WORKER_FAILED: &str = "RemoteFX worker failed";

/// One packed tile region: where it goes on the surface and where its pixels start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Placement {
    pub(crate) rect: ExclusiveRectangle,
    pub(crate) offset: u32,
}

/// The placements in a Worker's reply: five `i32`s each (left, top, width, height, byte
/// offset). A placement whose values cannot describe a surface region is skipped.
pub(crate) fn placements(raw: &[i32]) -> Vec<Placement> {
    raw.chunks_exact(5)
        .filter_map(|values| {
            let [left, top, width, height] = [values[0], values[1], values[2], values[3]].map(u16::try_from);
            let (Ok(left), Ok(top), Ok(width), Ok(height), Ok(offset)) =
                (left, top, width, height, u32::try_from(values[4]))
            else {
                return None;
            };
            Some(Placement {
                rect: ExclusiveRectangle {
                    left,
                    top,
                    right: left.checked_add(width)?,
                    bottom: top.checked_add(height)?,
                },
                offset,
            })
        })
        .collect()
}

/// Job outcomes a Worker has reported, until polled: a handle into the tile store, or an
/// error reason.
#[derive(Default)]
pub(crate) struct Results {
    outcomes: BTreeMap<u32, Result<u64, &'static str>>,
    failed: bool,
}

impl Results {
    pub(crate) fn complete(&mut self, job: u32, outcome: Result<u64, &'static str>) {
        self.outcomes.insert(job, outcome);
    }

    /// The Worker is gone: every job without a result fails.
    pub(crate) fn fail(&mut self) {
        self.failed = true;
    }

    pub(crate) fn poll(&mut self, job: u32) -> Option<Result<u64, &'static str>> {
        self.outcomes
            .remove(&job)
            .or_else(|| self.failed.then_some(Err(WORKER_FAILED)))
    }
}

/// A completed job's pixels, as the Worker transferred them.
pub(crate) struct ExternalTiles {
    pub(crate) pixels: ArrayBuffer,
    /// Bytes of `pixels` in use; the buffer may be a larger one from the Worker's pool.
    pub(crate) length: u32,
    pub(crate) placements: Vec<Placement>,
    /// The slot whose Worker the buffer goes back to.
    owner: u32,
}

/// Decoded jobs not yet drawn, by handle.
#[derive(Default)]
struct TileStore {
    tiles: BTreeMap<u64, ExternalTiles>,
    next: u64,
}

thread_local! {
    static SLOTS: RefCell<BTreeMap<u32, Slot>> = const { RefCell::new(BTreeMap::new()) };
    static NEXT_SLOT: Cell<u32> = const { Cell::new(0) };
    static TILES: RefCell<TileStore> = RefCell::new(TileStore::default());
    // DIAG
    static DIAG: Cell<(f64, f64, f64, f64)> = const { Cell::new((0.0, 0.0, 0.0, 0.0)) };
    // DIAG: (submitted, completed, last completed job), and the job being waited on since when.
    static DIAG_JOBS: Cell<(u32, u32, u32)> = const { Cell::new((0, 0, 0)) };
    static DIAG_WAIT: Cell<(u32, f64, bool)> = const { Cell::new((u32::MAX, 0.0, false)) };
    /// The compiled Worker module, compiled once per page; `None` once compiling failed.
    static MODULE: RefCell<Option<Option<js_sys::WebAssembly::Module>>> = const { RefCell::new(None) };
}

// DIAG: tiles held, their bytes, outcomes held, (worker wasm bytes, pool len, pool bytes, last pixels len).
pub(crate) fn diag() -> String {
    let (tiles, tile_bytes) = TILES.with_borrow(|store| (store.tiles.len(), store.tiles.values().map(|t| f64::from(t.pixels.byte_length())).sum::<f64>()));
    let outcomes: usize = SLOTS.with_borrow(|slots| slots.values().map(|slot| slot.results.borrow().outcomes.len()).sum());
    let (wasm, pool, pool_bytes, last) = DIAG.get();
    format!("tiles={tiles} tileMB={:.1} outcomes={outcomes} workerWasmMB={:.1} pool={pool} poolMB={:.1} lastPixels={last}", tile_bytes / 1e6, wasm / 1e6, pool_bytes / 1e6)
}

/// Takes a completed job's pixels for drawing; hand the buffer back with [`return_buffer`].
pub(crate) fn take_tiles(handle: u64) -> Option<ExternalTiles> {
    TILES.with_borrow_mut(|store| store.tiles.remove(&handle))
}

/// Reads a completed job's pixels without taking them (the bench's verification).
pub(crate) fn with_tiles<R>(handle: u64, f: impl FnOnce(&ExternalTiles) -> R) -> Option<R> {
    TILES.with_borrow(|store| store.tiles.get(&handle).map(f))
}

/// Gives a drawn job's pixel buffer back to its Worker for reuse.
pub(crate) fn return_buffer(tiles: ExternalTiles) {
    SLOTS.with_borrow(|slots| {
        if let Some(slot) = slots.get(&tiles.owner) {
            let message = Object::new();
            let sent = set(&message, "type", &JsValue::from_str("return"))
                .and_then(|()| set(&message, "buffer", &tiles.pixels))
                .and_then(|()| {
                    slot.worker
                        .post_message_with_transfer(&message, &Array::of1(&tiles.pixels))
                });
            if let Err(error) = sent {
                debug!(?error, "Failed to return a RemoteFX pixel buffer to its worker");
            }
        }
    });
}

fn set(target: &Object, key: &str, value: &JsValue) -> Result<(), JsValue> {
    Reflect::set(target, &JsValue::from_str(key), value).map(drop)
}

fn compiled_module() -> Option<js_sys::WebAssembly::Module> {
    MODULE.with_borrow_mut(|module| {
        module
            .get_or_insert_with(|| {
                // Synchronous compilation is fine for a module this small (about 170 KB).
                js_sys::WebAssembly::Module::new(&Uint8Array::from(WORKER_MODULE))
                    .inspect_err(|error| warn!(?error, "Failed to compile the RemoteFX worker module"))
                    .ok()
            })
            .clone()
    })
}

/// A Worker and the callbacks that feed its replies into [`Results`].
struct Slot {
    worker: web_sys::Worker,
    url: String,
    results: Rc<RefCell<Results>>,
    _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _on_error: Closure<dyn FnMut(web_sys::ErrorEvent)>,
}

/// [`ProgressiveBackend`] decoding in one Web Worker; a handle to its thread-local slot.
pub(crate) struct WorkerProgressiveBackend {
    slot: u32,
}

impl WorkerProgressiveBackend {
    /// Starts the Worker. `wake` is called once per completed job, and when the Worker is
    /// lost. `None` when this build has no Worker module or the browser cannot run it: the
    /// session then decodes in-thread.
    pub(crate) fn new(wake: impl Fn() + 'static) -> Option<Self> {
        if WORKER_MODULE.is_empty() {
            debug!("No RemoteFX worker module in this build");
            return None;
        }
        let module = compiled_module()?;
        let slot = NEXT_SLOT.with(|next| {
            let slot = next.get();
            next.set(slot.wrapping_add(1));
            slot
        });
        match Slot::start(slot, &module, Rc::new(wake)) {
            Ok(started) => {
                SLOTS.with_borrow_mut(|slots| slots.insert(slot, started));
                info!("Decoding RemoteFX in a Web Worker");
                Some(Self { slot })
            }
            Err(error) => {
                warn!(
                    ?error,
                    "Failed to start the RemoteFX worker; decoding on the main thread"
                );
                None
            }
        }
    }

    fn post(&self, message: &Object, transfer: Option<&Array>) {
        SLOTS.with_borrow(|slots| {
            let Some(slot) = slots.get(&self.slot) else {
                return;
            };
            let sent = match transfer {
                Some(transfer) => slot.worker.post_message_with_transfer(message, transfer),
                None => slot.worker.post_message(message),
            };
            if let Err(error) = sent {
                warn!(?error, "Failed to post to the RemoteFX worker");
                slot.results.borrow_mut().fail();
            }
        });
    }
}

impl Slot {
    fn start(owner: u32, module: &js_sys::WebAssembly::Module, wake: Rc<dyn Fn()>) -> Result<Self, JsValue> {
        let parts = Array::of1(&JsValue::from_str(WORKER_SCRIPT));
        let options = web_sys::BlobPropertyBag::new();
        options.set_type("text/javascript");
        let blob = web_sys::Blob::new_with_str_sequence_and_options(&parts, &options)?;
        let url = web_sys::Url::create_object_url_with_blob(&blob)?;
        let worker = match web_sys::Worker::new(&url) {
            Ok(worker) => worker,
            Err(error) => {
                let _ = web_sys::Url::revoke_object_url(&url);
                return Err(error);
            }
        };

        let results = Rc::new(RefCell::new(Results::default()));
        let on_message = {
            let (results, wake) = (Rc::clone(&results), Rc::clone(&wake));
            Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
                if let Some((job, outcome)) = read_result(owner, &event.data()) {
                    results.borrow_mut().complete(job, outcome);
                    wake();
                }
            })
        };
        let on_error = {
            let (results, wake) = (Rc::clone(&results), wake);
            Closure::<dyn FnMut(web_sys::ErrorEvent)>::new(move |event: web_sys::ErrorEvent| {
                warn!(message = %event.message(), "RemoteFX worker failed");
                results.borrow_mut().fail();
                wake();
            })
        };
        worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        worker.set_onerror(Some(on_error.as_ref().unchecked_ref()));

        let init = Object::new();
        set(&init, "type", &JsValue::from_str("init"))?;
        set(&init, "module", module)?;
        worker.post_message(&init)?;

        Ok(Self {
            worker,
            url,
            results,
            _on_message: on_message,
            _on_error: on_error,
        })
    }
}

/// Reads a `result` message: the job and its outcome, its pixels stored under a new handle.
fn read_result(owner: u32, data: &JsValue) -> Option<(u32, Result<u64, &'static str>)> {
    let field = |key: &str| Reflect::get(data, &JsValue::from_str(key)).ok();
    let number = |key: &str| field(key).and_then(|value| value.as_f64());
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "job ids, codes and lengths the Worker echoes back fit in u32"
    )]
    let as_u32 = |value: f64| value as u32;
    let job = as_u32(number("job")?);
    // DIAG
    DIAG_JOBS.with(|c| { let (s, d, _) = c.get(); c.set((s, d + 1, job)); });
    DIAG.set((number("wasmBytes").unwrap_or(-1.0), number("pool").unwrap_or(-1.0), number("poolBytes").unwrap_or(-1.0), number("pixelsLength").unwrap_or(-1.0)));
    let error = as_u32(number("error").unwrap_or(0.0));
    if error != 0 {
        return Some((job, Err(ironrdp_rfx_worker::error_reason(error))));
    }
    let pixels = field("pixels")?.dyn_into::<ArrayBuffer>().ok()?;
    let length = as_u32(number("pixelsLength")?);
    let placements = placements(&field("placements")?.dyn_into::<Int32Array>().ok()?.to_vec());
    let handle = TILES.with_borrow_mut(|store| {
        let handle = store.next;
        store.next = store.next.wrapping_add(1);
        store.tiles.insert(
            handle,
            ExternalTiles {
                pixels,
                length,
                placements,
                owner,
            },
        );
        handle
    });
    Some((job, Ok(handle)))
}

impl ProgressiveBackend for WorkerProgressiveBackend {
    fn max_in_flight(&self) -> usize {
        MAX_IN_FLIGHT
    }

    fn submit(&mut self, job: u32, surface_id: u16, codec_context_id: u32, width: u16, height: u16, data: &[u8]) {
        let bytes = Uint8Array::from(data);
        let message = Object::new();
        let built = set(&message, "type", &JsValue::from_str("submit"))
            .and_then(|()| set(&message, "job", &JsValue::from(job)))
            .and_then(|()| set(&message, "surface", &JsValue::from(surface_id)))
            .and_then(|()| set(&message, "ctx", &JsValue::from(codec_context_id)))
            .and_then(|()| set(&message, "width", &JsValue::from(width)))
            .and_then(|()| set(&message, "height", &JsValue::from(height)))
            .and_then(|()| set(&message, "data", &bytes.buffer()));
        if built.is_ok() {
            // DIAG
            DIAG_JOBS.with(|c| { let (s, d, l) = c.get(); c.set((s + 1, d, l)); });
            self.post(&message, Some(&Array::of1(&bytes.buffer())));
        }
    }

    fn control(&mut self, control: ProgressiveControl) {
        let (kind, surface_id, codec_context_id) = match control {
            ProgressiveControl::StartFrame => (ironrdp_rfx_worker::CONTROL_START_FRAME, 0, 0),
            ProgressiveControl::EndFrame => (ironrdp_rfx_worker::CONTROL_END_FRAME, 0, 0),
            ProgressiveControl::DeleteContext {
                surface_id,
                codec_context_id,
            } => (ironrdp_rfx_worker::CONTROL_DELETE_CONTEXT, surface_id, codec_context_id),
            ProgressiveControl::DeleteSurface { surface_id } => {
                (ironrdp_rfx_worker::CONTROL_DELETE_SURFACE, surface_id, 0)
            }
        };
        let message = Object::new();
        let built = set(&message, "type", &JsValue::from_str("control"))
            .and_then(|()| set(&message, "kind", &JsValue::from(kind)))
            .and_then(|()| set(&message, "surface", &JsValue::from(surface_id)))
            .and_then(|()| set(&message, "ctx", &JsValue::from(codec_context_id)));
        if built.is_ok() {
            self.post(&message, None);
        }
    }

    fn poll(&mut self, job: u32) -> Option<Result<DecodedTiles, ProgressiveJobError>> {
        let outcome = SLOTS.with_borrow(|slots| match slots.get(&self.slot) {
            Some(slot) => slot.results.borrow_mut().poll(job),
            None => Some(Err(WORKER_FAILED)),
        });
        // DIAG: a job waited on for over 2 s.
        let now = js_sys::Date::now();
        let Some(outcome) = outcome else {
            let (waiting, since, reported) = DIAG_WAIT.get();
            if waiting != job {
                DIAG_WAIT.set((job, now, false));
            } else if !reported && 2000.0 < now - since {
                DIAG_WAIT.set((job, since, true));
                let (submitted, completed, last) = DIAG_JOBS.get();
                let held: Vec<u32> = SLOTS.with_borrow(|slots| slots.get(&self.slot).map(|slot| slot.results.borrow().outcomes.keys().copied().collect()).unwrap_or_default());
                warn!(job, submitted, completed, last_completed = last, ?held, waited_ms = now - since, diag = %diag(), "DIAG RemoteFX job not back");
            }
            return None;
        };
        if DIAG_WAIT.get().2 && DIAG_WAIT.get().0 == job {
            warn!(job, waited_ms = now - DIAG_WAIT.get().1, "DIAG RemoteFX job finally back");
        }
        Some(
            outcome
                .map(|handle| DecodedTiles {
                    external: Some(handle),
                    tiles: Vec::new(),
                })
                .map_err(|reason| ProgressiveJobError { reason }),
        )
    }
}

impl Drop for WorkerProgressiveBackend {
    fn drop(&mut self) {
        if let Some(slot) = SLOTS.with_borrow_mut(|slots| slots.remove(&self.slot)) {
            slot.worker.set_onmessage(None);
            slot.worker.set_onerror(None);
            slot.worker.terminate();
            let _ = web_sys::Url::revoke_object_url(&slot.url);
        }
        TILES.with_borrow_mut(|store| store.tiles.retain(|_, tiles| tiles.owner != self.slot));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placements_read_five_i32s_each() {
        assert_eq!(
            placements(&[64, 0, 36, 64, 0, 0, 64, 64, 8, 9216, 1]),
            [
                Placement {
                    rect: ExclusiveRectangle {
                        left: 64,
                        top: 0,
                        right: 100,
                        bottom: 64
                    },
                    offset: 0
                },
                Placement {
                    rect: ExclusiveRectangle {
                        left: 0,
                        top: 64,
                        right: 64,
                        bottom: 72
                    },
                    offset: 9216
                },
            ],
            "a trailing partial placement is ignored"
        );
    }

    #[test]
    fn placements_skip_values_outside_a_surface() {
        assert!(placements(&[-1, 0, 4, 4, 0]).is_empty());
        assert!(placements(&[0, 0, 70_000, 4, 0]).is_empty());
        assert!(placements(&[0, 0, 4, 4, -16]).is_empty());
    }

    #[test]
    fn results_are_polled_by_job() {
        let mut results = Results::default();
        results.complete(2, Ok(20));
        assert_eq!(results.poll(1), None, "job 1 is still decoding");
        results.complete(1, Err("rfx progressive decode failed: RLGR"));
        assert_eq!(results.poll(1), Some(Err("rfx progressive decode failed: RLGR")));
        assert_eq!(results.poll(2), Some(Ok(20)));
        assert_eq!(results.poll(2), None, "each result is handed out once");
    }

    #[test]
    fn a_failed_worker_fails_every_job_without_a_result() {
        let mut results = Results::default();
        results.complete(1, Ok(10));
        results.fail();
        assert_eq!(results.poll(1), Some(Ok(10)), "finished work still applies");
        assert_eq!(results.poll(2), Some(Err(WORKER_FAILED)));
        assert_eq!(results.poll(3), Some(Err(WORKER_FAILED)));
    }
}
