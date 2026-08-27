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

use core::cell::RefCell;
use core::fmt;
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
    DeviceWriteResponse, FileAttributes, FileBasicInformation, FileBothDirectoryInformation, FileDirectoryInformation,
    FileFsAttributeInformation, FileFsDeviceInformation, FileFsFullSizeInformation, FileFsSizeInformation,
    FileFsVolumeInformation, FileFullDirectoryInformation, FileInformationClass, FileInformationClassLevel,
    FileNamesInformation, FileStandardInformation, FileSystemAttributes, FileSystemInformationClass,
    FileSystemInformationClassLevel, Information, NtStatus, PrinterIoRequest, ServerDeviceAnnounceResponse,
    ServerDriveIoRequest, ServerDriveQueryDirectoryRequest, ServerDriveQueryInformationRequest,
    ServerDriveQueryVolumeInformationRequest, ServerDriveSetInformationRequest,
};
use ironrdp::rdpdr::pdu::esc::{ScardCall, ScardIoCtlCode};
use ironrdp_core::{EncodeResult, impl_as_any};
use ironrdp_pdu::{PduError, PduErrorExt as _, PduResult, pdu_other_err};
use ironrdp_svc::SvcMessage;
use tracing::{debug, warn};

use super::fs::{DriveFs, FsEntry, FsError};
use super::state::DriveState;

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
        }
    }

    fn dispatch_create(&self, create: DeviceCreateRequest) -> Vec<SvcMessage> {
        if self.read_only && is_write_intent(&create) {
            let response = DeviceCreateResponse {
                device_io_reply: DeviceIoResponse::new(create.device_io_request, NtStatus::ACCESS_DENIED),
                file_id: 0,
                information: Information::empty(),
            };
            return vec![SvcMessage::from(RdpdrPdu::DeviceCreateResponse(response))];
        }

        let is_directory = create.create_options.contains(CreateOptions::FILE_DIRECTORY_FILE);
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
        // `create` is not used again after this point, so its two fields the future needs are
        // moved out directly rather than cloned.
        let device_io_request = create.device_io_request;
        let path = create.path;

        let future: LocalBoxFuture<'static, ()> = Box::pin(async move {
            let outcome = if is_directory {
                open_or_create_directory(fs.as_ref(), &path, creates_new).await
            } else {
                fs.open_file(&path, write, creates_new, truncates)
                    .await
                    .map(|handle| (Some(handle), false))
            };

            let (status, file_id, information) = match outcome {
                Ok((fs_handle, is_dir)) => {
                    let file_id = state.borrow_mut().open(path, fs_handle, is_dir);
                    (NtStatus::SUCCESS, file_id, create_information)
                }
                Err(err) => (nt_status_for(&err), 0, Information::empty()),
            };

            let response = DeviceCreateResponse {
                device_io_reply: DeviceIoResponse::new(device_io_request, status),
                file_id,
                information,
            };
            send_completion(&tx, vec![SvcMessage::from(RdpdrPdu::DeviceCreateResponse(response))]);
        });
        (self.spawn)(future);
        Vec::new()
    }

    fn dispatch_query_information(&self, req: ServerDriveQueryInformationRequest) -> Vec<SvcMessage> {
        let file_id = req.device_io_request.file_id;
        let Some(path) = self.state.borrow().get(file_id).map(|entry| entry.path.clone()) else {
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
            let response = ClientDriveQueryInformationResponse {
                device_io_response: DeviceIoResponse::new(device_io_request, status),
                buffer,
            };
            send_completion(
                &tx,
                vec![SvcMessage::from(RdpdrPdu::ClientDriveQueryInformationResponse(
                    response,
                ))],
            );
        });
        (self.spawn)(future);
        Vec::new()
    }

    fn dispatch_close(&self, req: DeviceCloseRequest) -> Vec<SvcMessage> {
        let file_id = req.device_io_request.file_id;
        let Some(entry) = self.state.borrow_mut().close(file_id) else {
            let response = DeviceCloseResponse {
                device_io_response: DeviceIoResponse::new(req.device_io_request, NtStatus::INVALID_HANDLE),
            };
            return vec![SvcMessage::from(RdpdrPdu::DeviceCloseResponse(response))];
        };

        let Some(fs_handle) = entry.fs_handle else {
            // Directory handles have nothing to close on the `DriveFs` side (see
            // `OpenEntry::fs_handle`'s doc comment in `state.rs`).
            let response = DeviceCloseResponse {
                device_io_response: DeviceIoResponse::new(req.device_io_request, NtStatus::SUCCESS),
            };
            return vec![SvcMessage::from(RdpdrPdu::DeviceCloseResponse(response))];
        };

        let fs = Rc::clone(&self.fs);
        let tx = self.tx.clone();
        let device_io_request = req.device_io_request;
        let future: LocalBoxFuture<'static, ()> = Box::pin(async move {
            let status = match fs.close(fs_handle).await {
                Ok(()) => NtStatus::SUCCESS,
                Err(err) => nt_status_for(&err),
            };
            let response = DeviceCloseResponse {
                device_io_response: DeviceIoResponse::new(device_io_request, status),
            };
            send_completion(&tx, vec![SvcMessage::from(RdpdrPdu::DeviceCloseResponse(response))]);
        });
        (self.spawn)(future);
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
            let response = ClientDriveQueryDirectoryResponse {
                device_io_reply: DeviceIoResponse::new(device_io_request, status),
                buffer,
            };
            return vec![SvcMessage::from(RdpdrPdu::ClientDriveQueryDirectoryResponse(response))];
        }

        // Initial query: re-list the directory this `file_id` was opened against (the search
        // pattern in `req.path`, if any, is not applied — `DriveFs::list` has no filtering
        // primitive, so every initial query lists the whole directory).
        let Some(path) = self.state.borrow().get(file_id).map(|entry| entry.path.clone()) else {
            let response = ClientDriveQueryDirectoryResponse {
                device_io_reply: DeviceIoResponse::new(device_io_request, NtStatus::INVALID_HANDLE),
                buffer: None,
            };
            return vec![SvcMessage::from(RdpdrPdu::ClientDriveQueryDirectoryResponse(response))];
        };

        let fs = Rc::clone(&self.fs);
        let state = Rc::clone(&self.state);
        let tx = self.tx.clone();
        let class_lvl = req.file_info_class_lvl;

        let future: LocalBoxFuture<'static, ()> = Box::pin(async move {
            let (status, buffer) = match fs.list(&path).await {
                Ok(listing) => {
                    state.borrow_mut().set_dir_listing(file_id, listing);
                    match state.borrow_mut().next_dir_entry(file_id) {
                        Some(entry) => (NtStatus::SUCCESS, Some(build_dir_info(&class_lvl, &entry))),
                        None => (NtStatus::NO_MORE_FILES, None),
                    }
                }
                Err(err) => (nt_status_for(&err), None),
            };
            let response = ClientDriveQueryDirectoryResponse {
                device_io_reply: DeviceIoResponse::new(device_io_request, status),
                buffer,
            };
            send_completion(
                &tx,
                vec![SvcMessage::from(RdpdrPdu::ClientDriveQueryDirectoryResponse(response))],
            );
        });
        (self.spawn)(future);
        Vec::new()
    }

    fn dispatch_read(&self, req: DeviceReadRequest) -> Vec<SvcMessage> {
        let file_id = req.device_io_request.file_id;
        let device_io_request = req.device_io_request;
        let Some(fs_handle) = self.state.borrow().get(file_id).and_then(|entry| entry.fs_handle) else {
            let response = DeviceReadResponse {
                device_io_reply: DeviceIoResponse::new(device_io_request, NtStatus::INVALID_HANDLE),
                read_data: Vec::new(),
            };
            return vec![SvcMessage::from(RdpdrPdu::DeviceReadResponse(response))];
        };

        let fs = Rc::clone(&self.fs);
        let tx = self.tx.clone();
        let offset = req.offset;
        let length = req.length;

        let future: LocalBoxFuture<'static, ()> = Box::pin(async move {
            let (status, read_data) = match fs.read(fs_handle, offset, length).await {
                Ok(data) => (NtStatus::SUCCESS, data),
                Err(err) => (nt_status_for(&err), Vec::new()),
            };
            let response = DeviceReadResponse {
                device_io_reply: DeviceIoResponse::new(device_io_request, status),
                read_data,
            };
            send_completion(&tx, vec![SvcMessage::from(RdpdrPdu::DeviceReadResponse(response))]);
        });
        (self.spawn)(future);
        Vec::new()
    }

    fn dispatch_write(&self, req: DeviceWriteRequest) -> Vec<SvcMessage> {
        let file_id = req.device_io_request.file_id;
        let device_io_request = req.device_io_request;
        if self.read_only {
            let response = DeviceWriteResponse {
                device_io_reply: DeviceIoResponse::new(device_io_request, NtStatus::ACCESS_DENIED),
                length: 0,
            };
            return vec![SvcMessage::from(RdpdrPdu::DeviceWriteResponse(response))];
        }

        let Some(fs_handle) = self.state.borrow().get(file_id).and_then(|entry| entry.fs_handle) else {
            let response = DeviceWriteResponse {
                device_io_reply: DeviceIoResponse::new(device_io_request, NtStatus::INVALID_HANDLE),
                length: 0,
            };
            return vec![SvcMessage::from(RdpdrPdu::DeviceWriteResponse(response))];
        };

        let fs = Rc::clone(&self.fs);
        let tx = self.tx.clone();
        let offset = req.offset;
        let data = req.write_data;

        let future: LocalBoxFuture<'static, ()> = Box::pin(async move {
            let (status, length) = match fs.write(fs_handle, offset, data).await {
                Ok(written) => (NtStatus::SUCCESS, written),
                Err(err) => (nt_status_for(&err), 0),
            };
            let response = DeviceWriteResponse {
                device_io_reply: DeviceIoResponse::new(device_io_request, status),
                length,
            };
            send_completion(&tx, vec![SvcMessage::from(RdpdrPdu::DeviceWriteResponse(response))]);
        });
        (self.spawn)(future);
        Vec::new()
    }

    fn dispatch_set_information(&self, req: ServerDriveSetInformationRequest) -> PduResult<Vec<SvcMessage>> {
        if self.read_only {
            return Ok(vec![set_information_message(&req, NtStatus::ACCESS_DENIED)?]);
        }

        let file_id = req.device_io_request.file_id;
        let Some(path) = self.state.borrow().get(file_id).map(|entry| entry.path.clone()) else {
            return Ok(vec![set_information_message(&req, NtStatus::INVALID_HANDLE)?]);
        };

        // Only rename and delete-disposition map onto a `DriveFs` primitive. The remaining
        // classes `ServerDriveSetInformationRequest::decode` accepts — Basic/EndOfFile/
        // Allocation — have no corresponding `DriveFs` operation (no chmod/resize primitive in
        // Task 1's scope), so they complete immediately as unsupported rather than pretending to
        // apply.
        match req.set_buffer.clone() {
            FileInformationClass::Rename(rename) => {
                let fs = Rc::clone(&self.fs);
                let state = Rc::clone(&self.state);
                let tx = self.tx.clone();
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
                    match set_information_message(&req, status) {
                        Ok(message) => send_completion(&tx, vec![message]),
                        Err(error) => warn!(?error, "Failed to encode ClientDriveSetInformationResponse"),
                    }
                });
                (self.spawn)(future);
                Ok(Vec::new())
            }
            FileInformationClass::Disposition(disposition) if disposition.delete_pending != 0 => {
                let fs = Rc::clone(&self.fs);
                let tx = self.tx.clone();
                let target = path;
                let future: LocalBoxFuture<'static, ()> = Box::pin(async move {
                    let status = match fs.delete(&target).await {
                        Ok(()) => NtStatus::SUCCESS,
                        Err(err) => nt_status_for(&err),
                    };
                    match set_information_message(&req, status) {
                        Ok(message) => send_completion(&tx, vec![message]),
                        Err(error) => warn!(?error, "Failed to encode ClientDriveSetInformationResponse"),
                    }
                });
                (self.spawn)(future);
                Ok(Vec::new())
            }
            // `Disposition` with `delete_pending == 0` (clearing a delete request we never
            // actually deferred) is a trivial acknowledgement; every other class has no
            // `DriveFs` primitive.
            FileInformationClass::Disposition(_) => Ok(vec![set_information_message(&req, NtStatus::SUCCESS)?]),
            _ => Ok(vec![set_information_message(&req, NtStatus::NOT_SUPPORTED)?]),
        }
    }
}

impl RdpdrBackend for WasmDriveBackend {
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
            ServerDriveIoRequest::ServerDriveNotifyChangeDirectoryRequest(_req) => Ok(Vec::new()),

            ServerDriveIoRequest::ServerDriveQueryVolumeInformationRequest(req) => {
                Ok(vec![query_volume_information_response(req)])
            }

            // `IRP_MJ_DEVICE_CONTROL`: no filesystem IOCTL this backend implements; ack empty
            // per the crib (matches FreeRDP's own default reply for unhandled control codes).
            ServerDriveIoRequest::DeviceControlRequest(req) => {
                let response = DeviceControlResponse::new(req, NtStatus::SUCCESS, None);
                Ok(vec![SvcMessage::from(RdpdrPdu::DeviceControlResponse(response))])
            }

            // `IRP_MJ_FLUSH_BUFFERS`: every `DriveFs::write` already applies synchronously from
            // the server's perspective (there is no separate buffered-write stage to flush), so
            // this is an immediate acknowledgement, no `DriveFs` round-trip needed.
            ServerDriveIoRequest::DeviceFlushBuffersRequest(req) => {
                let response = DeviceFlushBuffersResponse {
                    device_io_response: DeviceIoResponse::new(req.device_io_request, NtStatus::SUCCESS),
                };
                Ok(vec![SvcMessage::from(RdpdrPdu::DeviceFlushBuffersResponse(response))])
            }

            // `IRP_MJ_LOCK_CONTROL`: no-op success, matching FreeRDP parity for a
            // browser-backed share (byte-range locking has no meaning without a real
            // multi-client filesystem to coordinate).
            ServerDriveIoRequest::ServerDriveLockControlRequest(req) => {
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
                let response = ClientDriveQuerySecurityResponse {
                    device_io_response: DeviceIoResponse::new(req.device_io_request, NtStatus::NOT_SUPPORTED),
                    security_descriptor: None,
                };
                Ok(vec![SvcMessage::from(RdpdrPdu::ClientDriveQuerySecurityResponse(
                    response,
                ))])
            }
            ServerDriveIoRequest::ServerDriveSetSecurityRequest(req) => {
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
/// (`RdpInputEvent::DriveBackend`, wired up by a later task).
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

fn send_completion(tx: &mpsc::UnboundedSender<DriveBackendMessage>, messages: Vec<SvcMessage>) {
    if tx.unbounded_send(DriveBackendMessage::IoCompleted(messages)).is_err() {
        warn!("Failed to deliver drive IRP completion; event loop receiver is closed");
    }
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
    } else {
        None
    }
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
        DeviceIoRequest, FileDispositionInformation, FileRenameInformation, MajorFunction, MinorFunction, SharedAccess,
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
}
