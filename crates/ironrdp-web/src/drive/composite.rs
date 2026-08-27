//! [`WasmCompositeBackend`]: an [`RdpdrBackend`] that lets printer redirection
//! ([`WasmPrinterBackend`]) and drive redirection ([`super::backend::WasmDriveBackend`]) coexist
//! on the single backend slot `Rdpdr` accepts (`Rdpdr::new` takes exactly one
//! `Box<dyn RdpdrBackend>` — see `crates/ironrdp-rdpdr/src/lib.rs`).
//!
//! Routing for the two IRP-shaped methods is unambiguous without knowing device ids at all:
//! the RDPDR channel processor (`Rdpdr::process`) already separates `DeviceType::Filesystem`
//! IRPs (routed to [`RdpdrBackend::handle_drive_io_request`]) from `DeviceType::Print` IRPs
//! (routed to [`RdpdrBackend::handle_printer_io_request`]) before either method is ever called —
//! so each method here always means "the drive member" / "the printer member" respectively,
//! regardless of which numeric device id the server used.
//!
//! [`RdpdrBackend::handle_server_device_announce_response`] is different: `Rdpdr` calls it once
//! per announced device, of EITHER type, through the same method. This backend has to know
//! which device id belongs to which member to route it correctly (and to log accurately when
//! neither member claims a given id) — hence the `_device_id` fields alongside each member.
//!
//! ## Absent-member fallbacks
//!
//! [`RdpdrBackend::handle_printer_io_request`] has a trait default (`backend/mod.rs`): answer
//! immediately with a `DeviceCloseResponse`/`NOT_SUPPORTED`, since a printer IRP is always
//! Create/Write/Close and a close-shaped completion satisfies the "this IRP is now done"
//! contract regardless of which major function it actually was. This backend reproduces that
//! exact fallback verbatim when `printer` is `None` (overriding the method to route to a present
//! member forfeits access to the trait's own default body, so [`printer_not_configured_response`]
//! duplicates it rather than calling it).
//!
//! [`RdpdrBackend::handle_drive_io_request`] has NO trait default — it is a required method with
//! no body (`backend/mod.rs`'s own doc: only `handle_scard_call` and
//! `handle_server_device_announce_response` are unconditionally required; `handle_drive_io_request`
//! is documented as required too and has no default block). [`WasmDriveBackend`]'s own drive-only
//! answer for an unrelated IRP class (`handle_printer_io_request`) is a session-killing `Err`, and
//! so is [`ironrdp_rdpdr::backend::noop::NoopRdpdrBackend`]'s `handle_drive_io_request` — neither
//! is the "graceful" shape this composite needs when its own `drive` member is absent. Instead,
//! [`drive_not_configured_response`] generalizes the printer trait default's own convention
//! (complete the IRP immediately with a close-shaped `NOT_SUPPORTED` response) across every
//! [`ServerDriveIoRequest`] variant, extracting whichever variant's `device_io_request` (or
//! `header`, for `DeviceControlRequest`) it carries. In real operation this path is unreachable
//! dead code: `Rdpdr::process` only ever dispatches a `DeviceType::Filesystem` IRP for a device id
//! present in `active_device_ids`, which requires an actual filesystem device announcement — a
//! composite built with `drive: None` never asks `Rdpdr` to announce one (see `session.rs`'s
//! attach site). This fallback exists purely as defense in depth against that invariant ever
//! drifting, not as a normal-operation code path.

use ironrdp::rdpdr::backend::RdpdrBackend;
use ironrdp::rdpdr::pdu::RdpdrPdu;
use ironrdp::rdpdr::pdu::efs::{
    DeviceCloseResponse, DeviceControlRequest, DeviceIoRequest, DeviceIoResponse, NtStatus, PrinterIoRequest,
    ServerDeviceAnnounceResponse, ServerDriveIoRequest,
};
use ironrdp::rdpdr::pdu::esc::{ScardCall, ScardIoCtlCode};
use ironrdp_core::impl_as_any;
use ironrdp_pdu::PduResult;
use ironrdp_svc::SvcMessage;
use tracing::{debug, warn};

use super::backend::WasmDriveBackend;
use crate::printer::WasmPrinterBackend;

/// Composite [`RdpdrBackend`] letting a redirected printer and a redirected drive share coexist
/// on the one backend slot `Rdpdr` accepts. Either member may be absent (constructing this with
/// both absent is legal but pointless — callers only reach for a composite when at least one is
/// configured; see `session.rs`'s attach site).
///
/// `printer_device_id` / `drive_device_id` mirror whichever member is `Some` (`Some(id)` iff the
/// matching backend is `Some`) — they exist only to route
/// [`RdpdrBackend::handle_server_device_announce_response`], the one method `Rdpdr` calls for
/// both device types through a single entry point (see this module's doc comment).
#[derive(Debug)]
pub(crate) struct WasmCompositeBackend {
    printer: Option<WasmPrinterBackend>,
    printer_device_id: Option<u32>,
    drive: Option<WasmDriveBackend>,
    drive_device_id: Option<u32>,
}

impl_as_any!(WasmCompositeBackend);

impl WasmCompositeBackend {
    /// Builds a composite from whichever of `printer` / `drive` were configured for this
    /// session, each paired with the device id it will be announced under.
    pub(crate) fn new(printer: Option<(WasmPrinterBackend, u32)>, drive: Option<(WasmDriveBackend, u32)>) -> Self {
        let (printer, printer_device_id) = match printer {
            Some((backend, device_id)) => (Some(backend), Some(device_id)),
            None => (None, None),
        };
        let (drive, drive_device_id) = match drive {
            Some((backend, device_id)) => (Some(backend), Some(device_id)),
            None => (None, None),
        };
        Self {
            printer,
            printer_device_id,
            drive,
            drive_device_id,
        }
    }
}

impl RdpdrBackend for WasmCompositeBackend {
    /// Releases per-sequence state on both members. Delegating unconditionally is safe even for
    /// a member whose own `reset` is the trait's no-op default (as [`WasmPrinterBackend`]'s is
    /// today) — see this struct's own doc comment on why that leaves its open print jobs
    /// undisturbed across a reset, a pre-existing property of [`WasmPrinterBackend`] this
    /// composite neither introduces nor fixes.
    fn reset(&mut self) -> PduResult<()> {
        if let Some(printer) = &mut self.printer {
            printer.reset()?;
        }
        if let Some(drive) = &mut self.drive {
            drive.reset()?;
        }
        Ok(())
    }

    /// Routes by device id to whichever member owns it (see this module's doc comment for why
    /// device id, unlike the two IRP-handling methods below, is unavoidable here). An id owned by
    /// neither member — which should never happen, since both ids are exactly the ones this
    /// backend itself was constructed with and `Rdpdr` only announces devices its own device list
    /// carries — is logged rather than silently dropped, so a routing invariant violation is
    /// visible instead of invisible.
    fn handle_server_device_announce_response(&mut self, pdu: ServerDeviceAnnounceResponse) -> PduResult<()> {
        let device_id = pdu.device_id;
        if self.drive_device_id == Some(device_id) {
            if let Some(drive) = &mut self.drive {
                return drive.handle_server_device_announce_response(pdu);
            }
        }
        if self.printer_device_id == Some(device_id) {
            if let Some(printer) = &mut self.printer {
                return printer.handle_server_device_announce_response(pdu);
            }
        }
        warn!(
            device_id,
            "RDPDR device announce response for a device id owned by neither the composite \
             backend's printer nor drive member; ignoring"
        );
        Ok(())
    }

    /// Neither member implements smartcard redirection — [`WasmDriveBackend`]'s own answer is a
    /// session-killing `Err` (see this module's doc comment on why that shape must never
    /// propagate through the composite), and [`WasmPrinterBackend`]'s is a graceful no-op. This
    /// composite always takes the graceful shape: the smartcard device type is never announced by
    /// either member's construction path (see `session.rs`), so in practice a server never has
    /// reason to send this at all.
    fn handle_scard_call(
        &mut self,
        _req: DeviceControlRequest<ScardIoCtlCode>,
        _call: ScardCall,
    ) -> PduResult<Vec<SvcMessage>> {
        warn!("Smartcard IOCTL reached composite RDPDR backend; ignoring (unsupported by both members)");
        Ok(Vec::new())
    }

    /// `Rdpdr::process` only reaches this method for a `DeviceType::Filesystem` IRP (see this
    /// module's doc comment), so an absent `drive` member here is the "drive redirection was
    /// never configured for this session" case, not a routing ambiguity.
    fn handle_drive_io_request(&mut self, req: ServerDriveIoRequest) -> PduResult<Vec<SvcMessage>> {
        match &mut self.drive {
            Some(drive) => {
                debug!("[rdpdr-drive] composite: routing drive IRP to the configured drive member");
                drive.handle_drive_io_request(req)
            }
            None => {
                debug!("[rdpdr-drive] composite: no drive member configured; using not-configured fallback");
                Ok(drive_not_configured_response(req))
            }
        }
    }

    /// `Rdpdr::process` only reaches this method for a `DeviceType::Print` IRP, so an absent
    /// `printer` member here is the "printer redirection was never configured for this session"
    /// case, not a routing ambiguity.
    fn handle_printer_io_request(&mut self, req: PrinterIoRequest) -> PduResult<Vec<SvcMessage>> {
        match &mut self.printer {
            Some(printer) => {
                debug!("[rdpdr-printer] composite: routing printer IRP to the configured printer member");
                printer.handle_printer_io_request(req)
            }
            None => {
                debug!("[rdpdr-printer] composite: no printer member configured; using not-configured fallback");
                Ok(printer_not_configured_response(req))
            }
        }
    }
}

/// Reproduces [`RdpdrBackend::handle_printer_io_request`]'s own trait-default body verbatim
/// (`backend/mod.rs`) — overriding the method above forfeits access to that default, so it is
/// duplicated here rather than called.
fn printer_not_configured_response(req: PrinterIoRequest) -> Vec<SvcMessage> {
    let device_io_request = req.into_device_io_request();
    vec![SvcMessage::from(RdpdrPdu::DeviceCloseResponse(DeviceCloseResponse {
        device_io_response: DeviceIoResponse::new(device_io_request, NtStatus::NOT_SUPPORTED),
    }))]
}

/// Generalizes the same close-shaped `NOT_SUPPORTED` convention
/// [`printer_not_configured_response`] reproduces across every [`ServerDriveIoRequest`] variant —
/// see this module's doc comment for why `handle_drive_io_request` has no trait default of its
/// own to fall back to. Dead code in real operation (see this module's doc comment); kept simple
/// (one status, one response shape) accordingly rather than mirroring each variant's "real"
/// response type.
fn drive_not_configured_response(req: ServerDriveIoRequest) -> Vec<SvcMessage> {
    let device_io_request = drive_device_io_request(&req);
    vec![SvcMessage::from(RdpdrPdu::DeviceCloseResponse(DeviceCloseResponse {
        device_io_response: DeviceIoResponse::new(device_io_request, NtStatus::NOT_SUPPORTED),
    }))]
}

/// Extracts the common `DeviceIoRequest` header every [`ServerDriveIoRequest`] variant carries
/// (named `device_io_request` on every variant except `DeviceControlRequest`, which names it
/// `header` — see `ironrdp-rdpdr/src/pdu/efs.rs`).
fn drive_device_io_request(req: &ServerDriveIoRequest) -> DeviceIoRequest {
    match req {
        ServerDriveIoRequest::ServerCreateDriveRequest(r) => r.device_io_request.clone(),
        ServerDriveIoRequest::ServerDriveQueryInformationRequest(r) => r.device_io_request.clone(),
        ServerDriveIoRequest::DeviceCloseRequest(r) => r.device_io_request.clone(),
        ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(r) => r.device_io_request.clone(),
        ServerDriveIoRequest::ServerDriveNotifyChangeDirectoryRequest(r) => r.device_io_request.clone(),
        ServerDriveIoRequest::ServerDriveQueryVolumeInformationRequest(r) => r.device_io_request.clone(),
        ServerDriveIoRequest::DeviceControlRequest(r) => r.header.clone(),
        ServerDriveIoRequest::DeviceReadRequest(r) => r.device_io_request.clone(),
        ServerDriveIoRequest::DeviceWriteRequest(r) => r.device_io_request.clone(),
        ServerDriveIoRequest::DeviceFlushBuffersRequest(r) => r.device_io_request.clone(),
        ServerDriveIoRequest::ServerDriveSetInformationRequest(r) => r.device_io_request.clone(),
        ServerDriveIoRequest::ServerDriveLockControlRequest(r) => r.device_io_request.clone(),
        ServerDriveIoRequest::ServerDriveQuerySecurityRequest(r) => r.device_io_request.clone(),
        ServerDriveIoRequest::ServerDriveSetSecurityRequest(r) => r.device_io_request.clone(),
    }
}

/// Resolves a device-id collision between the drive share's device id and the (possibly
/// caller-configured via the `printerDeviceId()` extension) printer device id. `Rdpdr` would
/// otherwise be asked to announce two devices under the same id, which nothing downstream
/// deduplicates.
///
/// Returns the drive device id to actually announce: `default_drive_device_id` unchanged unless
/// it collides with `printer_device_id`, in which case the drive id is shifted to
/// `printer_device_id + 1` and the collision is logged at `warn!`.
///
/// `printer_device_id` is always `> 0` by construction (`SessionBuilder::extension`'s
/// `printerDeviceId()` handler maps a caller-supplied `0` to `None`, i.e. "use the default," never
/// to `Some(0)`), so `printer_device_id - 1` below can never underflow — it is used only as the
/// fallback for the astronomically unlikely case where `printer_device_id` is already `u32::MAX`
/// (an ordinary small device index in every real caller) and `+ 1` would otherwise wrap to `0`.
pub(crate) fn resolve_drive_device_id(default_drive_device_id: u32, printer_device_id: u32) -> u32 {
    if default_drive_device_id != printer_device_id {
        return default_drive_device_id;
    }
    let resolved = printer_device_id.checked_add(1).unwrap_or(printer_device_id - 1);
    warn!(
        printer_device_id,
        drive_device_id = default_drive_device_id,
        resolved_drive_device_id = resolved,
        "Drive and printer device ids collided; shifting the drive device id to avoid announcing \
         two RDPDR devices under the same id"
    );
    resolved
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use futures::executor::LocalPool;
    use futures::task::LocalSpawnExt;
    use futures_channel::mpsc;
    use ironrdp::rdpdr::pdu::efs::{
        CreateDisposition, CreateOptions, DesiredAccess, DeviceCloseRequest, DeviceCreateRequest, DeviceReadRequest,
        FileAttributes, MajorFunction, MinorFunction, SharedAccess,
    };

    use super::*;
    use crate::drive::backend::{DriveBackendMessage, DriveFsSpawner};
    use crate::drive::fs::MockFs;
    use crate::printer::{PrinterBackendMessage, WasmPrinterMessageProxy};
    use crate::session::RdpInputEvent;

    const PRINTER_DEVICE_ID: u32 = 2;
    const DRIVE_DEVICE_ID: u32 = 1;

    fn drive_dev_io_req(file_id: u32, completion_id: u32, major_function: MajorFunction) -> DeviceIoRequest {
        DeviceIoRequest {
            device_id: DRIVE_DEVICE_ID,
            file_id,
            completion_id,
            major_function,
            minor_function: MinorFunction::from(0),
        }
    }

    fn open_request(path: &str, completion_id: u32) -> DeviceCreateRequest {
        DeviceCreateRequest {
            device_io_request: drive_dev_io_req(0, completion_id, MajorFunction::Create),
            desired_access: DesiredAccess::FILE_READ_DATA_OR_FILE_LIST_DIRECTORY,
            allocation_size: 0,
            file_attributes: FileAttributes::empty(),
            shared_access: SharedAccess::empty(),
            create_disposition: CreateDisposition::FILE_OPEN,
            create_options: CreateOptions::empty(),
            path: path.to_string(),
        }
    }

    fn printer_dev_io_req(file_id: u32, completion_id: u32, major_function: MajorFunction) -> DeviceIoRequest {
        DeviceIoRequest {
            device_id: PRINTER_DEVICE_ID,
            file_id,
            completion_id,
            major_function,
            minor_function: MinorFunction::from(0),
        }
    }

    fn printer_create_request(completion_id: u32) -> DeviceCreateRequest {
        DeviceCreateRequest {
            device_io_request: printer_dev_io_req(0, completion_id, MajorFunction::Create),
            desired_access: DesiredAccess::empty(),
            allocation_size: 0,
            file_attributes: FileAttributes::empty(),
            shared_access: SharedAccess::empty(),
            create_disposition: CreateDisposition::FILE_OPEN,
            create_options: CreateOptions::empty(),
            path: String::new(),
        }
    }

    fn encoded(message: &SvcMessage) -> Vec<u8> {
        message.encode_unframed_pdu().expect("response must encode")
    }

    fn read_u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn status_of(message: &SvcMessage) -> NtStatus {
        NtStatus::from(read_u32_at(&encoded(message), 12))
    }

    fn only(mut messages: Vec<SvcMessage>) -> SvcMessage {
        assert_eq!(messages.len(), 1, "expected exactly one completion PDU");
        messages.remove(0)
    }

    /// Bundles a [`WasmCompositeBackend`] with everything needed to drive its (optional) drive
    /// member's async completions (mirrors `drive::backend::tests::Harness`) and observe its
    /// (optional) printer member's events (mirrors `printer::tests`). The two members keep their
    /// own independent channels here — composing them onto one shared `RdpInputEvent` stream is
    /// `session.rs`'s job (a plain forwarding task), not something the backend itself needs to
    /// exercise these delegation properties.
    struct Harness {
        backend: WasmCompositeBackend,
        printer_rx: mpsc::UnboundedReceiver<RdpInputEvent>,
        drive_rx: mpsc::UnboundedReceiver<DriveBackendMessage>,
        pool: LocalPool,
    }

    impl Harness {
        fn new(with_printer: bool, with_drive: Option<Rc<MockFs>>) -> Self {
            let pool = LocalPool::new();

            let (printer_tx, printer_rx) = mpsc::unbounded();
            let printer_member = with_printer.then(|| {
                let proxy = WasmPrinterMessageProxy::new(printer_tx);
                (WasmPrinterBackend::new(proxy), PRINTER_DEVICE_ID)
            });

            let (drive_tx, drive_rx) = mpsc::unbounded();
            let drive_member = with_drive.map(|fs| {
                let spawner = pool.spawner();
                let spawn: DriveFsSpawner = Rc::new(move |future| {
                    spawner.spawn_local(future).expect("spawn_local must succeed in tests");
                });
                (WasmDriveBackend::new(drive_tx, fs, false, spawn), DRIVE_DEVICE_ID)
            });

            let backend = WasmCompositeBackend::new(printer_member, drive_member);
            Self {
                backend,
                printer_rx,
                drive_rx,
                pool,
            }
        }

        /// Dispatches a drive IRP and drains any async completion queued by a spawned `DriveFs`
        /// future, exactly like `drive::backend::tests::Harness::dispatch`.
        fn dispatch_drive(&mut self, req: ServerDriveIoRequest) -> Vec<SvcMessage> {
            let mut messages = self
                .backend
                .handle_drive_io_request(req)
                .expect("dispatch must not error");
            self.pool.run_until_stalled();
            while let Ok(DriveBackendMessage::IoCompleted(more)) = self.drive_rx.try_recv() {
                messages.extend(more);
            }
            messages
        }

        fn expect_printer_created(&mut self, expected_file_id: u32) {
            match self.printer_rx.try_recv().expect("expected a queued printer event") {
                RdpInputEvent::Printer(PrinterBackendMessage::Created { file_id }) => {
                    assert_eq!(file_id, expected_file_id);
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }

    #[test]
    fn printer_irp_reaches_printer_member() {
        let mut harness = Harness::new(true, None);

        let response = only(
            harness
                .backend
                .handle_printer_io_request(PrinterIoRequest::Create(printer_create_request(1)))
                .expect("dispatch must not error"),
        );
        assert_eq!(status_of(&response), NtStatus::SUCCESS);
        let file_id = read_u32_at(&encoded(&response), 16);
        assert_ne!(file_id, 0, "printer member must have allocated a real file id");
        harness.expect_printer_created(file_id);
    }

    #[test]
    fn drive_irp_reaches_drive_member() {
        let fs = Rc::new(MockFs::new());
        fs.seed_file("\\hello.txt", b"hello world");
        let mut harness = Harness::new(false, Some(fs));

        let create = only(
            harness.dispatch_drive(ServerDriveIoRequest::ServerCreateDriveRequest(open_request(
                "\\hello.txt",
                1,
            ))),
        );
        assert_eq!(status_of(&create), NtStatus::SUCCESS);
        let file_id = read_u32_at(&encoded(&create), 16);
        assert_ne!(file_id, 0, "drive member must have allocated a real file id");

        let read = only(
            harness.dispatch_drive(ServerDriveIoRequest::DeviceReadRequest(DeviceReadRequest {
                device_io_request: drive_dev_io_req(file_id, 2, MajorFunction::Read),
                length: 32,
                offset: 0,
            })),
        );
        assert_eq!(status_of(&read), NtStatus::SUCCESS);
        let read_bytes = encoded(&read);
        let data_len = read_u32_at(&read_bytes, 16) as usize;
        assert_eq!(&read_bytes[20..20 + data_len], b"hello world");
    }

    #[test]
    fn drive_irp_with_no_drive_member_is_not_supported_not_err() {
        // Printer configured, drive absent — mirrors the printer-only session shape that existed
        // before this task, now routed through the composite.
        let mut harness = Harness::new(true, None);

        let response = only(
            harness
                .backend
                .handle_drive_io_request(ServerDriveIoRequest::DeviceCloseRequest(DeviceCloseRequest {
                    device_io_request: drive_dev_io_req(1, 1, MajorFunction::Close),
                }))
                .expect("absent drive member must answer gracefully, never Err"),
        );
        assert_eq!(status_of(&response), NtStatus::NOT_SUPPORTED);
        // Completion echoes the real completion id, proving the fallback still answers the
        // actual IRP rather than a synthesized one.
        assert_eq!(read_u32_at(&encoded(&response), 8), 1);
    }

    #[test]
    fn printer_irp_with_no_printer_member_is_not_supported_not_err() {
        // Drive configured, printer absent — mirrors the drive-only session shape that existed
        // before this task.
        let fs = Rc::new(MockFs::new());
        let mut harness = Harness::new(false, Some(fs));

        let response = only(
            harness
                .backend
                .handle_printer_io_request(PrinterIoRequest::Close(DeviceCloseRequest {
                    device_io_request: printer_dev_io_req(1, 7, MajorFunction::Close),
                }))
                .expect("absent printer member must answer gracefully, never Err"),
        );
        assert_eq!(status_of(&response), NtStatus::NOT_SUPPORTED);
        assert_eq!(read_u32_at(&encoded(&response), 8), 7);
    }

    #[test]
    fn reset_delegates_to_both_present_members() {
        let fs = Rc::new(MockFs::new());
        fs.seed_file("\\a.txt", b"data");
        let mut harness = Harness::new(true, Some(fs));

        // Open a drive handle and a printer job before reset.
        let drive_created = only(
            harness.dispatch_drive(ServerDriveIoRequest::ServerCreateDriveRequest(open_request(
                "\\a.txt", 1,
            ))),
        );
        assert_eq!(status_of(&drive_created), NtStatus::SUCCESS);
        assert_eq!(
            read_u32_at(&encoded(&drive_created), 16),
            1,
            "drive member's first allocated file id"
        );

        let printer_created = only(
            harness
                .backend
                .handle_printer_io_request(PrinterIoRequest::Create(printer_create_request(1)))
                .unwrap(),
        );
        let printer_file_id = read_u32_at(&encoded(&printer_created), 16);
        harness.expect_printer_created(printer_file_id);

        harness.backend.reset().expect("reset must not error");
        harness.pool.run_until_stalled();

        // Drive member observably reset: a fresh DriveState restarts its file-id counter at 1,
        // exactly like `drive::backend::tests::reset_frees_open_drivefs_handles_...`.
        let recreated = only(
            harness.dispatch_drive(ServerDriveIoRequest::ServerCreateDriveRequest(open_request(
                "\\a.txt", 2,
            ))),
        );
        assert_eq!(status_of(&recreated), NtStatus::SUCCESS);
        assert_eq!(
            read_u32_at(&encoded(&recreated), 16),
            1,
            "reset must have reached the drive member: file_id counter restarts at 1"
        );

        // Printer member still functions correctly through the composite after a reset cycle
        // (its own `reset` is the trait's no-op default today — see this backend's `reset` doc
        // comment — so this asserts "delegation didn't break the member," not "reset cleared its
        // open jobs").
        let post_reset_created = only(
            harness
                .backend
                .handle_printer_io_request(PrinterIoRequest::Create(printer_create_request(2)))
                .unwrap(),
        );
        assert_eq!(status_of(&post_reset_created), NtStatus::SUCCESS);
        let post_reset_file_id = read_u32_at(&encoded(&post_reset_created), 16);
        harness.expect_printer_created(post_reset_file_id);
    }

    #[test]
    fn resolve_drive_device_id_no_collision_is_unchanged() {
        assert_eq!(resolve_drive_device_id(1, 2), 1);
    }

    #[test]
    fn resolve_drive_device_id_collision_shifts_past_printer_id() {
        assert_eq!(resolve_drive_device_id(1, 1), 2);
        assert_eq!(resolve_drive_device_id(5, 5), 6);
    }

    #[test]
    fn resolve_drive_device_id_collision_at_u32_max_falls_back_below() {
        assert_eq!(resolve_drive_device_id(u32::MAX, u32::MAX), u32::MAX - 1);
    }

    #[test]
    fn announce_response_for_unknown_device_id_does_not_error() {
        let fs = Rc::new(MockFs::new());
        let mut harness = Harness::new(true, Some(fs));

        harness
            .backend
            .handle_server_device_announce_response(ServerDeviceAnnounceResponse {
                device_id: 99,
                result_code: NtStatus::SUCCESS,
            })
            .expect("an unrecognized device id must be logged, not errored");
    }

    #[test]
    fn announce_response_routes_to_each_known_member_without_error() {
        let fs = Rc::new(MockFs::new());
        let mut harness = Harness::new(true, Some(fs));

        harness
            .backend
            .handle_server_device_announce_response(ServerDeviceAnnounceResponse {
                device_id: DRIVE_DEVICE_ID,
                result_code: NtStatus::SUCCESS,
            })
            .expect("drive device id must route to the drive member");
        harness
            .backend
            .handle_server_device_announce_response(ServerDeviceAnnounceResponse {
                device_id: PRINTER_DEVICE_ID,
                result_code: NtStatus::SUCCESS,
            })
            .expect("printer device id must route to the printer member");
    }
}
