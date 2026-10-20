//! RemoteFX Progressive decoding in Web Workers.
//!
//! [`WorkerProgressiveBackend`] runs `ironrdp-rfx-worker`'s wasm module in up to four classic
//! Workers (`rfx_worker.js`, from a Blob URL), each one shard of the decoder
//! (`ProgressiveDecoder::sharded`), and implements [`ProgressiveBackend`] over them. Every
//! Worker gets every progressive input in stream order; each decodes, crops and packs only
//! its own tiles, and transfers the pixels back in its own buffer. A job is done when every
//! Worker has answered. Its buffers go into a thread-local store under one handle; the GPU
//! backend uploads them straight from there ([`take_tiles`]) and hands each back to its
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

/// Workers for `navigator.hardwareConcurrency` cores: two left for the main thread and the
/// rest of the browser, at most four (the design's measured target), at least one.
pub(crate) fn worker_count(hardware_concurrency: Option<f64>) -> usize {
    match hardware_concurrency {
        Some(cores) if 6.0 <= cores => 4,
        Some(cores) if 5.0 <= cores => 3,
        Some(cores) if 4.0 <= cores => 2,
        _ => 1,
    }
}

/// What one Worker's reply did to its job ([`JobResults::complete`]).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Reply<T> {
    /// Other Workers have not answered yet.
    Pending,
    /// Every Worker has answered: the job can be taken.
    Complete,
    /// Not kept: the Workers have failed. Its buffer goes back.
    Dropped(Result<T, &'static str>),
}

/// A taken job ([`JobResults::take`]): its parts, or why it failed and the parts that did not.
pub(crate) type TakenJob<T> = Result<Vec<T>, (&'static str, Vec<T>)>;

/// The Workers' answers to each job, until taken: one part per shard.
pub(crate) struct JobResults<T> {
    shards: usize,
    jobs: BTreeMap<u32, Vec<Option<Result<T, &'static str>>>>,
    failed: bool,
}

impl<T> JobResults<T> {
    pub(crate) fn new(shards: usize) -> Self {
        Self {
            shards,
            jobs: BTreeMap::new(),
            failed: false,
        }
    }

    /// Records shard `shard`'s answer to `job`.
    pub(crate) fn complete(&mut self, job: u32, shard: usize, outcome: Result<T, &'static str>) -> Reply<T> {
        if self.failed || self.shards <= shard {
            return Reply::Dropped(outcome);
        }
        let shards = self.shards;
        let parts = self
            .jobs
            .entry(job)
            .or_insert_with(|| core::iter::repeat_with(|| None).take(shards).collect());
        parts[shard] = Some(outcome);
        if parts.iter().all(Option::is_some) {
            Reply::Complete
        } else {
            Reply::Pending
        }
    }

    /// A Worker is gone, and its tile state with it: every job not answered by all fails.
    pub(crate) fn fail(&mut self) {
        self.failed = true;
    }

    /// `job`'s parts in shard order, once every Worker has answered or the Workers failed.
    /// On error: the first error by shard order (or [`WORKER_FAILED`]) and the parts that
    /// succeeded, for their buffers to go back.
    pub(crate) fn take(&mut self, job: u32) -> Option<TakenJob<T>> {
        let complete = self
            .jobs
            .get(&job)
            .is_some_and(|parts| parts.iter().all(Option::is_some));
        if !complete && !self.failed {
            return None;
        }
        let mut error = (!complete).then_some(WORKER_FAILED);
        let mut parts = Vec::new();
        for part in self.jobs.remove(&job).unwrap_or_default().into_iter().flatten() {
            match part {
                Ok(part) => parts.push(part),
                Err(reason) => {
                    error.get_or_insert(reason);
                }
            }
        }
        Some(match error {
            None => Ok(parts),
            Some(reason) => Err((reason, parts)),
        })
    }
}

/// One Worker's part of a completed job: its pixels, as it transferred them.
pub(crate) struct ExternalTiles {
    pub(crate) pixels: ArrayBuffer,
    /// Bytes of `pixels` in use; the buffer may be a larger one from the Worker's pool.
    pub(crate) length: u32,
    pub(crate) placements: Vec<Placement>,
    /// The group, and the Worker in it, the buffer goes back to.
    group: u32,
    shard: usize,
}

/// Decoded jobs not yet drawn, by handle: one part per Worker.
#[derive(Default)]
struct TileStore {
    tiles: BTreeMap<u64, Vec<ExternalTiles>>,
    next: u64,
}

thread_local! {
    static GROUPS: RefCell<BTreeMap<u32, Group>> = const { RefCell::new(BTreeMap::new()) };
    static NEXT_GROUP: Cell<u32> = const { Cell::new(0) };
    static TILES: RefCell<TileStore> = RefCell::new(TileStore::default());
    /// The compiled Worker module, compiled once per page; `None` once compiling failed.
    static MODULE: RefCell<Option<Option<js_sys::WebAssembly::Module>>> = const { RefCell::new(None) };
    /// Milliseconds each Worker of the latest group has spent decoding, and the jobs it was
    /// given (the bench's measures).
    static BUSY_MS: RefCell<Vec<f64>> = const { RefCell::new(Vec::new()) };
    static JOBS: Cell<u32> = const { Cell::new(0) };
}

/// Takes a completed job's parts for drawing; hand each buffer back with [`return_buffer`].
pub(crate) fn take_tiles(handle: u64) -> Option<Vec<ExternalTiles>> {
    TILES.with_borrow_mut(|store| store.tiles.remove(&handle))
}

/// Reads a completed job's parts without taking them (the bench's verification).
pub(crate) fn with_tiles<R>(handle: u64, f: impl FnOnce(&[ExternalTiles]) -> R) -> Option<R> {
    TILES.with_borrow(|store| store.tiles.get(&handle).map(|parts| f(parts)))
}

/// Milliseconds each Worker of the latest backend has spent decoding, and the jobs it has
/// been given (each Worker gets every job).
pub(crate) fn busy_ms() -> (Vec<f64>, u32) {
    (BUSY_MS.with_borrow(Clone::clone), JOBS.get())
}

/// Gives a part's pixel buffer back to its Worker for reuse.
pub(crate) fn return_buffer(tiles: ExternalTiles) {
    GROUPS.with_borrow(|groups| {
        if let Some(slot) = groups
            .get(&tiles.group)
            .and_then(|group| group.workers.get(tiles.shard))
        {
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

/// One backend's Workers, one shard each, and their answers.
struct Group {
    workers: Vec<Slot>,
    results: Rc<RefCell<JobResults<ExternalTiles>>>,
    wake: Rc<dyn Fn()>,
}

/// A Worker and the callbacks that feed its replies into its group's [`JobResults`].
struct Slot {
    worker: web_sys::Worker,
    url: String,
    _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _on_error: Closure<dyn FnMut(web_sys::ErrorEvent)>,
}

/// [`ProgressiveBackend`] decoding in Web Workers; a handle to its thread-local group.
pub(crate) struct WorkerProgressiveBackend {
    group: u32,
}

impl WorkerProgressiveBackend {
    /// Starts `count` Workers, one shard each. `wake` is called once per completed job, and
    /// when a Worker is lost. `None` when this build has no Worker module or the browser
    /// cannot start a Worker: the session then decodes in-thread. A module that fails to
    /// instantiate inside a Worker reports later, as a lost Worker.
    pub(crate) fn new(count: usize, wake: impl Fn() + 'static) -> Option<Self> {
        if WORKER_MODULE.is_empty() {
            debug!("No RemoteFX worker module in this build");
            return None;
        }
        let module = compiled_module()?;
        let count = count.clamp(1, 4);
        let group = NEXT_GROUP.with(|next| {
            let group = next.get();
            next.set(group.wrapping_add(1));
            group
        });
        let results = Rc::new(RefCell::new(JobResults::new(count)));
        let wake: Rc<dyn Fn()> = Rc::new(wake);
        BUSY_MS.set(vec![0.0; count]);
        JOBS.set(0);
        let mut workers = Vec::with_capacity(count);
        for shard in 0..count {
            match Slot::start(group, shard, count, &module, &results, &wake) {
                Ok(slot) => workers.push(slot),
                Err(error) => {
                    warn!(
                        ?error,
                        "Failed to start the RemoteFX workers; decoding on the main thread"
                    );
                    workers.iter().for_each(Slot::stop);
                    return None;
                }
            }
        }
        GROUPS.with_borrow_mut(|groups| groups.insert(group, Group { workers, results, wake }));
        info!(workers = count, "Decoding RemoteFX in Web Workers");
        Some(Self { group })
    }

    /// Posts a message built by `message` to every Worker. If one cannot take it, the jobs
    /// fail and the session is woken to poll them, as for a lost Worker.
    fn post_all(&self, message: impl Fn() -> Option<(Object, Option<Array>)>) {
        let wake = GROUPS.with_borrow(|groups| {
            let group = groups.get(&self.group)?;
            let mut failed = false;
            for slot in &group.workers {
                let Some((message, transfer)) = message() else {
                    continue;
                };
                let sent = match transfer {
                    Some(transfer) => slot.worker.post_message_with_transfer(&message, &transfer),
                    None => slot.worker.post_message(&message),
                };
                if let Err(error) = sent {
                    warn!(?error, "Failed to post to a RemoteFX worker");
                    failed = true;
                }
            }
            failed.then(|| {
                group.results.borrow_mut().fail();
                Rc::clone(&group.wake)
            })
        });
        if let Some(wake) = wake {
            wake();
        }
    }
}

impl Slot {
    fn start(
        group: u32,
        shard: usize,
        count: usize,
        module: &js_sys::WebAssembly::Module,
        results: &Rc<RefCell<JobResults<ExternalTiles>>>,
        wake: &Rc<dyn Fn()>,
    ) -> Result<Self, JsValue> {
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

        let on_message = {
            let (results, wake) = (Rc::clone(results), Rc::clone(wake));
            Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
                let Some((job, outcome, ms)) = read_result(group, shard, &event.data()) else {
                    return;
                };
                BUSY_MS.with_borrow_mut(|busy| {
                    if let Some(busy) = busy.get_mut(shard) {
                        *busy += ms;
                    }
                });
                let reply = results.borrow_mut().complete(job, shard, outcome);
                match reply {
                    Reply::Complete => wake(),
                    Reply::Dropped(Ok(tiles)) => return_buffer(tiles),
                    Reply::Pending | Reply::Dropped(Err(_)) => {}
                }
            })
        };
        let on_error = {
            let (results, wake) = (Rc::clone(results), Rc::clone(wake));
            Closure::<dyn FnMut(web_sys::ErrorEvent)>::new(move |event: web_sys::ErrorEvent| {
                warn!(message = %event.message(), shard, "RemoteFX worker failed");
                results.borrow_mut().fail();
                wake();
            })
        };
        worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        worker.set_onerror(Some(on_error.as_ref().unchecked_ref()));

        let slot = Self {
            worker,
            url,
            _on_message: on_message,
            _on_error: on_error,
        };
        let init = Object::new();
        let started = set(&init, "type", &JsValue::from_str("init"))
            .and_then(|()| set(&init, "module", module))
            .and_then(|()| set(&init, "shard", &JsValue::from(shard)))
            .and_then(|()| set(&init, "shards", &JsValue::from(count)))
            .and_then(|()| slot.worker.post_message(&init));
        if let Err(error) = started {
            slot.stop();
            return Err(error);
        }
        Ok(slot)
    }

    fn stop(&self) {
        self.worker.set_onmessage(None);
        self.worker.set_onerror(None);
        self.worker.terminate();
        let _ = web_sys::Url::revoke_object_url(&self.url);
    }
}

/// Reads a `result` message: the job, its outcome for this Worker, and the milliseconds it
/// took.
fn read_result(group: u32, shard: usize, data: &JsValue) -> Option<(u32, Result<ExternalTiles, &'static str>, f64)> {
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
    let ms = number("ms").unwrap_or(0.0);
    let error = as_u32(number("error").unwrap_or(0.0));
    if error != 0 {
        return Some((job, Err(ironrdp_rfx_worker::error_reason(error)), ms));
    }
    let pixels = field("pixels")?.dyn_into::<ArrayBuffer>().ok()?;
    let length = as_u32(number("pixelsLength")?);
    let placements = placements(&field("placements")?.dyn_into::<Int32Array>().ok()?.to_vec());
    Some((
        job,
        Ok(ExternalTiles {
            pixels,
            length,
            placements,
            group,
            shard,
        }),
        ms,
    ))
}

impl ProgressiveBackend for WorkerProgressiveBackend {
    fn max_in_flight(&self) -> usize {
        MAX_IN_FLIGHT
    }

    fn submit(&mut self, job: u32, surface_id: u16, codec_context_id: u32, width: u16, height: u16, data: &[u8]) {
        JOBS.set(JOBS.get().wrapping_add(1));
        // Each Worker gets its own copy: the buffer is transferred.
        self.post_all(|| {
            let bytes = Uint8Array::from(data);
            let message = Object::new();
            set(&message, "type", &JsValue::from_str("submit"))
                .and_then(|()| set(&message, "job", &JsValue::from(job)))
                .and_then(|()| set(&message, "surface", &JsValue::from(surface_id)))
                .and_then(|()| set(&message, "ctx", &JsValue::from(codec_context_id)))
                .and_then(|()| set(&message, "width", &JsValue::from(width)))
                .and_then(|()| set(&message, "height", &JsValue::from(height)))
                .and_then(|()| set(&message, "data", &bytes.buffer()))
                .ok()?;
            Some((message, Some(Array::of1(&bytes.buffer()))))
        });
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
            self.post_all(|| Some((message.clone(), None)));
        }
    }

    fn poll(&mut self, job: u32) -> Option<Result<DecodedTiles, ProgressiveJobError>> {
        let taken = GROUPS.with_borrow(|groups| match groups.get(&self.group) {
            Some(group) => group.results.borrow_mut().take(job),
            None => Some(Err((WORKER_FAILED, Vec::new()))),
        })?;
        match taken {
            Ok(parts) => {
                let handle = TILES.with_borrow_mut(|store| {
                    let handle = store.next;
                    store.next = store.next.wrapping_add(1);
                    store.tiles.insert(handle, parts);
                    handle
                });
                Some(Ok(DecodedTiles {
                    external: Some(handle),
                    tiles: Vec::new(),
                }))
            }
            Err((reason, parts)) => {
                parts.into_iter().for_each(return_buffer);
                Some(Err(ProgressiveJobError { reason }))
            }
        }
    }
}

impl Drop for WorkerProgressiveBackend {
    fn drop(&mut self) {
        if let Some(group) = GROUPS.with_borrow_mut(|groups| groups.remove(&self.group)) {
            group.workers.iter().for_each(Slot::stop);
        }
        TILES.with_borrow_mut(|store| {
            store
                .tiles
                .retain(|_, parts| parts.iter().all(|part| part.group != self.group))
        });
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
    fn a_job_completes_when_every_shard_has_answered() {
        let mut results = JobResults::new(3);
        assert_eq!(results.complete(1, 2, Ok(12)), Reply::Pending);
        assert_eq!(results.complete(1, 0, Ok(10)), Reply::Pending);
        assert_eq!(results.take(1), None, "shard 1 has not answered");
        assert_eq!(results.complete(1, 1, Ok(11)), Reply::Complete);
        assert_eq!(results.take(1), Some(Ok(vec![10, 11, 12])), "in shard order");
        assert_eq!(results.take(1), None, "each result is handed out once");
    }

    #[test]
    fn a_failed_shard_fails_the_job_and_hands_back_the_rest() {
        let mut results = JobResults::new(3);
        results.complete(4, 0, Ok(40));
        results.complete(4, 1, Err("rfx progressive decode failed: RLGR"));
        results.complete(4, 2, Ok(42));
        assert_eq!(
            results.take(4),
            Some(Err(("rfx progressive decode failed: RLGR", vec![40, 42])))
        );
    }

    #[test]
    fn a_lost_worker_fails_outstanding_jobs() {
        let mut results = JobResults::new(3);
        for shard in 0..3 {
            results.complete(1, shard, Ok(10 + shard));
        }
        results.complete(2, 0, Ok(20));
        results.complete(2, 2, Ok(22));
        results.fail();
        assert_eq!(
            results.take(1),
            Some(Ok(vec![10, 11, 12])),
            "finished work still applies"
        );
        assert_eq!(results.take(2), Some(Err((WORKER_FAILED, vec![20, 22]))));
        assert_eq!(results.take(3), Some(Err((WORKER_FAILED, vec![]))));
    }

    #[test]
    fn a_late_reply_after_a_failure_is_handed_back() {
        let mut results = JobResults::new(2);
        results.complete(1, 0, Ok(10));
        results.fail();
        assert_eq!(results.take(1), Some(Err((WORKER_FAILED, vec![10]))));
        assert_eq!(
            results.complete(1, 1, Ok(11)),
            Reply::Dropped(Ok(11)),
            "its buffer goes back"
        );
        assert_eq!(results.take(1), Some(Err((WORKER_FAILED, vec![]))), "no entry was kept");
    }

    #[test]
    fn worker_count_leaves_two_cores_and_caps_at_four() {
        assert_eq!(worker_count(None), 1);
        assert_eq!(worker_count(Some(1.0)), 1);
        assert_eq!(worker_count(Some(4.0)), 2);
        assert_eq!(worker_count(Some(10.0)), 4);
        assert_eq!(worker_count(Some(f64::NAN)), 1);
    }
}
