//! [`WasmDriveBackend`]: the RDPDR backend that answers server drive IRPs by driving a
//! [`super::fs::DriveFs`] implementation asynchronously.
//!
//! Architecture mirrors [`crate::printer::WasmPrinterBackend`] in spirit — a backend living on
//! the SVC processor side — but with a twist: unlike the printer backend (which can synthesize
//! every completion synchronously), most drive IRPs need an `await` on `DriveFs` before they can
//! be answered, and [`ironrdp::rdpdr::backend::RdpdrBackend::handle_drive_io_request`] is a
//! synchronous `&mut self` method that cannot `.await` anything itself. So the shape here is:
//! look up whatever `DriveState` already knows synchronously; if that alone answers the IRP
//! (a cached directory-listing pop, a read-only-mode denial, an unknown handle), return the
//! completion directly. Otherwise clone what's needed, spawn a `'static` future that drives
//! `DriveFs`, and return `Ok(vec![])` — the future sends its completion via
//! [`DriveBackendMessage::IoCompleted`] once `DriveFs` resolves.
//!
//! `fs: Rc<dyn DriveFs>` and `state: Rc<RefCell<DriveState>>` make this backend, in the general
//! case, `!Send` — see [`super::fs`]'s module doc for why that tension exists and is resolved
//! with `unsafe impl Send` below.

use core::cell::{Cell, RefCell};
use core::fmt;
use std::collections::VecDeque;
use std::rc::Rc;

use futures_channel::mpsc;
use futures_util::future::LocalBoxFuture;
use ironrdp::rdpdr::backend::RdpdrBackend;
use ironrdp::rdpdr::pdu::RdpdrPdu;
use ironrdp::rdpdr::pdu::efs::{
    Boolean, Characteristics, ClientDriveLockControlResponse, ClientDriveQueryDirectoryResponse,
    ClientDriveQueryInformationResponse, ClientDriveQuerySecurityResponse, ClientDriveQueryVolumeInformationResponse,
    ClientDriveSetInformationResponse, ClientDriveSetSecurityResponse, CreateDisposition, CreateOptions, DesiredAccess,
    DeviceCloseRequest, DeviceCloseResponse, DeviceControlResponse, DeviceCreateRequest, DeviceCreateResponse,
    DeviceFlushBuffersResponse, DeviceIoResponse, DeviceReadRequest, DeviceReadResponse, DeviceWriteRequest,
    DeviceWriteResponse, FileAttributeTagInformation, FileAttributes, FileBasicInformation,
    FileBothDirectoryInformation, FileDirectoryInformation, FileDispositionInformation, FileFsAttributeInformation,
    FileFsDeviceInformation, FileFsFullSizeInformation, FileFsSizeInformation, FileFsVolumeInformation,
    FileFullDirectoryInformation, FileInformationClass, FileInformationClassLevel, FileNamesInformation,
    FileStandardInformation, FileSystemAttributes, FileSystemInformationClass, FileSystemInformationClassLevel,
    Information, NtStatus, PrinterIoRequest, ServerDeviceAnnounceResponse, ServerDriveIoRequest,
    ServerDriveQueryDirectoryRequest, ServerDriveQueryInformationRequest, ServerDriveQueryVolumeInformationRequest,
    ServerDriveSetInformationRequest,
};
use ironrdp::rdpdr::pdu::esc::{ScardCall, ScardIoCtlCode};
use ironrdp_core::{EncodeResult, impl_as_any};
use ironrdp_pdu::{PduError, PduErrorExt as _, PduResult, pdu_other_err};
use ironrdp_svc::SvcMessage;
use tracing::{debug, info, trace, warn};

use super::fs::{DriveFs, FsEntry, FsError};
use super::state::{DriveState, normalize_path};

/// Current size of the wasm linear memory, in bytes.
///
/// Diagnostic only: the browser tab has been observed dying outright (renderer crash, not a JS
/// exception) shortly after drive activity, including after a single 33-byte read. A renderer
/// death like that is an allocation failure rather than a protocol error, so the read path logs
/// this to show whether the heap is growing without bound.
#[cfg(target_arch = "wasm32")]
fn wasm_heap_bytes() -> u32 {
    use wasm_bindgen::JsCast as _;

    wasm_bindgen::memory()
        .dyn_into::<js_sys::WebAssembly::Memory>()
        .ok()
        .and_then(|memory| memory.buffer().dyn_into::<js_sys::ArrayBuffer>().ok())
        .map_or(0, |buffer| buffer.byte_length())
}

#[cfg(not(target_arch = "wasm32"))]
fn wasm_heap_bytes() -> u32 {
    0
}

/// Maximum bytes this backend will honor from a single `DeviceReadRequest`, regardless of what
/// the server actually asked for (`dispatch_read` clamps with `.min(MAX_DRIVE_READ_BYTES)`).
///
/// 1 MiB, matching what FreeRDP's drive channel honors (`drive_process_irp_read` in
/// `channels/drive/client/drive_main.c` reads the requested `Length` outright, with no cap) —
/// so this is a ceiling on absurd requests, not a throttle on normal ones.
///
/// A SHORT READ IS NOT SAFE HERE, despite MS-RDPEFS allowing `DeviceReadResponse` to carry fewer
/// bytes than requested. Measured against a live Windows Server host: after answering a
/// `length=1048576, offset=0` request with only 65536 bytes, the redirector's *next* read came in
/// at `offset=1048576` — it advanced by the REQUESTED length, not the returned one, silently
/// leaving a 960 KiB hole in the destination file. Windows also pipelines these reads
/// concurrently rather than issuing them in response to each completion, so there is no
/// follow-up read at the short-read boundary to fill the gap. Returning less than asked for
/// therefore corrupts copies instead of throttling them; only a genuine EOF short read is safe.
const MAX_DRIVE_READ_BYTES: u32 = 1024 * 1024;

/// Message sent from [`WasmDriveBackend`] each time an async `DriveFs` operation completes and
/// produces RDPDR completion PDU(s) to send back to the server.
///
/// Deliberately decoupled from `crate::session::RdpInputEvent` (which lives in a module this
/// task does not touch): the caller wires this into `RdpInputEvent::DriveBackend` (or
/// equivalent) on the session event loop.
#[derive(Debug)]
pub(crate) enum DriveBackendMessage {
    IoCompleted(Vec<SvcMessage>),
}

/// Spawns a `'static` future on whatever single-threaded executor is driving this backend.
/// Injected so native tests can supply `futures::executor::LocalPool` + `LocalSpawnExt` instead
/// of the real `wasm_bindgen_futures::spawn_local` [`wasm_drive_pair`] uses.
pub(crate) type DriveFsSpawner = Rc<dyn Fn(LocalBoxFuture<'static, ()>)>;

/// RDPDR backend that answers server drive IRPs by driving a [`DriveFs`] implementation.
pub(crate) struct WasmDriveBackend {
    fs: Rc<dyn DriveFs>,
    state: Rc<RefCell<DriveState>>,
    read_only: bool,
    tx: mpsc::UnboundedSender<DriveBackendMessage>,
    spawn: DriveFsSpawner,
    /// Bumped by [`Self::reset`] (called on every RDPDR Server Announce Request — see
    /// [`ironrdp::rdpdr::backend::RdpdrBackend::reset`]'s own doc comment). A future spawned
    /// before a `reset` captures the generation it was spawned in; if that no longer matches
    /// this counter by the time the future resolves, its completion is dropped instead of being
    /// delivered into the new sequence stamped with a now-stale `device_id`/`completion_id`.
    generation: Rc<Cell<u64>>,
    /// FIFO of IRP futures awaiting execution, drained strictly one at a time by a single worker
    /// (see [`Self::enqueue`]). This matches FreeRDP's drive channel, which processes IRPs
    /// serially on one thread (`drive_thread_func` + `MessageQueue` in
    /// `channels/drive/client/drive_main.c`): Windows pipelines requests, but every mainstream
    /// client answers them in order, one at a time. Running them concurrently is spec-legal but
    /// untested interop territory, and it lets N pipelined 1 MiB reads hold N response buffers
    /// (plus their JS-side `ArrayBuffer`s) alive at once. Serial execution bounds that to one.
    queue: Rc<RefCell<VecDeque<LocalBoxFuture<'static, ()>>>>,
    /// Whether the drain worker spawned by [`Self::enqueue`] is currently running.
    draining: Rc<Cell<bool>>,
}

// SAFETY: `RdpdrBackend: Send` is a supertrait bound the SVC processor's generic
// `Box<dyn RdpdrBackend>` storage requires unconditionally — it is not evidence that a backend
// is ever actually moved across a real OS thread. This backend, like the browser environment it
// targets, only ever runs on a single logical thread: constructed and driven from the wasm
// event loop (or, in native tests, a single test-harness thread pumping
// `futures::executor::LocalPool`). Its `Rc`-based fields (`fs`, `state`, `spawn`) and the
// futures spawned through `spawn` never cross a real thread boundary. See `super::fs`'s module
// doc for the same bridge described from `DriveFs`'s side.
unsafe impl Send for WasmDriveBackend {}

impl fmt::Debug for WasmDriveBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmDriveBackend")
            .field("read_only", &self.read_only)
            .field("state", &self.state)
            .field("generation", &self.generation.get())
            .finish_non_exhaustive()
    }
}

impl_as_any!(WasmDriveBackend);

impl WasmDriveBackend {
    /// Constructs a backend driven by an injected `spawn` function. Crate-internal: native
    /// tests call this directly with a `LocalPool`-backed spawner; [`wasm_drive_pair`] is the
    /// public wasm-facing factory that supplies `wasm_bindgen_futures::spawn_local`.
    pub(crate) fn new(
        tx: mpsc::UnboundedSender<DriveBackendMessage>,
        fs: Rc<dyn DriveFs>,
        read_only: bool,
        spawn: DriveFsSpawner,
    ) -> Self {
        Self {
            fs,
            state: Rc::new(RefCell::new(DriveState::new())),
            read_only,
            tx,
            spawn,
            generation: Rc::new(Cell::new(0)),
            queue: Rc::new(RefCell::new(VecDeque::new())),
            draining: Rc::new(Cell::new(false)),
        }
    }

    /// Queues an IRP future for strictly serial execution: each future runs to completion before
    /// the next starts, in dispatch (i.e. wire-arrival) order. A single drain worker is spawned
    /// lazily and exits when the queue empties; everything here runs on one logical thread, and
    /// there is no `await` between observing the empty queue and clearing `draining`, so the
    /// spawn-or-not decision cannot race.
    ///
    /// `reset()` deliberately does NOT clear this queue: a queued stale-generation future still
    /// runs (its `DriveFs` side effects happen, matching the previous concurrent behavior), but
    /// its completion is dropped by `send_completion_if_current`.
    fn enqueue(&self, future: LocalBoxFuture<'static, ()>) {
        self.queue.borrow_mut().push_back(future);
        if self.draining.get() {
            return;
        }
        self.draining.set(true);
        let queue = Rc::clone(&self.queue);
        let draining = Rc::clone(&self.draining);
        (self.spawn)(Box::pin(async move {
            loop {
                let next = queue.borrow_mut().pop_front();
                match next {
                    Some(irp_future) => irp_future.await,
                    None => break,
                }
            }
            draining.set(false);
        }));
    }

    /// Snapshots the current generation before spawning a future, so the future can check —
    /// once it resolves — whether a `reset` superseded it in the meantime.
    fn spawn_generation(&self) -> (Rc<Cell<u64>>, u64) {
        let generation = Rc::clone(&self.generation);
        let spawned_generation = generation.get();
        (generation, spawned_generation)
    }

    fn dispatch_create(&self, create: DeviceCreateRequest) -> Vec<SvcMessage> {
        if self.read_only && is_write_intent(&create) {
            log_outgoing(
                "DeviceCreateResponse",
                create.device_io_request.completion_id,
                create.device_io_request.device_id,
                NtStatus::ACCESS_DENIED,
            );
            let response = DeviceCreateResponse {
                device_io_reply: DeviceIoResponse::new(create.device_io_request, NtStatus::ACCESS_DENIED),
                file_id: 0,
                information: Information::empty(),
            };
            return vec![SvcMessage::from(RdpdrPdu::DeviceCreateResponse(response))];
        }

        // `state.rs`'s own docs promise every drive-IRP path reaches `DriveFs` only after
        // `normalize_path` validation (rejecting `..` traversal); this backend is `DriveFs`'s
        // one caller, so it enforces that here rather than leaving it to whatever a given
        // `DriveFs` implementation happens to check internally.
        if !path_is_valid(&create.path) {
            log_outgoing(
                "DeviceCreateResponse",
                create.device_io_request.completion_id,
                create.device_io_request.device_id,
                NtStatus::ACCESS_DENIED,
            );
            let response = DeviceCreateResponse {
                device_io_reply: DeviceIoResponse::new(create.device_io_request, NtStatus::ACCESS_DENIED),
                file_id: 0,
                information: Information::empty(),
            };
            return vec![SvcMessage::from(RdpdrPdu::DeviceCreateResponse(response))];
        }

        let is_directory = create.create_options.contains(CreateOptions::FILE_DIRECTORY_FILE);
        let must_not_be_directory = create.create_options.contains(CreateOptions::FILE_NON_DIRECTORY_FILE);
        let creates_new = !matches!(create.create_disposition, CreateDisposition::FILE_OPEN);
        let truncates = matches!(
            create.create_disposition,
            CreateDisposition::FILE_SUPERSEDE
                | CreateDisposition::FILE_OVERWRITE
                | CreateDisposition::FILE_OVERWRITE_IF
        );
        let write = is_write_intent(&create);
        let create_information = if create.create_disposition == CreateDisposition::FILE_CREATE {
            Information::FILE_CREATED
        } else {
            Information::FILE_OPENED
        };

        let fs = Rc::clone(&self.fs);
        let state = Rc::clone(&self.state);
        let tx = self.tx.clone();
        let (generation, spawned_generation) = self.spawn_generation();
        // `create` is not used again after this point, so its two fields the future needs are
        // moved out directly rather than cloned.
        let device_io_request = create.device_io_request;
        let path = create.path;

        let future: LocalBoxFuture<'static, ()> = Box::pin(async move {
            let outcome = resolve_create_outcome(
                fs.as_ref(),
                &path,
                is_directory,
                must_not_be_directory,
                write,
                creates_new,
                truncates,
            )
            .await;

            let (status, file_id, information) = match outcome {
                Ok((fs_handle, is_dir)) => {
                    let file_id = state.borrow_mut().open(path, fs_handle, is_dir);
                    (NtStatus::SUCCESS, file_id, create_information)
                }
                Err(err) => (create_status_for(&err), 0, Information::empty()),
            };

            log_outgoing(
                "DeviceCreateResponse",
                device_io_request.completion_id,
                device_io_request.device_id,
                status,
            );
            let response = DeviceCreateResponse {
                device_io_reply: DeviceIoResponse::new(device_io_request, status),
                file_id,
                information,
            };
            send_completion_if_current(
                &tx,
                &generation,
                spawned_generation,
                vec![SvcMessage::from(RdpdrPdu::DeviceCreateResponse(response))],
            );
        });
        self.enqueue(future);
        Vec::new()
    }

    fn dispatch_query_information(&self, req: ServerDriveQueryInformationRequest) -> Vec<SvcMessage> {
        let file_id = req.device_io_request.file_id;
        let Some(path) = self.state.borrow().get(file_id).map(|entry| entry.path.clone()) else {
            log_outgoing(
                "ClientDriveQueryInformationResponse",
                req.device_io_request.completion_id,
                req.device_io_request.device_id,
                NtStatus::INVALID_HANDLE,
            );
            let response = ClientDriveQueryInformationResponse {
                device_io_response: DeviceIoResponse::new(req.device_io_request, NtStatus::INVALID_HANDLE),
                buffer: None,
            };
            return vec![SvcMessage::from(RdpdrPdu::ClientDriveQueryInformationResponse(
                response,
            ))];
        };

        let fs = Rc::clone(&self.fs);
        let tx = self.tx.clone();
        let (generation, spawned_generation) = self.spawn_generation();
        // `req` is not used again after this point (the `let-else` above already returned in
        // the only branch that still needed it), so its fields are moved rather than cloned.
        let class_lvl = req.file_info_class_lvl;
        let device_io_request = req.device_io_request;

        let future: LocalBoxFuture<'static, ()> = Box::pin(async move {
            let (status, buffer) = match fs.stat(&path).await {
                Ok(entry) => match build_query_info(&class_lvl, &entry) {
                    Some(info) => (NtStatus::SUCCESS, Some(info)),
                    None => (NtStatus::NOT_SUPPORTED, None),
                },
                Err(err) => (nt_status_for(&err), None),
            };
            log_outgoing(
                "ClientDriveQueryInformationResponse",
                device_io_request.completion_id,
                device_io_request.device_id,
                status,
            );
            let response = ClientDriveQueryInformationResponse {
                device_io_response: DeviceIoResponse::new(device_io_request, status),
                buffer,
            };
            send_completion_if_current(
                &tx,
                &generation,
                spawned_generation,
                vec![SvcMessage::from(RdpdrPdu::ClientDriveQueryInformationResponse(
                    response,
                ))],
            );
        });
        self.enqueue(future);
        Vec::new()
    }

    fn dispatch_close(&self, req: DeviceCloseRequest) -> Vec<SvcMessage> {
        let file_id = req.device_io_request.file_id;
        let Some(entry) = self.state.borrow_mut().close(file_id) else {
            log_outgoing(
                "DeviceCloseResponse",
                req.device_io_request.completion_id,
                req.device_io_request.device_id,
                NtStatus::INVALID_HANDLE,
            );
            let response = DeviceCloseResponse {
                device_io_response: DeviceIoResponse::new(req.device_io_request, NtStatus::INVALID_HANDLE),
            };
            return vec![SvcMessage::from(RdpdrPdu::DeviceCloseResponse(response))];
        };

        let Some(fs_handle) = entry.fs_handle else {
            // Directory handles have nothing to close on the `DriveFs` side (see
            // `OpenEntry::fs_handle`'s doc comment in `state.rs`).
            log_outgoing(
                "DeviceCloseResponse",
                req.device_io_request.completion_id,
                req.device_io_request.device_id,
                NtStatus::SUCCESS,
            );
            let response = DeviceCloseResponse {
                device_io_response: DeviceIoResponse::new(req.device_io_request, NtStatus::SUCCESS),
            };
            return vec![SvcMessage::from(RdpdrPdu::DeviceCloseResponse(response))];
        };

        let fs = Rc::clone(&self.fs);
        let tx = self.tx.clone();
        let (generation, spawned_generation) = self.spawn_generation();
        let device_io_request = req.device_io_request;
        let future: LocalBoxFuture<'static, ()> = Box::pin(async move {
            let status = match fs.close(fs_handle).await {
                Ok(()) => NtStatus::SUCCESS,
                Err(err) => nt_status_for(&err),
            };
            log_outgoing(
                "DeviceCloseResponse",
                device_io_request.completion_id,
                device_io_request.device_id,
                status,
            );
            let response = DeviceCloseResponse {
                device_io_response: DeviceIoResponse::new(device_io_request, status),
            };
            send_completion_if_current(
                &tx,
                &generation,
                spawned_generation,
                vec![SvcMessage::from(RdpdrPdu::DeviceCloseResponse(response))],
            );
        });
        self.enqueue(future);
        Vec::new()
    }

    fn dispatch_query_directory(&self, req: ServerDriveQueryDirectoryRequest) -> Vec<SvcMessage> {
        let file_id = req.device_io_request.file_id;
        let device_io_request = req.device_io_request;

        if req.initial_query == 0 {
            // Continuation of an already-cached listing: pop the next entry synchronously, no
            // `DriveFs` round-trip needed.
            let popped = self.state.borrow_mut().next_dir_entry(file_id);
            let (status, buffer) = match popped {
                Some(entry) => (
                    NtStatus::SUCCESS,
                    Some(build_dir_info(&req.file_info_class_lvl, &entry)),
                ),
                None => (NtStatus::NO_MORE_FILES, None),
            };
            log_outgoing(
                "ClientDriveQueryDirectoryResponse",
                device_io_request.completion_id,
                device_io_request.device_id,
                status,
            );
            let response = ClientDriveQueryDirectoryResponse {
                device_io_reply: DeviceIoResponse::new(device_io_request, status),
                buffer,
            };
            return vec![SvcMessage::from(RdpdrPdu::ClientDriveQueryDirectoryResponse(response))];
        }

        // Initial query: re-list the directory this `file_id` was opened against and apply
        // whatever search pattern `req.path` carries (e.g. `\dir\*.txt` or an exact filename
        // for an existence check) against each entry's name before caching.
        let Some(path) = self.state.borrow().get(file_id).map(|entry| entry.path.clone()) else {
            log_outgoing(
                "ClientDriveQueryDirectoryResponse",
                device_io_request.completion_id,
                device_io_request.device_id,
                NtStatus::INVALID_HANDLE,
            );
            let response = ClientDriveQueryDirectoryResponse {
                device_io_reply: DeviceIoResponse::new(device_io_request, NtStatus::INVALID_HANDLE),
                buffer: None,
            };
            return vec![SvcMessage::from(RdpdrPdu::ClientDriveQueryDirectoryResponse(response))];
        };

        let fs = Rc::clone(&self.fs);
        let state = Rc::clone(&self.state);
        let tx = self.tx.clone();
        let (generation, spawned_generation) = self.spawn_generation();
        let class_lvl = req.file_info_class_lvl;
        let search_pattern = dir_search_pattern(&req.path);

        let future: LocalBoxFuture<'static, ()> = Box::pin(async move {
            let (status, buffer) = match fs.list(&path).await {
                Ok(listing) => {
                    let filtered = match &search_pattern {
                        Some(pattern) => listing
                            .into_iter()
                            .filter(|entry| dos_wildcard_match(pattern, &entry.name))
                            .collect(),
                        None => listing,
                    };
                    state.borrow_mut().set_dir_listing(file_id, filtered);
                    match state.borrow_mut().next_dir_entry(file_id) {
                        Some(entry) => (NtStatus::SUCCESS, Some(build_dir_info(&class_lvl, &entry))),
                        // Zero matches on the INITIAL query (as opposed to an exhausted
                        // continuation, handled above) is `NO_SUCH_FILE`, matching
                        // FreeRDP/Windows drive-redirection semantics — `NO_MORE_FILES` means
                        // "there were some, you've seen them all."
                        None => (NtStatus::NO_SUCH_FILE, None),
                    }
                }
                Err(err) => (nt_status_for(&err), None),
            };
            log_outgoing(
                "ClientDriveQueryDirectoryResponse",
                device_io_request.completion_id,
                device_io_request.device_id,
                status,
            );
            let response = ClientDriveQueryDirectoryResponse {
                device_io_reply: DeviceIoResponse::new(device_io_request, status),
                buffer,
            };
            send_completion_if_current(
                &tx,
                &generation,
                spawned_generation,
                vec![SvcMessage::from(RdpdrPdu::ClientDriveQueryDirectoryResponse(response))],
            );
        });
        self.enqueue(future);
        Vec::new()
    }

    fn dispatch_read(&self, req: DeviceReadRequest) -> Vec<SvcMessage> {
        let file_id = req.device_io_request.file_id;
        let device_io_request = req.device_io_request;
        let Some(fs_handle) = self.state.borrow().get(file_id).and_then(|entry| entry.fs_handle) else {
            log_outgoing(
                "DeviceReadResponse",
                device_io_request.completion_id,
                device_io_request.device_id,
                NtStatus::INVALID_HANDLE,
            );
            let response = DeviceReadResponse {
                device_io_reply: DeviceIoResponse::new(device_io_request, NtStatus::INVALID_HANDLE),
                read_data: Vec::new(),
            };
            return vec![SvcMessage::from(RdpdrPdu::DeviceReadResponse(response))];
        };

        let fs = Rc::clone(&self.fs);
        let tx = self.tx.clone();
        let (generation, spawned_generation) = self.spawn_generation();
        let offset = req.offset;
        let requested_len = req.length;
        // See `MAX_DRIVE_READ_BYTES`: the redirector advances by the REQUESTED length whether or
        // not we return that much, so anything short of a real EOF read punches holes in the
        // destination file. This ceiling only guards against an absurd request.
        let clamped_len = requested_len.min(MAX_DRIVE_READ_BYTES);

        let future: LocalBoxFuture<'static, ()> = Box::pin(async move {
            let (status, read_data) = match fs.read(fs_handle, offset, clamped_len).await {
                Ok(data) => (NtStatus::SUCCESS, data),
                Err(err) => (nt_status_for(&err), Vec::new()),
            };
            // INFO (not debug) on purpose: the read path is the one we are
            // actively bisecting against a live server, and DEBUG-level output
            // for every IRP floods the browser console badly enough to kill the
            // tab. Reads alone are low-volume.
            info!(
                "[rdpdr-drive] dispatch_read offset={offset} requested_len={requested_len} clamped_len={clamped_len} returned_len={} status={status:?} heap={}",
                read_data.len(),
                wasm_heap_bytes()
            );
            log_outgoing(
                "DeviceReadResponse",
                device_io_request.completion_id,
                device_io_request.device_id,
                status,
            );
            let response = DeviceReadResponse {
                device_io_reply: DeviceIoResponse::new(device_io_request, status),
                read_data,
            };
            send_completion_if_current(
                &tx,
                &generation,
                spawned_generation,
                vec![SvcMessage::from(RdpdrPdu::DeviceReadResponse(response))],
            );
        });
        self.enqueue(future);
        Vec::new()
    }

    fn dispatch_write(&self, req: DeviceWriteRequest) -> Vec<SvcMessage> {
        let file_id = req.device_io_request.file_id;
        let device_io_request = req.device_io_request;
        if self.read_only {
            log_outgoing(
                "DeviceWriteResponse",
                device_io_request.completion_id,
                device_io_request.device_id,
                NtStatus::ACCESS_DENIED,
            );
            let response = DeviceWriteResponse {
                device_io_reply: DeviceIoResponse::new(device_io_request, NtStatus::ACCESS_DENIED),
                length: 0,
            };
            return vec![SvcMessage::from(RdpdrPdu::DeviceWriteResponse(response))];
        }

        let Some(fs_handle) = self.state.borrow().get(file_id).and_then(|entry| entry.fs_handle) else {
            log_outgoing(
                "DeviceWriteResponse",
                device_io_request.completion_id,
                device_io_request.device_id,
                NtStatus::INVALID_HANDLE,
            );
            let response = DeviceWriteResponse {
                device_io_reply: DeviceIoResponse::new(device_io_request, NtStatus::INVALID_HANDLE),
                length: 0,
            };
            return vec![SvcMessage::from(RdpdrPdu::DeviceWriteResponse(response))];
        };

        let fs = Rc::clone(&self.fs);
        let tx = self.tx.clone();
        let (generation, spawned_generation) = self.spawn_generation();
        let offset = req.offset;
        let data = req.write_data;

        let future: LocalBoxFuture<'static, ()> = Box::pin(async move {
            let (status, length) = match fs.write(fs_handle, offset, data).await {
                Ok(written) => (NtStatus::SUCCESS, written),
                Err(err) => (nt_status_for(&err), 0),
            };
            log_outgoing(
                "DeviceWriteResponse",
                device_io_request.completion_id,
                device_io_request.device_id,
                status,
            );
            let response = DeviceWriteResponse {
                device_io_reply: DeviceIoResponse::new(device_io_request, status),
                length,
            };
            send_completion_if_current(
                &tx,
                &generation,
                spawned_generation,
                vec![SvcMessage::from(RdpdrPdu::DeviceWriteResponse(response))],
            );
        });
        self.enqueue(future);
        Vec::new()
    }

    fn dispatch_set_information(&self, req: ServerDriveSetInformationRequest) -> PduResult<Vec<SvcMessage>> {
        if self.read_only {
            log_outgoing(
                "ClientDriveSetInformationResponse",
                req.device_io_request.completion_id,
                req.device_io_request.device_id,
                NtStatus::ACCESS_DENIED,
            );
            return Ok(vec![set_information_message(&req, NtStatus::ACCESS_DENIED)?]);
        }

        let file_id = req.device_io_request.file_id;
        let Some(path) = self.state.borrow().get(file_id).map(|entry| entry.path.clone()) else {
            log_outgoing(
                "ClientDriveSetInformationResponse",
                req.device_io_request.completion_id,
                req.device_io_request.device_id,
                NtStatus::INVALID_HANDLE,
            );
            return Ok(vec![set_information_message(&req, NtStatus::INVALID_HANDLE)?]);
        };

        // Only rename and delete-disposition map onto a `DriveFs` primitive. The remaining
        // classes `ServerDriveSetInformationRequest::decode` accepts — Basic/EndOfFile/
        // Allocation — have no corresponding `DriveFs` operation (no chmod/resize primitive in
        // Task 1's scope), so they complete immediately as unsupported rather than pretending to
        // apply.
        match req.set_buffer.clone() {
            FileInformationClass::Rename(rename) => {
                // Same rule as `Create`'s path (see `dispatch_create`): a server-supplied path
                // must clear `normalize_path` before it ever reaches `DriveFs`.
                if !path_is_valid(&rename.file_name) {
                    log_outgoing(
                        "ClientDriveSetInformationResponse",
                        req.device_io_request.completion_id,
                        req.device_io_request.device_id,
                        NtStatus::ACCESS_DENIED,
                    );
                    return Ok(vec![set_information_message(&req, NtStatus::ACCESS_DENIED)?]);
                }

                let fs = Rc::clone(&self.fs);
                let state = Rc::clone(&self.state);
                let tx = self.tx.clone();
                let (generation, spawned_generation) = self.spawn_generation();
                let from = path;
                let to = rename.file_name;
                let future: LocalBoxFuture<'static, ()> = Box::pin(async move {
                    let status = match fs.rename(&from, &to).await {
                        Ok(()) => {
                            if let Some(entry) = state.borrow_mut().get_mut(file_id) {
                                entry.path = to;
                            }
                            NtStatus::SUCCESS
                        }
                        Err(err) => nt_status_for(&err),
                    };
                    log_outgoing(
                        "ClientDriveSetInformationResponse",
                        req.device_io_request.completion_id,
                        req.device_io_request.device_id,
                        status,
                    );
                    let message = set_information_message(&req, status).unwrap_or_else(|error| {
                        warn!(
                            ?error,
                            "Failed to encode ClientDriveSetInformationResponse; sending UNSUCCESSFUL fallback"
                        );
                        set_information_fallback_message(&req, NtStatus::UNSUCCESSFUL)
                    });
                    send_completion_if_current(&tx, &generation, spawned_generation, vec![message]);
                });
                self.enqueue(future);
                Ok(Vec::new())
            }
            FileInformationClass::Disposition(disposition) if disposition.delete_pending != 0 => {
                let fs = Rc::clone(&self.fs);
                let tx = self.tx.clone();
                let (generation, spawned_generation) = self.spawn_generation();
                let target = path;
                let future: LocalBoxFuture<'static, ()> = Box::pin(async move {
                    let status = match fs.delete(&target).await {
                        Ok(()) => NtStatus::SUCCESS,
                        Err(err) => nt_status_for(&err),
                    };
                    log_outgoing(
                        "ClientDriveSetInformationResponse",
                        req.device_io_request.completion_id,
                        req.device_io_request.device_id,
                        status,
                    );
                    let message = set_information_message(&req, status).unwrap_or_else(|error| {
                        warn!(
                            ?error,
                            "Failed to encode ClientDriveSetInformationResponse; sending UNSUCCESSFUL fallback"
                        );
                        set_information_fallback_message(&req, NtStatus::UNSUCCESSFUL)
                    });
                    send_completion_if_current(&tx, &generation, spawned_generation, vec![message]);
                });
                self.enqueue(future);
                Ok(Vec::new())
            }
            // `Disposition` with `delete_pending == 0` (clearing a delete request we never
            // actually deferred) is a trivial acknowledgement; every other class has no
            // `DriveFs` primitive.
            FileInformationClass::Disposition(_) => {
                log_outgoing(
                    "ClientDriveSetInformationResponse",
                    req.device_io_request.completion_id,
                    req.device_io_request.device_id,
                    NtStatus::SUCCESS,
                );
                Ok(vec![set_information_message(&req, NtStatus::SUCCESS)?])
            }
            _ => {
                log_outgoing(
                    "ClientDriveSetInformationResponse",
                    req.device_io_request.completion_id,
                    req.device_io_request.device_id,
                    NtStatus::NOT_SUPPORTED,
                );
                Ok(vec![set_information_message(&req, NtStatus::NOT_SUPPORTED)?])
            }
        }
    }
}

impl RdpdrBackend for WasmDriveBackend {
    /// Called by `Rdpdr` on every Server Announce Request (a new RDPDR init sequence) — see
    /// this method's own doc comment on the trait: stateful backends MUST override it to
    /// discard deferred operations before devices are re-announced. Without this override, a
    /// re-init would keep every stale `file_id` and its `DriveFs` handle alive forever (leaked
    /// browser handles/writables), and any future still in flight from the previous sequence
    /// would eventually deliver a completion stamped with a `device_id`/`completion_id` that no
    /// longer means anything in the new one.
    fn reset(&mut self) -> PduResult<()> {
        // Any future spawned before this point now belongs to a superseded generation; its
        // completion (if any) will be dropped by `send_completion_if_current` instead of
        // reaching the new sequence.
        self.generation.set(self.generation.get().wrapping_add(1));

        let stale_handles = self.state.borrow().open_fs_handles();
        self.state = Rc::new(RefCell::new(DriveState::new()));

        if !stale_handles.is_empty() {
            let fs = Rc::clone(&self.fs);
            let future: LocalBoxFuture<'static, ()> = Box::pin(async move {
                for handle in stale_handles {
                    // Best-effort: the RDPDR sequence that opened these is already gone, so
                    // there is nobody left to report a close failure to.
                    let _ = fs.close(handle).await;
                }
            });
            self.enqueue(future);
        }

        Ok(())
    }

    fn handle_server_device_announce_response(&mut self, pdu: ServerDeviceAnnounceResponse) -> PduResult<()> {
        // Surface server-side rejection at `warn!` so a redirected share that silently never
        // appears in the session is visible at the default tracing level (same rationale as
        // `WasmPrinterBackend`'s override of this method).
        if pdu.result_code == NtStatus::SUCCESS {
            debug!(device_id = pdu.device_id, "RDPDR drive announce accepted by server");
        } else {
            warn!(
                device_id = pdu.device_id,
                result_code = ?pdu.result_code,
                "RDPDR drive announce rejected by server; redirected share will not appear in session"
            );
        }
        Ok(())
    }

    fn handle_scard_call(
        &mut self,
        _req: ironrdp::rdpdr::pdu::efs::DeviceControlRequest<ScardIoCtlCode>,
        _call: ScardCall,
    ) -> PduResult<Vec<SvcMessage>> {
        Err(pdu_other_err!("smartcard IOCTL reached drive-only WasmDriveBackend"))
    }

    fn handle_drive_io_request(&mut self, req: ServerDriveIoRequest) -> PduResult<Vec<SvcMessage>> {
        // Diagnostic instrumentation (kept, not removed after debugging) — see this backend's
        // module doc comment for why: every incoming IRP, logged before dispatch so it's visible
        // even for a variant this backend never answers (e.g. NotifyChangeDirectory).
        let (variant, device_id, completion_id, file_id) = drive_request_debug_info(&req);
        debug!("[rdpdr-drive] IN {variant} device_id={device_id} completion_id={completion_id} file_id={file_id}");
        if let ServerDriveIoRequest::ServerCreateDriveRequest(create) = &req {
            debug!(
                "[rdpdr-drive] IN Create details path={:?} create_disposition={:#x} create_options={:#x} desired_access={:#x}",
                create.path,
                u32::from(create.create_disposition),
                create.create_options.bits(),
                create.desired_access.bits(),
            );
        }

        match req {
            ServerDriveIoRequest::ServerCreateDriveRequest(create) => Ok(self.dispatch_create(create)),
            ServerDriveIoRequest::ServerDriveQueryInformationRequest(req) => Ok(self.dispatch_query_information(req)),
            ServerDriveIoRequest::DeviceCloseRequest(req) => Ok(self.dispatch_close(req)),
            ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(req) => Ok(self.dispatch_query_directory(req)),
            ServerDriveIoRequest::DeviceReadRequest(req) => Ok(self.dispatch_read(req)),
            ServerDriveIoRequest::DeviceWriteRequest(req) => Ok(self.dispatch_write(req)),
            ServerDriveIoRequest::ServerDriveSetInformationRequest(req) => self.dispatch_set_information(req),

            // `IRP_MJ_DIRECTORY_CONTROL` / `IRP_MN_NOTIFY_CHANGE_DIRECTORY`: never answered,
            // matching FreeRDP's own drive redirection — the IRP is left pending until the
            // device (or session) tears down. `Ok(Vec::new())` is exactly the trait's own
            // "defer to `poll_deferred_messages`" contract, which this backend never populates
            // for this IRP.
            ServerDriveIoRequest::ServerDriveNotifyChangeDirectoryRequest(_req) => {
                debug!(
                    "[rdpdr-drive] OUT ServerDriveNotifyChangeDirectoryRequest completion_id={completion_id} device_id={device_id} status=<none, deferred per FreeRDP parity>"
                );
                Ok(Vec::new())
            }

            ServerDriveIoRequest::ServerDriveQueryVolumeInformationRequest(req) => {
                Ok(vec![query_volume_information_response(req)])
            }

            // `IRP_MJ_DEVICE_CONTROL`: no filesystem IOCTL this backend implements; ack empty
            // per the crib (matches FreeRDP's own default reply for unhandled control codes).
            ServerDriveIoRequest::DeviceControlRequest(req) => {
                log_outgoing("DeviceControlResponse", completion_id, device_id, NtStatus::SUCCESS);
                let response = DeviceControlResponse::new(req, NtStatus::SUCCESS, None);
                Ok(vec![SvcMessage::from(RdpdrPdu::DeviceControlResponse(response))])
            }

            // `IRP_MJ_FLUSH_BUFFERS`: every `DriveFs::write` already applies synchronously from
            // the server's perspective (there is no separate buffered-write stage to flush), so
            // this is an immediate acknowledgement, no `DriveFs` round-trip needed.
            ServerDriveIoRequest::DeviceFlushBuffersRequest(req) => {
                log_outgoing(
                    "DeviceFlushBuffersResponse",
                    completion_id,
                    device_id,
                    NtStatus::SUCCESS,
                );
                let response = DeviceFlushBuffersResponse {
                    device_io_response: DeviceIoResponse::new(req.device_io_request, NtStatus::SUCCESS),
                };
                Ok(vec![SvcMessage::from(RdpdrPdu::DeviceFlushBuffersResponse(response))])
            }

            // `IRP_MJ_LOCK_CONTROL`: no-op success, matching FreeRDP parity for a
            // browser-backed share (byte-range locking has no meaning without a real
            // multi-client filesystem to coordinate).
            ServerDriveIoRequest::ServerDriveLockControlRequest(req) => {
                log_outgoing(
                    "ClientDriveLockControlResponse",
                    completion_id,
                    device_id,
                    NtStatus::SUCCESS,
                );
                let response = ClientDriveLockControlResponse::new(req.device_io_request, NtStatus::SUCCESS);
                Ok(vec![SvcMessage::from(RdpdrPdu::ClientDriveLockControlResponse(
                    response,
                ))])
            }

            // `IRP_MJ_QUERY_SECURITY` / `IRP_MJ_SET_SECURITY`: `DriveFs` exposes no ACL
            // primitive, and `WasmDriveBackend::supports_drive_security` is left at its default
            // `false` accordingly, so the channel never advertises the capability — these two
            // stubs only guard against a server asking anyway.
            ServerDriveIoRequest::ServerDriveQuerySecurityRequest(req) => {
                log_outgoing(
                    "ClientDriveQuerySecurityResponse",
                    completion_id,
                    device_id,
                    NtStatus::NOT_SUPPORTED,
                );
                let response = ClientDriveQuerySecurityResponse {
                    device_io_response: DeviceIoResponse::new(req.device_io_request, NtStatus::NOT_SUPPORTED),
                    security_descriptor: None,
                };
                Ok(vec![SvcMessage::from(RdpdrPdu::ClientDriveQuerySecurityResponse(
                    response,
                ))])
            }
            ServerDriveIoRequest::ServerDriveSetSecurityRequest(req) => {
                log_outgoing(
                    "ClientDriveSetSecurityResponse",
                    completion_id,
                    device_id,
                    NtStatus::NOT_SUPPORTED,
                );
                let response = to_pdu_result(
                    "ClientDriveSetSecurityResponse",
                    ClientDriveSetSecurityResponse::new(&req, NtStatus::NOT_SUPPORTED),
                )?;
                Ok(vec![SvcMessage::from(RdpdrPdu::ClientDriveSetSecurityResponse(
                    response,
                ))])
            }
        }
    }

    fn handle_printer_io_request(&mut self, _req: PrinterIoRequest) -> PduResult<Vec<SvcMessage>> {
        Err(pdu_other_err!("printer IRP reached drive-only WasmDriveBackend"))
    }
}

/// Factory used by the session builder to construct a [`WasmDriveBackend`] for the wasm event
/// loop, supplying `wasm_bindgen_futures::spawn_local` as its spawner. `tx` carries this
/// backend's [`DriveBackendMessage`]s to whatever adapts them into the session's own event type
/// (`RdpInputEvent::DriveBackend`, wired up in `crate::session::SessionBuilder::connect`, which
/// forwards them onto the shared event channel; the backend itself is then handed to
/// [`super::composite::WasmCompositeBackend`] at that module's attach site in `session.rs`).
pub(crate) fn wasm_drive_pair(
    tx: mpsc::UnboundedSender<DriveBackendMessage>,
    fs: Rc<dyn DriveFs>,
    read_only: bool,
) -> WasmDriveBackend {
    let spawn: DriveFsSpawner = Rc::new(|future: LocalBoxFuture<'static, ()>| {
        wasm_bindgen_futures::spawn_local(future);
    });
    WasmDriveBackend::new(tx, fs, read_only, spawn)
}

/// Opens (or, for a creatable disposition, creates) a directory, returning `(None, true)` — no
/// `DriveFs` file handle applies to a directory (see `OpenEntry::fs_handle`'s doc comment).
async fn open_or_create_directory(
    fs: &dyn DriveFs,
    path: &str,
    creates_new: bool,
) -> Result<(Option<u32>, bool), FsError> {
    match fs.stat(path).await {
        Ok(entry) if entry.is_dir => Ok((None, true)),
        Ok(_file) => Err(FsError::Other("path exists and is not a directory".to_owned())),
        Err(FsError::NotFound) if creates_new => {
            fs.mkdir(path).await?;
            Ok((None, true))
        }
        Err(err) => Err(err),
    }
}

/// A `dispatch_create` failure that isn't a plain [`FsError`] — specifically, "the caller
/// explicitly asked for a non-directory and the target is one," which has no `FsError` variant
/// of its own since it isn't a filesystem-layer failure at all.
enum CreateOutcomeError {
    Fs(FsError),
    IsADirectory,
}

impl From<FsError> for CreateOutcomeError {
    fn from(err: FsError) -> Self {
        Self::Fs(err)
    }
}

fn create_status_for(err: &CreateOutcomeError) -> NtStatus {
    match err {
        CreateOutcomeError::Fs(err) => nt_status_for(err),
        CreateOutcomeError::IsADirectory => NtStatus::FILE_IS_A_DIRECTORY,
    }
}

/// Resolves a `Create` IRP into either a directory open/create or a file open/create,
/// independent of whether the server explicitly said which one it wanted.
///
/// `CreateOptions::FILE_DIRECTORY_FILE` is the only flag `DeviceCreateRequest` carries that
/// unambiguously means "this is a directory" — real Windows redirector traffic routinely opens
/// a path with NEITHER `FILE_DIRECTORY_FILE` nor `FILE_NON_DIRECTORY_FILE` set (observed live:
/// double-clicking a file on the share opens its *parent directory* this way as part of the
/// lookup), so treating "flag not set" as "must be a file" and unconditionally calling
/// `DriveFs::open_file` breaks against a real `DriveFs` implementation the moment the target
/// turns out to be a directory (the browser File System Access API throws `TypeMismatchError`
/// calling `getFileHandle` on a directory). So whenever the server hasn't explicitly committed
/// to "directory," this `stat`s first and decides from the real answer.
async fn resolve_create_outcome(
    fs: &dyn DriveFs,
    path: &str,
    is_directory: bool,
    must_not_be_directory: bool,
    write: bool,
    creates_new: bool,
    truncates: bool,
) -> Result<(Option<u32>, bool), CreateOutcomeError> {
    if is_directory {
        debug!("[rdpdr-drive] resolve_create_outcome path={path:?} branch=dir (explicit FILE_DIRECTORY_FILE)");
        return Ok(open_or_create_directory(fs, path, creates_new).await?);
    }

    match fs.stat(path).await {
        Ok(entry) if entry.is_dir => {
            if must_not_be_directory {
                // The server explicitly asked for a non-directory (`FILE_NON_DIRECTORY_FILE`)
                // and got one anyway.
                debug!("[rdpdr-drive] resolve_create_outcome path={path:?} branch=is-a-directory-error");
                Err(CreateOutcomeError::IsADirectory)
            } else {
                debug!("[rdpdr-drive] resolve_create_outcome path={path:?} branch=dir (stat-detected)");
                Ok((None, true))
            }
        }
        Ok(_) => {
            debug!("[rdpdr-drive] resolve_create_outcome path={path:?} branch=file");
            Ok(fs
                .open_file(path, write, creates_new, truncates)
                .await
                .map(|handle| (Some(handle), false))?)
        }
        // Doesn't exist yet: `open_file` itself creates it when the disposition allows, and
        // otherwise returns `FsError::NotFound` — exactly `NO_SUCH_FILE`, the status a
        // `FILE_OPEN` disposition against a missing path should produce either way.
        Err(FsError::NotFound) if creates_new => {
            debug!("[rdpdr-drive] resolve_create_outcome path={path:?} branch=notfound-create");
            Ok(fs
                .open_file(path, write, creates_new, truncates)
                .await
                .map(|handle| (Some(handle), false))?)
        }
        Err(FsError::NotFound) => {
            debug!("[rdpdr-drive] resolve_create_outcome path={path:?} branch=notfound-nosuchfile");
            Ok(fs
                .open_file(path, write, creates_new, truncates)
                .await
                .map(|handle| (Some(handle), false))?)
        }
        Err(err) => {
            debug!("[rdpdr-drive] resolve_create_outcome path={path:?} branch=stat-error err={err:?}");
            Err(err.into())
        }
    }
}

/// Diagnostic instrumentation (kept, not removed after debugging): extracts `(variant name,
/// device_id, completion_id, file_id)` from any [`ServerDriveIoRequest`], for the `[rdpdr-drive]
/// IN` log line at the top of `handle_drive_io_request`. Named `device_io_request` on every
/// variant except `DeviceControlRequest`, which names it `header` — see `ironrdp-rdpdr`'s
/// `efs.rs`.
fn drive_request_debug_info(req: &ServerDriveIoRequest) -> (&'static str, u32, u32, u32) {
    fn parts(io: &ironrdp::rdpdr::pdu::efs::DeviceIoRequest) -> (u32, u32, u32) {
        (io.device_id, io.completion_id, io.file_id)
    }
    match req {
        ServerDriveIoRequest::ServerCreateDriveRequest(r) => {
            let (d, c, f) = parts(&r.device_io_request);
            ("ServerCreateDriveRequest", d, c, f)
        }
        ServerDriveIoRequest::ServerDriveQueryInformationRequest(r) => {
            let (d, c, f) = parts(&r.device_io_request);
            ("ServerDriveQueryInformationRequest", d, c, f)
        }
        ServerDriveIoRequest::DeviceCloseRequest(r) => {
            let (d, c, f) = parts(&r.device_io_request);
            ("DeviceCloseRequest", d, c, f)
        }
        ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(r) => {
            let (d, c, f) = parts(&r.device_io_request);
            ("ServerDriveQueryDirectoryRequest", d, c, f)
        }
        ServerDriveIoRequest::ServerDriveNotifyChangeDirectoryRequest(r) => {
            let (d, c, f) = parts(&r.device_io_request);
            ("ServerDriveNotifyChangeDirectoryRequest", d, c, f)
        }
        ServerDriveIoRequest::ServerDriveQueryVolumeInformationRequest(r) => {
            let (d, c, f) = parts(&r.device_io_request);
            ("ServerDriveQueryVolumeInformationRequest", d, c, f)
        }
        ServerDriveIoRequest::DeviceControlRequest(r) => {
            let (d, c, f) = parts(&r.header);
            ("DeviceControlRequest", d, c, f)
        }
        ServerDriveIoRequest::DeviceReadRequest(r) => {
            let (d, c, f) = parts(&r.device_io_request);
            ("DeviceReadRequest", d, c, f)
        }
        ServerDriveIoRequest::DeviceWriteRequest(r) => {
            let (d, c, f) = parts(&r.device_io_request);
            ("DeviceWriteRequest", d, c, f)
        }
        ServerDriveIoRequest::DeviceFlushBuffersRequest(r) => {
            let (d, c, f) = parts(&r.device_io_request);
            ("DeviceFlushBuffersRequest", d, c, f)
        }
        ServerDriveIoRequest::ServerDriveSetInformationRequest(r) => {
            let (d, c, f) = parts(&r.device_io_request);
            ("ServerDriveSetInformationRequest", d, c, f)
        }
        ServerDriveIoRequest::ServerDriveLockControlRequest(r) => {
            let (d, c, f) = parts(&r.device_io_request);
            ("ServerDriveLockControlRequest", d, c, f)
        }
        ServerDriveIoRequest::ServerDriveQuerySecurityRequest(r) => {
            let (d, c, f) = parts(&r.device_io_request);
            ("ServerDriveQuerySecurityRequest", d, c, f)
        }
        ServerDriveIoRequest::ServerDriveSetSecurityRequest(r) => {
            let (d, c, f) = parts(&r.device_io_request);
            ("ServerDriveSetSecurityRequest", d, c, f)
        }
    }
}

/// Diagnostic instrumentation (kept, not removed after debugging): logs one line per RDPDR
/// completion this backend sends, at the point each is constructed — variant name, the
/// `completion_id`/`device_id` it echoes, and the `NtStatus` decided for it.
fn log_outgoing(variant: &'static str, completion_id: u32, device_id: u32, status: NtStatus) {
    debug!("[rdpdr-drive] OUT {variant} completion_id={completion_id} device_id={device_id} status={status:?}");
}

fn send_completion(tx: &mpsc::UnboundedSender<DriveBackendMessage>, messages: Vec<SvcMessage>) {
    debug!("[rdpdr-drive] send_completion delivering {} message(s)", messages.len());
    if tx.unbounded_send(DriveBackendMessage::IoCompleted(messages)).is_err() {
        warn!("Failed to deliver drive IRP completion; event loop receiver is closed");
    }
}

/// Same as [`send_completion`], but first checks that `generation` (the backend's live
/// generation counter) still matches `spawned_generation` (the value captured when the future
/// now calling this was spawned). A mismatch means [`WasmDriveBackend::reset`] ran in the
/// meantime — the RDPDR sequence that IRP belonged to is gone, so its completion is dropped
/// rather than delivered with a `device_id`/`completion_id` that no longer means anything.
fn send_completion_if_current(
    tx: &mpsc::UnboundedSender<DriveBackendMessage>,
    generation: &Cell<u64>,
    spawned_generation: u64,
    messages: Vec<SvcMessage>,
) {
    if generation.get() != spawned_generation {
        debug!(
            "[rdpdr-drive] send_completion_if_current DROPPED stale-generation completion (spawned_generation={spawned_generation} current_generation={})",
            generation.get()
        );
        trace!("Dropping drive IRP completion from an RDPDR sequence superseded by reset()");
        return;
    }
    send_completion(tx, messages);
}

/// `state.rs`'s own docs promise every drive-IRP path reaches `DriveFs` only after
/// `normalize_path` validation; this is the one call site that promise is upheld from, since
/// `DriveFs` implementations are only required to *interpret* an already-validated path, not
/// necessarily re-validate it themselves (`MockFs` happens to, but that's not a contract).
fn path_is_valid(path: &str) -> bool {
    normalize_path(path).is_ok()
}

fn nt_status_for(err: &FsError) -> NtStatus {
    match err {
        FsError::NotFound => NtStatus::NO_SUCH_FILE,
        FsError::AccessDenied => NtStatus::ACCESS_DENIED,
        FsError::Other(_) => NtStatus::UNSUCCESSFUL,
    }
}

/// Write-intent per the controller ruling: either a write-capable `DesiredAccess` bit, or a
/// `CreateDisposition` that can create or overwrite (anything other than `FILE_OPEN`).
fn is_write_intent(create: &DeviceCreateRequest) -> bool {
    let write_bits = DesiredAccess::FILE_WRITE_DATA_OR_FILE_ADD_FILE
        | DesiredAccess::FILE_APPEND_DATA_OR_FILE_ADD_SUBDIRECTORY
        | DesiredAccess::FILE_WRITE_EA
        | DesiredAccess::FILE_WRITE_ATTRIBUTES
        | DesiredAccess::DELETE
        | DesiredAccess::GENERIC_WRITE
        | DesiredAccess::GENERIC_ALL;
    create.desired_access.intersects(write_bits) || !matches!(create.create_disposition, CreateDisposition::FILE_OPEN)
}

/// Milliseconds between the NT epoch (1601-01-01) and the Unix epoch (1970-01-01).
const UNIX_TO_NT_EPOCH_OFFSET_MS: i64 = 11_644_473_600_000;

/// Converts a Unix-epoch millisecond timestamp (as carried by [`FsEntry::last_modified_ms`])
/// into an NT time (100ns intervals since 1601-01-01).
fn nt_time_from_unix_ms(unix_ms: f64) -> i64 {
    #[expect(clippy::as_conversions, clippy::cast_possible_truncation)]
    let ms = unix_ms as i64;
    ms.saturating_add(UNIX_TO_NT_EPOCH_OFFSET_MS).saturating_mul(10_000)
}

/// `FsEntry` only carries one timestamp; per the controller ruling it stands in for all four NT
/// times (creation/access/write/change).
fn nt_times(entry: &FsEntry) -> (i64, i64, i64, i64) {
    let t = nt_time_from_unix_ms(entry.last_modified_ms);
    (t, t, t, t)
}

fn attrs_for(is_dir: bool) -> FileAttributes {
    if is_dir {
        FileAttributes::FILE_ATTRIBUTE_DIRECTORY
    } else {
        FileAttributes::FILE_ATTRIBUTE_NORMAL
    }
}

fn entry_size_i64(entry: &FsEntry) -> i64 {
    i64::try_from(entry.size).unwrap_or(i64::MAX)
}

/// Builds a `ClientDriveQueryInformationResponse` buffer for the classes this backend supports.
/// `None` means "class not supported" (caller maps that to `NtStatus::NOT_SUPPORTED`).
fn build_query_info(class_lvl: &FileInformationClassLevel, entry: &FsEntry) -> Option<FileInformationClass> {
    if *class_lvl == FileInformationClassLevel::FILE_BASIC_INFORMATION {
        let (creation_time, last_access_time, last_write_time, change_time) = nt_times(entry);
        Some(
            FileBasicInformation {
                creation_time,
                last_access_time,
                last_write_time,
                change_time,
                file_attributes: attrs_for(entry.is_dir),
            }
            .into(),
        )
    } else if *class_lvl == FileInformationClassLevel::FILE_STANDARD_INFORMATION {
        let size = entry_size_i64(entry);
        Some(
            FileStandardInformation {
                allocation_size: size,
                end_of_file: size,
                number_of_links: 1,
                delete_pending: Boolean::False,
                directory: if entry.is_dir { Boolean::True } else { Boolean::False },
            }
            .into(),
        )
    } else if *class_lvl == FileInformationClassLevel::FILE_ATTRIBUTE_TAG_INFORMATION {
        // Windows queries this on the source file during Explorer copy/move operations;
        // FreeRDP's own drive backend answers exactly three query classes — Basic, Standard,
        // and this one — so leaving it `NOT_SUPPORTED` (the `None` fallback below) aborts the
        // copy and, worse, tears down the whole redirected share for the rest of the session.
        // We never expose reparse points, so `reparse_tag` is always 0.
        Some(
            FileAttributeTagInformation {
                file_attributes: attrs_for(entry.is_dir),
                reparse_tag: 0,
            }
            .into(),
        )
    } else {
        None
    }
}

/// Extracts the search pattern from a `QueryDirectory` initial-query path, e.g. `\dir\*.txt` ->
/// `Some("*.txt")`, `\dir\report.pdf` -> `Some("report.pdf")` (an exact-name existence check —
/// Windows issues these for Save-As overwrite prompts and rename-collision checks).
/// `None` means "no filter" (bare `*`, or an empty path — both mean "list everything").
fn dir_search_pattern(path: &str) -> Option<String> {
    let pattern = path.rsplit(['\\', '/']).next().unwrap_or(path);
    if pattern.is_empty() || pattern == "*" {
        None
    } else {
        Some(pattern.to_owned())
    }
}

/// DOS/Windows wildcard match (`*` = any run of characters including none, `?` = exactly one
/// character), case-insensitive — the classic greedy two-pointer algorithm (iterative, so an
/// adversarial pattern can't blow the stack the way a naive recursive matcher could).
fn dos_wildcard_match(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().map(|c| c.to_ascii_lowercase()).collect();
    let name: Vec<char> = name.chars().map(|c| c.to_ascii_lowercase()).collect();

    let mut p = 0usize;
    let mut n = 0usize;
    let mut star_p: Option<usize> = None;
    let mut star_n = 0usize;

    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star_p = Some(p);
            star_n = n;
            p += 1;
        } else if let Some(sp) = star_p {
            p = sp + 1;
            star_n += 1;
            n = star_n;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// Builds a directory-listing entry in whichever of the four levels
/// `ServerDriveQueryDirectoryRequest::decode` accepts (`Both`/`Full`/`Directory`/`Names`) the
/// server asked for.
fn build_dir_info(class_lvl: &FileInformationClassLevel, entry: &FsEntry) -> FileInformationClass {
    let (creation_time, last_access_time, last_write_time, change_time) = nt_times(entry);
    let attrs = attrs_for(entry.is_dir);
    let size = entry_size_i64(entry);
    let name = entry.name.clone();

    if *class_lvl == FileInformationClassLevel::FILE_BOTH_DIRECTORY_INFORMATION {
        FileBothDirectoryInformation::new(
            creation_time,
            last_access_time,
            last_write_time,
            change_time,
            size,
            attrs,
            name,
        )
        .into()
    } else if *class_lvl == FileInformationClassLevel::FILE_FULL_DIRECTORY_INFORMATION {
        FileFullDirectoryInformation::new(
            creation_time,
            last_access_time,
            last_write_time,
            change_time,
            size,
            attrs,
            name,
        )
        .into()
    } else if *class_lvl == FileInformationClassLevel::FILE_DIRECTORY_INFORMATION {
        FileDirectoryInformation::new(
            creation_time,
            last_access_time,
            last_write_time,
            change_time,
            size,
            attrs,
            name,
        )
        .into()
    } else {
        // The decode-side match in `ServerDriveQueryDirectoryRequest::decode` restricts
        // `file_info_class_lvl` to exactly these four values, so reaching here means
        // `FILE_NAMES_INFORMATION`.
        FileNamesInformation::new(name).into()
    }
}

/// Builds a `ClientDriveQueryVolumeInformationResponse` for the five levels
/// `ServerDriveQueryVolumeInformationRequest::decode` accepts, with fixed, plausible values —
/// `DriveFs` (Task 1's scope) exposes no free/total space primitive, so this reports a large
/// fake volume rather than refusing writes for apparent lack of space.
fn build_volume_info(fs_info_class_lvl: &FileSystemInformationClassLevel) -> Option<FileSystemInformationClass> {
    const BYTES_PER_SECTOR: u32 = 512;
    const SECTORS_PER_ALLOC_UNIT: u32 = 8; // 4 KiB clusters
    const TOTAL_ALLOC_UNITS: i64 = i64::MAX / 4096;

    if *fs_info_class_lvl == FileSystemInformationClassLevel::FILE_FS_VOLUME_INFORMATION {
        Some(
            FileFsVolumeInformation {
                volume_creation_time: 0,
                volume_serial_number: 0x0001_0000,
                supports_objects: Boolean::False,
                volume_label: String::new(),
            }
            .into(),
        )
    } else if *fs_info_class_lvl == FileSystemInformationClassLevel::FILE_FS_SIZE_INFORMATION {
        Some(
            FileFsSizeInformation {
                total_alloc_units: TOTAL_ALLOC_UNITS,
                available_alloc_units: TOTAL_ALLOC_UNITS,
                sectors_per_alloc_unit: SECTORS_PER_ALLOC_UNIT,
                bytes_per_sector: BYTES_PER_SECTOR,
            }
            .into(),
        )
    } else if *fs_info_class_lvl == FileSystemInformationClassLevel::FILE_FS_FULL_SIZE_INFORMATION {
        Some(
            FileFsFullSizeInformation {
                total_alloc_units: TOTAL_ALLOC_UNITS,
                caller_available_alloc_units: TOTAL_ALLOC_UNITS,
                actual_available_alloc_units: TOTAL_ALLOC_UNITS,
                sectors_per_alloc_unit: SECTORS_PER_ALLOC_UNIT,
                bytes_per_sector: BYTES_PER_SECTOR,
            }
            .into(),
        )
    } else if *fs_info_class_lvl == FileSystemInformationClassLevel::FILE_FS_ATTRIBUTE_INFORMATION {
        Some(
            FileFsAttributeInformation {
                file_system_attributes: FileSystemAttributes::FILE_UNICODE_ON_DISK
                    | FileSystemAttributes::FILE_CASE_PRESERVED_NAMES,
                max_component_name_len: 255,
                file_system_name: "WEBSHARE".to_owned(),
            }
            .into(),
        )
    } else if *fs_info_class_lvl == FileSystemInformationClassLevel::FILE_FS_DEVICE_INFORMATION {
        Some(
            FileFsDeviceInformation {
                device_type: 0x0000_0007, // FILE_DEVICE_DISK, MS-FSCC 2.5.10
                characteristics: Characteristics::empty(),
            }
            .into(),
        )
    } else {
        None
    }
}

fn query_volume_information_response(req: ServerDriveQueryVolumeInformationRequest) -> SvcMessage {
    let buffer = build_volume_info(&req.fs_info_class_lvl);
    let status = if buffer.is_some() {
        NtStatus::SUCCESS
    } else {
        NtStatus::NOT_SUPPORTED
    };
    log_outgoing(
        "ClientDriveQueryVolumeInformationResponse",
        req.device_io_request.completion_id,
        req.device_io_request.device_id,
        status,
    );
    let response = ClientDriveQueryVolumeInformationResponse::new(req.device_io_request, status, buffer);
    SvcMessage::from(RdpdrPdu::ClientDriveQueryVolumeInformationResponse(response))
}

fn set_information_message(req: &ServerDriveSetInformationRequest, status: NtStatus) -> PduResult<SvcMessage> {
    let response = to_pdu_result(
        "ClientDriveSetInformationResponse",
        ClientDriveSetInformationResponse::new(req, status),
    )?;
    Ok(SvcMessage::from(RdpdrPdu::ClientDriveSetInformationResponse(response)))
}

/// Fallback used when [`set_information_message`] itself fails to encode — its `Length` field
/// is computed from `req.set_buffer.size()` via `cast_length!`, so a failure there means that
/// exact computation can't be retried with the same `set_buffer`. Substitutes a
/// `FileDispositionInformation` (a fixed 1-byte payload, so `cast_length!` always succeeds)
/// while still echoing the real `device_io_request` — an IRP is always answered, never dropped,
/// even when the "real" response can't be built.
fn set_information_fallback_message(req: &ServerDriveSetInformationRequest, status: NtStatus) -> SvcMessage {
    let fallback = ServerDriveSetInformationRequest {
        device_io_request: req.device_io_request.clone(),
        set_buffer: FileInformationClass::Disposition(FileDispositionInformation { delete_pending: 0 }),
    };
    let response = ClientDriveSetInformationResponse::new(&fallback, status)
        .expect("FileDispositionInformation's fixed 1-byte size always fits a u32 length field");
    SvcMessage::from(RdpdrPdu::ClientDriveSetInformationResponse(response))
}

/// Converts a fallible PDU-construction result (`cast_length!`'s `EncodeError`, e.g. a
/// file name too long to fit a `u32`-length-prefixed wire field) into this backend's
/// `PduResult`. In practice this only ever fires for pathologically large inputs.
fn to_pdu_result<T>(context: &'static str, result: EncodeResult<T>) -> PduResult<T> {
    result.map_err(|err| PduError::encode(context, err))
}

#[cfg(test)]
mod tests {
    use futures::executor::LocalPool;
    use futures::task::LocalSpawnExt;
    use ironrdp::rdpdr::pdu::efs::{
        DeviceIoRequest, FileRenameInformation, MajorFunction, MinorFunction, SharedAccess,
    };

    use super::*;
    use crate::drive::fs::MockFs;

    const DEVICE_ID: u32 = 7;

    fn dev_io_req(file_id: u32, completion_id: u32, major_function: MajorFunction) -> DeviceIoRequest {
        DeviceIoRequest {
            device_id: DEVICE_ID,
            file_id,
            completion_id,
            major_function,
            minor_function: MinorFunction::from(0),
        }
    }

    fn create_request(
        path: &str,
        completion_id: u32,
        desired_access: DesiredAccess,
        disposition: CreateDisposition,
    ) -> DeviceCreateRequest {
        DeviceCreateRequest {
            device_io_request: dev_io_req(0, completion_id, MajorFunction::Create),
            desired_access,
            allocation_size: 0,
            file_attributes: FileAttributes::empty(),
            shared_access: SharedAccess::empty(),
            create_disposition: disposition,
            create_options: CreateOptions::empty(),
            path: path.to_string(),
        }
    }

    fn open_request(path: &str, completion_id: u32) -> DeviceCreateRequest {
        create_request(
            path,
            completion_id,
            DesiredAccess::FILE_READ_DATA_OR_FILE_LIST_DIRECTORY,
            CreateDisposition::FILE_OPEN,
        )
    }

    /// Backend + channel + a `LocalPool` that must be pumped (`run_until_stalled`) for any
    /// spawned `DriveFs` future to actually resolve and send its completion.
    struct Harness {
        backend: WasmDriveBackend,
        rx: mpsc::UnboundedReceiver<DriveBackendMessage>,
        pool: LocalPool,
    }

    impl Harness {
        fn new(fs: Rc<MockFs>, read_only: bool) -> Self {
            let (tx, rx) = mpsc::unbounded();
            let pool = LocalPool::new();
            let spawner = pool.spawner();
            let spawn: DriveFsSpawner = Rc::new(move |future| {
                spawner.spawn_local(future).expect("spawn_local must succeed in tests");
            });
            let backend = WasmDriveBackend::new(tx, fs, read_only, spawn);
            Self { backend, rx, pool }
        }

        fn dispatch(&mut self, req: ServerDriveIoRequest) -> Vec<SvcMessage> {
            let mut messages = self
                .backend
                .handle_drive_io_request(req)
                .expect("dispatch must not error");
            self.pool.run_until_stalled();
            while let Ok(DriveBackendMessage::IoCompleted(more)) = self.rx.try_recv() {
                messages.extend(more);
            }
            messages
        }

        fn assert_no_completion(&mut self) {
            self.pool.run_until_stalled();
            assert!(self.rx.try_recv().is_err(), "expected no queued completion");
        }
    }

    fn encoded(message: &SvcMessage) -> Vec<u8> {
        message.encode_unframed_pdu().expect("response must encode")
    }

    fn read_u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    // Every drive completion PDU here shares the same 16-byte prefix: a 4-byte RDPDR
    // `SharedHeader` followed by a 12-byte `DeviceIoResponse` (device_id, completion_id,
    // io_status) — see `ironrdp_rdpdr::pdu::efs::DeviceIoResponse`.
    fn completion_id_of(message: &SvcMessage) -> u32 {
        read_u32_at(&encoded(message), 8)
    }

    fn status_of(message: &SvcMessage) -> NtStatus {
        NtStatus::from(read_u32_at(&encoded(message), 12))
    }

    fn only(mut messages: Vec<SvcMessage>) -> SvcMessage {
        assert_eq!(messages.len(), 1, "expected exactly one completion PDU");
        messages.remove(0)
    }

    /// Decodes the `FileName` out of a `ClientDriveQueryDirectoryResponse` built from
    /// `FileNamesInformation` (as every `QueryDirectory` test here requests): 16-byte
    /// `SharedHeader` + `DeviceIoResponse` prefix, then `Length`(4) +
    /// `NextEntryOffset`(4) + `FileIndex`(4) + `FileNameLength`(4) + `FileName` (UTF-16LE, no
    /// null terminator) — see `FileNamesInformation::encode`.
    fn dir_entry_name(message: &SvcMessage) -> String {
        let bytes = encoded(message);
        let file_name_length = read_u32_at(&bytes, 28) as usize;
        let utf16: Vec<u16> = bytes[32..32 + file_name_length]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16_lossy(&utf16)
    }

    /// `MockFs`'s futures never actually suspend (no wasm I/O involved), so a single poll always
    /// resolves them — mirrors the identically-named helper in `fs.rs`'s own test module, which
    /// is private to that module and so not reusable here.
    fn block_on<F: Future>(future: F) -> F::Output {
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        match future.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(output) => output,
            std::task::Poll::Pending => panic!("MockFs futures resolve synchronously; got Pending"),
        }
    }

    /// Reads back a whole file directly through `DriveFs`, bypassing the backend entirely — used
    /// to assert on-disk contents independent of whatever the backend reported.
    fn read_all_via_fs(fs: &MockFs, path: &str) -> Vec<u8> {
        block_on(async {
            let entry = fs.stat(path).await.expect("path must exist");
            let handle = fs
                .open_file(path, false, false, false)
                .await
                .expect("path must open for read");
            let data = fs
                .read(
                    handle,
                    0,
                    u32::try_from(entry.size).expect("test fixture size fits u32"),
                )
                .await
                .expect("read must succeed");
            fs.close(handle).await.expect("close must succeed");
            data
        })
    }

    #[test]
    fn create_query_read_close_happy_path_echoes_completion_ids() {
        let fs = Rc::new(MockFs::new());
        fs.seed_file("\\hello.txt", b"hello world");
        let mut harness = Harness::new(fs, false);

        let create = only(
            harness.dispatch(ServerDriveIoRequest::ServerCreateDriveRequest(open_request(
                "\\hello.txt",
                1,
            ))),
        );
        assert_eq!(completion_id_of(&create), 1);
        assert_eq!(status_of(&create), NtStatus::SUCCESS);
        let file_id = read_u32_at(&encoded(&create), 16);
        assert_ne!(file_id, 0);

        let query = only(
            harness.dispatch(ServerDriveIoRequest::ServerDriveQueryInformationRequest(
                ServerDriveQueryInformationRequest {
                    device_io_request: dev_io_req(file_id, 2, MajorFunction::QueryInformation),
                    file_info_class_lvl: FileInformationClassLevel::FILE_STANDARD_INFORMATION,
                },
            )),
        );
        assert_eq!(completion_id_of(&query), 2);
        assert_eq!(status_of(&query), NtStatus::SUCCESS);

        let read = only(
            harness.dispatch(ServerDriveIoRequest::DeviceReadRequest(DeviceReadRequest {
                device_io_request: dev_io_req(file_id, 3, MajorFunction::Read),
                length: 32,
                offset: 0,
            })),
        );
        assert_eq!(completion_id_of(&read), 3);
        assert_eq!(status_of(&read), NtStatus::SUCCESS);
        let read_bytes = encoded(&read);
        let data_len = read_u32_at(&read_bytes, 16) as usize;
        assert_eq!(&read_bytes[20..20 + data_len], b"hello world");

        let close = only(
            harness.dispatch(ServerDriveIoRequest::DeviceCloseRequest(DeviceCloseRequest {
                device_io_request: dev_io_req(file_id, 4, MajorFunction::Close),
            })),
        );
        assert_eq!(completion_id_of(&close), 4);
        assert_eq!(status_of(&close), NtStatus::SUCCESS);
    }

    #[test]
    fn query_directory_initial_then_iterate_to_no_more_files() {
        let fs = Rc::new(MockFs::new());
        fs.seed_file("\\dir\\a.txt", b"a");
        fs.seed_file("\\dir\\b.txt", b"bb");
        let mut harness = Harness::new(fs, false);

        let mut create = open_request("\\dir", 1);
        create.create_options = CreateOptions::FILE_DIRECTORY_FILE;
        let created = only(harness.dispatch(ServerDriveIoRequest::ServerCreateDriveRequest(create)));
        assert_eq!(status_of(&created), NtStatus::SUCCESS);
        let file_id = read_u32_at(&encoded(&created), 16);

        let initial = only(harness.dispatch(ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(
            ServerDriveQueryDirectoryRequest {
                device_io_request: dev_io_req(file_id, 2, MajorFunction::DirectoryControl),
                file_info_class_lvl: FileInformationClassLevel::FILE_NAMES_INFORMATION,
                initial_query: 1,
                path: "\\dir\\*".to_string(),
            },
        )));
        assert_eq!(status_of(&initial), NtStatus::SUCCESS);

        let second = only(harness.dispatch(ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(
            ServerDriveQueryDirectoryRequest {
                device_io_request: dev_io_req(file_id, 3, MajorFunction::DirectoryControl),
                file_info_class_lvl: FileInformationClassLevel::FILE_NAMES_INFORMATION,
                initial_query: 0,
                path: String::new(),
            },
        )));
        assert_eq!(status_of(&second), NtStatus::SUCCESS);

        let exhausted = only(harness.dispatch(ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(
            ServerDriveQueryDirectoryRequest {
                device_io_request: dev_io_req(file_id, 4, MajorFunction::DirectoryControl),
                file_info_class_lvl: FileInformationClassLevel::FILE_NAMES_INFORMATION,
                initial_query: 0,
                path: String::new(),
            },
        )));
        assert_eq!(status_of(&exhausted), NtStatus::NO_MORE_FILES);
    }

    #[test]
    fn read_only_backend_denies_write_with_access_denied_and_does_not_touch_fs() {
        let fs = Rc::new(MockFs::new());
        fs.seed_file("\\ro.txt", b"immutable");
        let mut harness = Harness::new(Rc::clone(&fs), true);

        let created = only(
            harness.dispatch(ServerDriveIoRequest::ServerCreateDriveRequest(open_request(
                "\\ro.txt", 1,
            ))),
        );
        let file_id = read_u32_at(&encoded(&created), 16);

        let write = only(
            harness.dispatch(ServerDriveIoRequest::DeviceWriteRequest(DeviceWriteRequest {
                device_io_request: dev_io_req(file_id, 2, MajorFunction::Write),
                offset: 0,
                write_data: b"changed".to_vec(),
            })),
        );
        assert_eq!(status_of(&write), NtStatus::ACCESS_DENIED);
        assert_eq!(read_u32_at(&encoded(&write), 16), 0, "length must be 0 on denial");
        harness.assert_no_completion();

        // File on disk must be untouched.
        assert_eq!(read_all_via_fs(&fs, "\\ro.txt"), b"immutable");
    }

    #[test]
    fn read_only_backend_denies_write_intent_create_without_spawning() {
        let fs = Rc::new(MockFs::new());
        let mut harness = Harness::new(fs, true);

        let create = create_request(
            "\\new.txt",
            1,
            DesiredAccess::FILE_WRITE_DATA_OR_FILE_ADD_FILE,
            CreateDisposition::FILE_CREATE,
        );
        let response = only(harness.dispatch(ServerDriveIoRequest::ServerCreateDriveRequest(create)));
        assert_eq!(status_of(&response), NtStatus::ACCESS_DENIED);
        assert_eq!(read_u32_at(&encoded(&response), 16), 0);
        harness.assert_no_completion();
    }

    #[test]
    fn read_only_backend_denies_set_information() {
        let fs = Rc::new(MockFs::new());
        fs.seed_file("\\ro.txt", b"data");
        let mut harness = Harness::new(fs, true);

        let created = only(
            harness.dispatch(ServerDriveIoRequest::ServerCreateDriveRequest(open_request(
                "\\ro.txt", 1,
            ))),
        );
        let file_id = read_u32_at(&encoded(&created), 16);

        let response = only(harness.dispatch(ServerDriveIoRequest::ServerDriveSetInformationRequest(
            ServerDriveSetInformationRequest {
                device_io_request: dev_io_req(file_id, 2, MajorFunction::SetInformation),
                set_buffer: FileInformationClass::Disposition(FileDispositionInformation { delete_pending: 1 }),
            },
        )));
        assert_eq!(status_of(&response), NtStatus::ACCESS_DENIED);
        harness.assert_no_completion();
    }

    #[test]
    fn set_information_rename_updates_open_entry_path() {
        let fs = Rc::new(MockFs::new());
        fs.seed_file("\\old.txt", b"payload");
        let mut harness = Harness::new(Rc::clone(&fs), false);

        let created = only(
            harness.dispatch(ServerDriveIoRequest::ServerCreateDriveRequest(open_request(
                "\\old.txt",
                1,
            ))),
        );
        let file_id = read_u32_at(&encoded(&created), 16);

        let renamed = only(harness.dispatch(ServerDriveIoRequest::ServerDriveSetInformationRequest(
            ServerDriveSetInformationRequest {
                device_io_request: dev_io_req(file_id, 2, MajorFunction::SetInformation),
                set_buffer: FileInformationClass::Rename(FileRenameInformation {
                    replace_if_exists: Boolean::False,
                    file_name: "\\new.txt".to_string(),
                }),
            },
        )));
        assert_eq!(status_of(&renamed), NtStatus::SUCCESS);
        assert_eq!(read_all_via_fs(&fs, "\\new.txt"), b"payload");

        // A subsequent QueryInformation against the same `file_id` must resolve the NEW path,
        // proving `DriveState`'s open-entry path followed the rename. (Read/Write instead use
        // the raw `DriveFs` handle allocated at `open_file` time, which `MockFs` documents does
        // NOT retarget on rename — see the `MockFs` caveat in this crate's `fs.rs` — so this
        // deliberately exercises the state-layer path update rather than the handle.)
        let query = only(
            harness.dispatch(ServerDriveIoRequest::ServerDriveQueryInformationRequest(
                ServerDriveQueryInformationRequest {
                    device_io_request: dev_io_req(file_id, 3, MajorFunction::QueryInformation),
                    file_info_class_lvl: FileInformationClassLevel::FILE_STANDARD_INFORMATION,
                },
            )),
        );
        assert_eq!(status_of(&query), NtStatus::SUCCESS);
    }

    #[test]
    fn notify_change_directory_returns_no_response() {
        let fs = Rc::new(MockFs::new());
        let mut harness = Harness::new(fs, false);

        let messages = harness.dispatch(ServerDriveIoRequest::ServerDriveNotifyChangeDirectoryRequest(
            ironrdp::rdpdr::pdu::efs::ServerDriveNotifyChangeDirectoryRequest {
                device_io_request: dev_io_req(1, 1, MajorFunction::DirectoryControl),
                watch_tree: 0,
                completion_filter: 0,
            },
        ));
        assert!(messages.is_empty());
        harness.assert_no_completion();
    }

    #[test]
    fn lock_control_is_success_noop() {
        let fs = Rc::new(MockFs::new());
        let mut harness = Harness::new(fs, false);

        let response = only(harness.dispatch(ServerDriveIoRequest::ServerDriveLockControlRequest(
            ironrdp::rdpdr::pdu::efs::ServerDriveLockControlRequest {
                device_io_request: dev_io_req(1, 1, MajorFunction::LockControl),
                operation: ironrdp::rdpdr::pdu::efs::LockOperation::Shared,
                wait: false,
                locks: Vec::new(),
            },
        )));
        assert_eq!(status_of(&response), NtStatus::SUCCESS);
    }

    #[test]
    fn query_security_is_not_supported() {
        let fs = Rc::new(MockFs::new());
        let mut harness = Harness::new(fs, false);

        let response = only(harness.dispatch(ServerDriveIoRequest::ServerDriveQuerySecurityRequest(
            ironrdp::rdpdr::pdu::efs::ServerDriveQuerySecurityRequest {
                device_io_request: dev_io_req(1, 1, MajorFunction::QuerySecurity),
                security_information: ironrdp::rdpdr::pdu::efs::SecurityInformation::empty(),
            },
        )));
        assert_eq!(status_of(&response), NtStatus::NOT_SUPPORTED);
    }

    #[test]
    fn device_control_is_success_empty() {
        let fs = Rc::new(MockFs::new());
        let mut harness = Harness::new(fs, false);

        let response = only(harness.dispatch(ServerDriveIoRequest::DeviceControlRequest(
            ironrdp::rdpdr::pdu::efs::DeviceControlRequest {
                header: dev_io_req(1, 1, MajorFunction::DeviceControl),
                output_buffer_length: 0,
                input_buffer_length: 0,
                io_control_code: ironrdp::rdpdr::pdu::efs::AnyIoCtlCode(0),
            },
        )));
        assert_eq!(status_of(&response), NtStatus::SUCCESS);
        assert_eq!(read_u32_at(&encoded(&response), 16), 0, "OutputBufferLength must be 0");
    }

    #[test]
    fn close_of_unknown_file_id_is_invalid_handle() {
        let fs = Rc::new(MockFs::new());
        let mut harness = Harness::new(fs, false);

        let response = only(
            harness.dispatch(ServerDriveIoRequest::DeviceCloseRequest(DeviceCloseRequest {
                device_io_request: dev_io_req(99, 1, MajorFunction::Close),
            })),
        );
        assert_eq!(status_of(&response), NtStatus::INVALID_HANDLE);
    }

    #[test]
    fn query_information_unknown_file_id_is_invalid_handle() {
        let fs = Rc::new(MockFs::new());
        let mut harness = Harness::new(fs, false);

        let response = only(
            harness.dispatch(ServerDriveIoRequest::ServerDriveQueryInformationRequest(
                ServerDriveQueryInformationRequest {
                    device_io_request: dev_io_req(99, 1, MajorFunction::QueryInformation),
                    file_info_class_lvl: FileInformationClassLevel::FILE_BASIC_INFORMATION,
                },
            )),
        );
        assert_eq!(status_of(&response), NtStatus::INVALID_HANDLE);
    }

    #[test]
    fn query_information_file_attribute_tag_returns_success_with_decodable_buffer() {
        // Regression test: Windows queries FileAttributeTagInformation on the source file
        // during Explorer copy/move operations (FreeRDP's own drive backend answers exactly
        // three query classes: Basic, Standard, and this one). Before this fix, this class
        // fell through to the NOT_SUPPORTED default — the one non-SUCCESS status observed in
        // a live capture — which aborted the copy AND tore down the whole share for the rest
        // of the session.
        let fs = Rc::new(MockFs::new());
        fs.seed_file("\\hello.txt", b"hello world");
        let mut harness = Harness::new(fs, false);

        let created = only(
            harness.dispatch(ServerDriveIoRequest::ServerCreateDriveRequest(open_request(
                "\\hello.txt",
                1,
            ))),
        );
        let file_id = read_u32_at(&encoded(&created), 16);

        let query = only(
            harness.dispatch(ServerDriveIoRequest::ServerDriveQueryInformationRequest(
                ServerDriveQueryInformationRequest {
                    device_io_request: dev_io_req(file_id, 2, MajorFunction::QueryInformation),
                    file_info_class_lvl: FileInformationClassLevel::FILE_ATTRIBUTE_TAG_INFORMATION,
                },
            )),
        );
        assert_eq!(status_of(&query), NtStatus::SUCCESS);

        let bytes = encoded(&query);
        // 16-byte SharedHeader+DeviceIoResponse prefix, then a 4-byte Length field, then the
        // FileAttributeTagInformation buffer itself: FileAttributes(u32 LE) + ReparseTag(u32 LE)
        // per MS-FSCC 2.4.6 — see `FileAttributeTagInformation::size()` in ironrdp-rdpdr's efs.rs.
        let length = read_u32_at(&bytes, 16);
        assert_eq!(
            length, 8,
            "FileAttributeTagInformation must be exactly 8 bytes on the wire"
        );
        let file_attributes = read_u32_at(&bytes, 20);
        let reparse_tag = read_u32_at(&bytes, 24);
        assert_eq!(file_attributes, FileAttributes::FILE_ATTRIBUTE_NORMAL.bits());
        assert_eq!(reparse_tag, 0, "this backend never exposes reparse points");
    }

    #[test]
    fn query_information_file_attribute_tag_on_directory_reports_directory_attribute() {
        let fs = Rc::new(MockFs::new());
        fs.seed_dir("\\dir");
        let mut harness = Harness::new(fs, false);
        let file_id = open_dir(&mut harness, "\\dir", 1);

        let query = only(
            harness.dispatch(ServerDriveIoRequest::ServerDriveQueryInformationRequest(
                ServerDriveQueryInformationRequest {
                    device_io_request: dev_io_req(file_id, 2, MajorFunction::QueryInformation),
                    file_info_class_lvl: FileInformationClassLevel::FILE_ATTRIBUTE_TAG_INFORMATION,
                },
            )),
        );
        assert_eq!(status_of(&query), NtStatus::SUCCESS);
        let bytes = encoded(&query);
        let file_attributes = read_u32_at(&bytes, 20);
        assert_eq!(file_attributes, FileAttributes::FILE_ATTRIBUTE_DIRECTORY.bits());
    }

    #[test]
    fn large_read_request_returns_everything_available_not_a_short_read() {
        // Regression test for a corruption bug, not a throughput one: Windows requests up to
        // 1 MiB per DeviceReadRequest and advances its next read by the REQUESTED length
        // regardless of how much we actually return (measured live: answering
        // `length=1048576, offset=0` with 65536 bytes produced a follow-up read at
        // offset=1048576, not 65536). A deliberate short read therefore silently punches holes
        // in the copied file. Only a genuine EOF short read is safe.
        let fs = Rc::new(MockFs::new());
        let mut contents = vec![0u8; 70 * 1024];
        for (i, byte) in contents.iter_mut().enumerate() {
            *byte = u8::try_from(i % 251).expect("i % 251 fits in a u8");
        }
        fs.seed_file("\\big.bin", &contents);
        let mut harness = Harness::new(fs, false);

        let created = only(
            harness.dispatch(ServerDriveIoRequest::ServerCreateDriveRequest(open_request(
                "\\big.bin",
                1,
            ))),
        );
        let file_id = read_u32_at(&encoded(&created), 16);

        let read = only(
            harness.dispatch(ServerDriveIoRequest::DeviceReadRequest(DeviceReadRequest {
                device_io_request: dev_io_req(file_id, 2, MajorFunction::Read),
                length: 1024 * 1024,
                offset: 0,
            })),
        );
        assert_eq!(status_of(&read), NtStatus::SUCCESS);
        let bytes = encoded(&read);
        let data_len = read_u32_at(&bytes, 16) as usize;
        assert_eq!(
            data_len,
            70 * 1024,
            "a 1 MiB request against a 70 KiB file must return ALL 70 KiB (a real EOF short read); returning less makes the redirector skip the remainder"
        );
        assert_eq!(
            &bytes[20..20 + data_len],
            &contents[..],
            "read must return the correct bytes starting at the requested offset"
        );

        // Reading at a non-zero offset must start exactly there — the redirector picks its own
        // offsets (it pipelines reads rather than chaining them off each completion).
        let second_read = only(
            harness.dispatch(ServerDriveIoRequest::DeviceReadRequest(DeviceReadRequest {
                device_io_request: dev_io_req(file_id, 3, MajorFunction::Read),
                length: 1024 * 1024,
                offset: 64 * 1024,
            })),
        );
        assert_eq!(status_of(&second_read), NtStatus::SUCCESS);
        let second_bytes = encoded(&second_read);
        let second_len = read_u32_at(&second_bytes, 16) as usize;
        assert_eq!(
            second_len,
            70 * 1024 - 64 * 1024,
            "EOF short read: everything left in the file from the requested offset"
        );
        assert_eq!(&second_bytes[20..20 + second_len], &contents[64 * 1024..]);
    }

    #[test]
    fn create_of_missing_file_without_create_disposition_is_no_such_file() {
        let fs = Rc::new(MockFs::new());
        let mut harness = Harness::new(fs, false);

        let response = only(
            harness.dispatch(ServerDriveIoRequest::ServerCreateDriveRequest(open_request(
                "\\missing.txt",
                1,
            ))),
        );
        assert_eq!(status_of(&response), NtStatus::NO_SUCH_FILE);
    }

    #[test]
    fn create_on_existing_directory_with_no_type_flags_succeeds_as_directory() {
        // Windows' redirector routinely opens a directory with NEITHER FILE_DIRECTORY_FILE nor
        // FILE_NON_DIRECTORY_FILE set (observed live: double-clicking a file on the share opens
        // its parent directory this way). The backend must not blindly assume "file" and call
        // `DriveFs::open_file` on a directory path.
        let fs = Rc::new(MockFs::new());
        fs.seed_file("\\dir\\a.txt", b"a");
        let mut harness = Harness::new(fs, false);

        let create = open_request("\\dir", 1);
        assert!(
            create.create_options.is_empty(),
            "test fixture must exercise the no-flags case"
        );
        let created = only(harness.dispatch(ServerDriveIoRequest::ServerCreateDriveRequest(create)));
        assert_eq!(status_of(&created), NtStatus::SUCCESS);
        let file_id = read_u32_at(&encoded(&created), 16);

        // `MockFs::list` re-derives the target by path, not by the handle's recorded type, so it
        // would "accidentally" succeed below even if the handle were mis-tracked as a file —
        // assert directly on `DriveState`'s bookkeeping (accessible here: `tests` is a
        // descendant module of `backend`, where `WasmDriveBackend::state` is defined) to
        // actually pin down the bug: a directory opened without type flags must never carry a
        // `DriveFs` file handle, or the real (browser) `DriveFs` would have already rejected
        // `open_file` against it with a `TypeMismatchError` before we got this far.
        {
            let state = harness.backend.state.borrow();
            let entry = state.get(file_id).expect("file_id must be open");
            assert!(
                entry.is_dir,
                "directory opened without type flags must be tracked as a directory"
            );
            assert!(
                entry.fs_handle.is_none(),
                "a directory entry must never carry a DriveFs file handle"
            );
        }

        // And functionally: QueryDirectory only works against a directory entry.
        let listing = only(harness.dispatch(ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(
            query_directory_request(file_id, 2, 1, "\\dir\\*"),
        )));
        assert_eq!(status_of(&listing), NtStatus::SUCCESS);
        assert_eq!(dir_entry_name(&listing), "a.txt");
    }

    #[test]
    fn create_with_non_directory_flag_on_a_directory_is_file_is_a_directory() {
        let fs = Rc::new(MockFs::new());
        fs.seed_dir("\\dir");
        let mut harness = Harness::new(fs, false);

        let mut create = open_request("\\dir", 1);
        create.create_options = CreateOptions::FILE_NON_DIRECTORY_FILE;
        let response = only(harness.dispatch(ServerDriveIoRequest::ServerCreateDriveRequest(create)));
        assert_eq!(status_of(&response), NtStatus::FILE_IS_A_DIRECTORY);
    }

    fn open_dir(harness: &mut Harness, path: &str, completion_id: u32) -> u32 {
        let mut create = open_request(path, completion_id);
        create.create_options = CreateOptions::FILE_DIRECTORY_FILE;
        let created = only(harness.dispatch(ServerDriveIoRequest::ServerCreateDriveRequest(create)));
        assert_eq!(status_of(&created), NtStatus::SUCCESS);
        read_u32_at(&encoded(&created), 16)
    }

    fn query_directory_request(
        file_id: u32,
        completion_id: u32,
        initial_query: u8,
        path: &str,
    ) -> ServerDriveQueryDirectoryRequest {
        ServerDriveQueryDirectoryRequest {
            device_io_request: dev_io_req(file_id, completion_id, MajorFunction::DirectoryControl),
            file_info_class_lvl: FileInformationClassLevel::FILE_NAMES_INFORMATION,
            initial_query,
            path: path.to_string(),
        }
    }

    #[test]
    fn query_directory_exact_name_pattern_returns_only_that_entry() {
        let fs = Rc::new(MockFs::new());
        fs.seed_file("\\dir\\a.txt", b"a");
        fs.seed_file("\\dir\\b.txt", b"bb");
        let mut harness = Harness::new(fs, false);
        let file_id = open_dir(&mut harness, "\\dir", 1);

        let initial = only(harness.dispatch(ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(
            query_directory_request(file_id, 2, 1, "\\dir\\a.txt"),
        )));
        assert_eq!(status_of(&initial), NtStatus::SUCCESS);
        assert_eq!(dir_entry_name(&initial), "a.txt");

        // Only one entry matched the exact-name pattern, so a continuation must be exhausted.
        let second = only(harness.dispatch(ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(
            query_directory_request(file_id, 3, 0, ""),
        )));
        assert_eq!(status_of(&second), NtStatus::NO_MORE_FILES);
    }

    #[test]
    fn query_directory_exact_name_pattern_for_missing_file_is_no_such_file() {
        let fs = Rc::new(MockFs::new());
        fs.seed_file("\\dir\\a.txt", b"a");
        let mut harness = Harness::new(fs, false);
        let file_id = open_dir(&mut harness, "\\dir", 1);

        let response = only(harness.dispatch(ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(
            query_directory_request(file_id, 2, 1, "\\dir\\missing.txt"),
        )));
        assert_eq!(status_of(&response), NtStatus::NO_SUCH_FILE);
    }

    #[test]
    fn query_directory_wildcard_pattern_filters_by_extension() {
        let fs = Rc::new(MockFs::new());
        fs.seed_file("\\dir\\a.txt", b"a");
        fs.seed_file("\\dir\\b.log", b"bb");
        let mut harness = Harness::new(fs, false);
        let file_id = open_dir(&mut harness, "\\dir", 1);

        let initial = only(harness.dispatch(ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(
            query_directory_request(file_id, 2, 1, "\\dir\\*.txt"),
        )));
        assert_eq!(status_of(&initial), NtStatus::SUCCESS);
        assert_eq!(dir_entry_name(&initial), "a.txt");

        // `b.log` never matched `*.txt`, so only one entry was ever cached.
        let second = only(harness.dispatch(ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(
            query_directory_request(file_id, 3, 0, ""),
        )));
        assert_eq!(status_of(&second), NtStatus::NO_MORE_FILES);
    }

    #[test]
    fn create_with_parent_traversal_path_is_access_denied_without_touching_fs() {
        let fs = Rc::new(MockFs::new());
        let mut harness = Harness::new(fs, false);

        let response = only(
            harness.dispatch(ServerDriveIoRequest::ServerCreateDriveRequest(open_request(
                "\\..\\x", 1,
            ))),
        );
        assert_eq!(status_of(&response), NtStatus::ACCESS_DENIED);
        harness.assert_no_completion();
    }

    #[test]
    fn set_information_rename_with_parent_traversal_target_is_access_denied() {
        let fs = Rc::new(MockFs::new());
        fs.seed_file("\\old.txt", b"payload");
        let mut harness = Harness::new(Rc::clone(&fs), false);

        let created = only(
            harness.dispatch(ServerDriveIoRequest::ServerCreateDriveRequest(open_request(
                "\\old.txt",
                1,
            ))),
        );
        let file_id = read_u32_at(&encoded(&created), 16);

        let response = only(harness.dispatch(ServerDriveIoRequest::ServerDriveSetInformationRequest(
            ServerDriveSetInformationRequest {
                device_io_request: dev_io_req(file_id, 2, MajorFunction::SetInformation),
                set_buffer: FileInformationClass::Rename(FileRenameInformation {
                    replace_if_exists: Boolean::False,
                    file_name: "\\..\\escaped.txt".to_string(),
                }),
            },
        )));
        assert_eq!(status_of(&response), NtStatus::ACCESS_DENIED);
        harness.assert_no_completion();

        // The original file must be untouched.
        assert_eq!(read_all_via_fs(&fs, "\\old.txt"), b"payload");
    }

    #[test]
    fn reset_frees_open_drivefs_handles_and_drops_stale_generation_completions() {
        let fs = Rc::new(MockFs::new());
        fs.seed_file("\\a.txt", b"data");
        let mut harness = Harness::new(Rc::clone(&fs), false);

        let created = only(
            harness.dispatch(ServerDriveIoRequest::ServerCreateDriveRequest(open_request(
                "\\a.txt", 1,
            ))),
        );
        assert_eq!(status_of(&created), NtStatus::SUCCESS);
        let file_id = read_u32_at(&encoded(&created), 16);
        let fs_handle = harness
            .backend
            .state
            .borrow()
            .get(file_id)
            .and_then(|entry| entry.fs_handle)
            .expect("Create must have allocated a DriveFs handle");

        // A second IRP against the still-open handle, deliberately dispatched WITHOUT pumping
        // the pool first — its future is queued but has not run yet, so it belongs to the
        // pre-reset generation.
        let messages = harness
            .backend
            .handle_drive_io_request(ServerDriveIoRequest::DeviceReadRequest(DeviceReadRequest {
                device_io_request: dev_io_req(file_id, 2, MajorFunction::Read),
                length: 16,
                offset: 0,
            }))
            .expect("dispatch must not error");
        assert!(messages.is_empty(), "Read is always answered asynchronously");

        // A Server Announce Request re-init happens mid-flight — `Rdpdr::handle_server_announce`
        // calls `reset()` on every announce (crates/ironrdp-rdpdr/src/lib.rs).
        harness.backend.reset().expect("reset must not error");
        harness.pool.run_until_stalled();

        assert!(
            harness.rx.try_recv().is_err(),
            "the Read queued before reset must never deliver a completion into the new sequence"
        );
        assert_eq!(
            block_on(fs.close(fs_handle)),
            Err(FsError::NotFound),
            "reset must already have closed the stale DriveFs handle (a double-close is NotFound)"
        );

        // A fresh Create after reset must allocate from a clean, empty DriveState.
        let recreated = only(
            harness.dispatch(ServerDriveIoRequest::ServerCreateDriveRequest(open_request(
                "\\a.txt", 3,
            ))),
        );
        assert_eq!(status_of(&recreated), NtStatus::SUCCESS);
        assert_eq!(
            read_u32_at(&encoded(&recreated), 16),
            1,
            "file_id counter must restart at 1 in the fresh DriveState"
        );
    }
}
