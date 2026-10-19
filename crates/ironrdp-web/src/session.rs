use core::cell::{Cell, RefCell};
use core::net::{Ipv4Addr, SocketAddrV4};
use core::num::NonZeroU32;
use core::time::Duration;
use std::borrow::Cow;
use std::rc::Rc;

use anyhow::Context as _;
use base64::Engine as _;
use futures_channel::mpsc;
use futures_util::io::{ReadHalf, WriteHalf};
use futures_util::{AsyncWriteExt as _, FutureExt as _, StreamExt as _, select};
use gloo_net::websocket;
use gloo_net::websocket::futures::WebSocket;
use gloo_timers::future::IntervalStream;
use iron_remote_desktop::{CursorStyle, DesktopSize, Extension, IronErrorKind};
use ironrdp::cliprdr::CliprdrClient;
use ironrdp::cliprdr::backend::ClipboardMessage;
use ironrdp::cliprdr::pdu::{FileContentsFlags, FileContentsRequest, FileContentsResponse, FileDescriptor};
use ironrdp::connector::connection_activation::ConnectionActivationState;
use ironrdp::connector::credssp::KerberosConfig;
use ironrdp::connector::{self, ClientConnector, Credentials};
use ironrdp::displaycontrol::client::DisplayControlClient;
use ironrdp::displaycontrol::pdu::MonitorLayoutEntry;
use ironrdp::dvc::DrdynvcClient;
use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp::pdu::input::fast_path::FastPathInputEvent;
use ironrdp::pdu::rdp::capability_sets::{BitmapCodecs, client_codecs_capabilities};
use ironrdp::pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp::pdu::rdp::server_redirection::{RedirectionFlags, ServerRedirectionPdu};
use ironrdp::pdu::rdstls::RdstlsAuthenticationRequest;
use ironrdp::rdpdr::Rdpdr;
use ironrdp::rdpdr::pdu::efs::{DEFAULT_PRINTER_DRIVER_NAME, MICROSOFT_PRINT_TO_PDF_DRIVER_NAME};
use ironrdp::rdpsnd::client::{NoopRdpsndBackend, Rdpsnd, RdpsndClientHandler};
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStageBuilder, ActiveStageOutput, GracefulDisconnectReason};
use ironrdp_core::WriteBuf;
use ironrdp_egfx::client::{FrameAckPolicy, GraphicsPipelineClient, GraphicsPipelineHandler};
use ironrdp_egfx::decode::H264Decoder;
use ironrdp_egfx::pdu::{CapabilitiesV8Flags, CapabilitiesV81Flags, CapabilitiesV107Flags, CapabilitySet};
use ironrdp_futures::{FramedWrite as _, single_sequence_step, single_sequence_step_read};
use rgb::AsPixels as _;
use tap::prelude::*;
use tracing::{debug, error, info, trace, warn};
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::spawn_local;
use web_sys::{FileSystemDirectoryHandle, HtmlCanvasElement};

use crate::audio::WasmRdpsndBackend;
use crate::canvas::{Canvas, Presenter};
use crate::clipboard;
use crate::clipboard::{ClipboardData, FileMetadata, WasmClipboard, WasmClipboardBackend, WasmClipboardBackendMessage};
use crate::drive::backend::{DriveBackendMessage, WasmDriveBackend, wasm_drive_pair};
use crate::drive::composite::{DriveSlot, WasmCompositeBackend, resolve_drive_device_id};
use crate::drive::fs::DriveFs;
use crate::drive::web_fs::WebFsDrive;
use crate::error::IronError;
use crate::image::extract_partial_image;
use crate::input::InputTransaction;
use crate::network_client::WasmNetworkClient;
use crate::printer::{JsPrinterStreamCallbacks, WasmPrinter, WasmPrinterBackend, wasm_printer_pair};

/// The RDP byte stream once TLS is up: everything after the handshake, including the
/// whole active session, runs through it.
pub(crate) type RdpStream = crate::tls::TlsStream<WebSocket>;

type RdpFramed = ironrdp_futures::LocalFuturesFramed<RdpStream>;

const DEFAULT_WIDTH: u16 = 1280;
const DEFAULT_HEIGHT: u16 = 720;

#[derive(Clone, Copy)]
struct SecurityConfig {
    enable_credssp: bool,
    enable_standard_rdp_security: bool,
}

/// Default device id used to announce the redirected drive share on the Rdpdr channel.
/// The printer branch's own `printer_device_id` falls back to `2` specifically
/// to leave `1` free for drive redirection (see that fallback in
/// `SessionBuilder::connect`) — this constant is the other half of that plan.
/// `printerDeviceId()` is caller-settable, though, so a session that explicitly configures
/// `printer_device_id == DRIVE_DEVICE_ID` is still possible; `connect` resolves that collision
/// via [`resolve_drive_device_id`] before announcing either device.
const DRIVE_DEVICE_ID: u32 = 1;

#[derive(Clone, Default)]
pub(crate) struct SessionBuilder(Rc<RefCell<SessionBuilderInner>>);

struct SessionBuilderInner {
    username: Option<String>,
    destination: Option<String>,
    server_domain: Option<String>,
    password: Option<String>,
    proxy_address: Option<String>,
    auth_token: Option<String>,
    pcb: Option<String>,
    vmconnect: Option<String>,
    kdc_proxy_url: Option<String>,
    client_name: String,
    desktop_size: DesktopSize,

    render_canvas: Option<HtmlCanvasElement>,
    set_cursor_style_callback: Option<js_sys::Function>,
    set_cursor_style_callback_context: Option<JsValue>,
    remote_clipboard_changed_callback: Option<js_sys::Function>,
    force_clipboard_update_callback: Option<js_sys::Function>,
    // File transfer callbacks
    files_available_callback: Option<js_sys::Function>,
    file_contents_request_callback: Option<js_sys::Function>,
    file_contents_response_callback: Option<js_sys::Function>,
    lock_callback: Option<js_sys::Function>,
    unlock_callback: Option<js_sys::Function>,
    locks_expired_callback: Option<js_sys::Function>,
    format_list_response_callback: Option<js_sys::Function>,

    // Setting printer stream callbacks activates the virtual printer.
    invalid_print_job_stream_callbacks: bool,
    print_job_stream_callbacks: Option<JsPrinterStreamCallbacks>,
    printer_name: Option<String>,
    printer_device_id: Option<u32>,
    printer_driver_name: Option<String>,

    // Setting a valid `driveShare` extension activates drive redirection.
    invalid_drive_share: bool,
    drive_share: Option<DriveShareConfig>,

    use_display_control: bool,
    /// Whether to attach the EGFX graphics pipeline. Defaults to `true`: EGFX is
    /// negotiated, so a host that does not support it simply never opens the
    /// channel and the legacy bitmap path continues untouched. The `egfx`
    /// extension can set this to `false` as a kill-switch.
    use_egfx: bool,
    /// Decode H.264 (AVC420) with the browser's WebCodecs `VideoDecoder`, letting Windows
    /// encode the desktop as H.264. Off by default; set by the `h264` extension. Ignored
    /// when EGFX is off or the browser cannot decode H.264.
    use_h264: bool,
    /// Render EGFX on the GPU (WebGL2), so H.264 frames are never read back to the CPU. Off
    /// by default; set by the `gpu_rendering` extension. Falls back to the 2D canvas when
    /// WebGL2 is unavailable or the desktop exceeds the GPU's texture limit.
    use_gpu_rendering: bool,
    /// Acknowledge EGFX frames the way Windows App does: suspend acknowledgements while the
    /// client keeps up, so Windows does not pace itself to them, and send QoE reports
    /// instead. Off by default, set by the `frame_ack_suspend` extension: without the
    /// acknowledgement brake Windows can outrun a slow link (a tunnel over the internet),
    /// and the backlog in the network starves audio on the same connection.
    frame_ack_suspend: bool,
    enable_audio_playback: bool,
    enable_credssp: bool,
    enable_standard_rdp_security: bool,
    enable_server_pointer: bool,
    legacy_graphics: bool,
    outbound_message_size_limit: Option<usize>,
    /// Client Info `performanceFlags` (MS-RDPBCGR 2.2.1.11.1.1.1); `None` keeps
    /// [`PerformanceFlags::default`].
    performance_flags: Option<PerformanceFlags>,
    /// Client core data `desktopScaleFactor` (MS-RDPBCGR 2.2.1.3.2), in percent; `None`
    /// sends none. Later scale changes travel with each resize request.
    desktop_scale_factor: Option<u32>,
}

impl Default for SessionBuilderInner {
    fn default() -> Self {
        Self {
            username: None,
            destination: None,
            server_domain: None,
            password: None,
            proxy_address: None,
            auth_token: None,
            pcb: None,
            vmconnect: None,
            kdc_proxy_url: None,
            client_name: "ironrdp-web".to_owned(),
            desktop_size: DesktopSize {
                width: DEFAULT_WIDTH,
                height: DEFAULT_HEIGHT,
            },

            render_canvas: None,
            set_cursor_style_callback: None,
            set_cursor_style_callback_context: None,
            remote_clipboard_changed_callback: None,
            force_clipboard_update_callback: None,
            files_available_callback: None,
            file_contents_request_callback: None,
            file_contents_response_callback: None,
            lock_callback: None,
            unlock_callback: None,
            locks_expired_callback: None,
            format_list_response_callback: None,

            invalid_print_job_stream_callbacks: false,
            print_job_stream_callbacks: None,
            printer_name: None,
            printer_device_id: None,
            printer_driver_name: None,

            invalid_drive_share: false,
            drive_share: None,

            use_display_control: false,
            use_egfx: true,
            use_h264: false,
            use_gpu_rendering: false,
            frame_ack_suspend: false,
            enable_audio_playback: false,
            performance_flags: None,
            desktop_scale_factor: None,
            enable_credssp: true,
            enable_standard_rdp_security: false,
            enable_server_pointer: true,
            legacy_graphics: false,
            outbound_message_size_limit: None,
        }
    }
}

impl iron_remote_desktop::SessionBuilder for SessionBuilder {
    type Session = Session;
    type Error = IronError;

    fn create() -> Self {
        Self(Rc::new(RefCell::new(SessionBuilderInner::default())))
    }

    /// Required
    fn username(&self, username: String) -> Self {
        self.0.borrow_mut().username = Some(username);
        self.clone()
    }

    /// Required
    fn destination(&self, destination: String) -> Self {
        self.0.borrow_mut().destination = Some(destination);
        self.clone()
    }

    /// Optional
    fn server_domain(&self, server_domain: String) -> Self {
        self.0.borrow_mut().server_domain = if server_domain.is_empty() {
            None
        } else {
            Some(server_domain)
        };
        self.clone()
    }

    /// Required
    fn password(&self, password: String) -> Self {
        self.0.borrow_mut().password = Some(password);
        self.clone()
    }

    /// Required
    fn proxy_address(&self, address: String) -> Self {
        self.0.borrow_mut().proxy_address = Some(address);
        self.clone()
    }

    /// Required
    fn auth_token(&self, token: String) -> Self {
        self.0.borrow_mut().auth_token = Some(token);
        self.clone()
    }

    /// Optional
    fn desktop_size(&self, desktop_size: DesktopSize) -> Self {
        self.0.borrow_mut().desktop_size = desktop_size;
        self.clone()
    }

    /// Optional
    fn render_canvas(&self, canvas: HtmlCanvasElement) -> Self {
        self.0.borrow_mut().render_canvas = Some(canvas);
        self.clone()
    }

    /// Required.
    ///
    /// # Callback signature:
    /// ```typescript
    /// function callback(
    ///     cursor_kind: string,
    ///     cursor_data: string | undefined,
    ///     hotspot_x: number | undefined,
    ///     hotspot_y: number | undefined
    /// ): void
    /// ```
    ///
    /// # Cursor kinds:
    /// - `default` (default system cursor); other arguments are `UNDEFINED`
    /// - `none` (hide cursor); other arguments are `UNDEFINED`
    /// - `url` (custom cursor data URL); `cursor_data` contains the data URL with Base64-encoded
    ///   cursor bitmap; `hotspot_x` and `hotspot_y` are set to the cursor hotspot coordinates.
    fn set_cursor_style_callback(&self, callback: js_sys::Function) -> Self {
        self.0.borrow_mut().set_cursor_style_callback = Some(callback);
        self.clone()
    }

    /// Required.
    fn set_cursor_style_callback_context(&self, context: JsValue) -> Self {
        self.0.borrow_mut().set_cursor_style_callback_context = Some(context);
        self.clone()
    }

    /// Optional
    fn remote_clipboard_changed_callback(&self, callback: js_sys::Function) -> Self {
        self.0.borrow_mut().remote_clipboard_changed_callback = Some(callback);
        self.clone()
    }

    /// Optional
    fn force_clipboard_update_callback(&self, callback: js_sys::Function) -> Self {
        self.0.borrow_mut().force_clipboard_update_callback = Some(callback);
        self.clone()
    }

    /// Because the server does not resize the framebuffer in the RDP protocol, this feature is unused in IronRDP.
    fn canvas_resized_callback(&self, _callback: js_sys::Function) -> Self {
        self.clone()
    }

    fn extension(&self, ext: Extension) -> Self {
        iron_remote_desktop::extension_match! {
            match ext;
            |pcb: String| { self.0.borrow_mut().pcb = Some(pcb) };
            |vmconnect: String| { self.0.borrow_mut().vmconnect = Some(vmconnect) };
            |kdc_proxy_url: String| { self.0.borrow_mut().kdc_proxy_url = Some(kdc_proxy_url) };
            |display_control: bool| { self.0.borrow_mut().use_display_control = display_control };
            |egfx: bool| { self.0.borrow_mut().use_egfx = egfx };
            |h264: bool| { self.0.borrow_mut().use_h264 = h264 };
            |gpu_rendering: bool| { self.0.borrow_mut().use_gpu_rendering = gpu_rendering };
            |frame_ack_suspend: bool| { self.0.borrow_mut().frame_ack_suspend = frame_ack_suspend };

            |enable_audio_playback: bool| { self.0.borrow_mut().enable_audio_playback = enable_audio_playback };
            |enable_credssp: bool| { self.0.borrow_mut().enable_credssp = enable_credssp };
            |enable_standard_rdp_security: bool| {
                self.0.borrow_mut().enable_standard_rdp_security = enable_standard_rdp_security;
            };
            |enable_server_pointer: bool| { self.0.borrow_mut().enable_server_pointer = enable_server_pointer };
            |legacy_graphics: bool| { self.0.borrow_mut().legacy_graphics = legacy_graphics };
            |performance_flags: f64| {
                let flags = performance_flags_from_js(performance_flags);
                if flags.is_none() {
                    warn!(performance_flags, "Invalid performance flags; keeping the defaults");
                }
                self.0.borrow_mut().performance_flags = flags;
            };
            |desktop_scale_factor: f64| {
                let scale = desktop_scale_factor_from_js(desktop_scale_factor);
                if scale.is_none() {
                    warn!(desktop_scale_factor, "Invalid desktop scale factor; sending none");
                }
                self.0.borrow_mut().desktop_scale_factor = scale;
            };
            |outbound_message_size_limit: f64| {
                let limit = if outbound_message_size_limit >= 0.0 && outbound_message_size_limit <= f64::from(u32::MAX) {
                    #[expect(clippy::as_conversions, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    { outbound_message_size_limit as usize }
                } else {
                    warn!(outbound_message_size_limit, "Invalid outbound message size limit; fallback to unlimited");
                    0 // Fallback to no limit for invalid values.
                };
                self.0.borrow_mut().outbound_message_size_limit = if limit > 0 { Some(limit) } else { None };
            };
            // File transfer callbacks - protocol-specific, routed through extension()
            // rather than dedicated trait methods to keep iron-remote-desktop protocol-agnostic.
            |files_available_callback: JsValue| {
                self.0.borrow_mut().files_available_callback = files_available_callback.dyn_into::<js_sys::Function>().ok();
            };
            |file_contents_request_callback: JsValue| {
                self.0.borrow_mut().file_contents_request_callback = file_contents_request_callback.dyn_into::<js_sys::Function>().ok();
            };
            |file_contents_response_callback: JsValue| {
                self.0.borrow_mut().file_contents_response_callback = file_contents_response_callback.dyn_into::<js_sys::Function>().ok();
            };
            |lock_callback: JsValue| {
                self.0.borrow_mut().lock_callback = lock_callback.dyn_into::<js_sys::Function>().ok();
            };
            |unlock_callback: JsValue| {
                self.0.borrow_mut().unlock_callback = unlock_callback.dyn_into::<js_sys::Function>().ok();
            };
            |locks_expired_callback: JsValue| {
                self.0.borrow_mut().locks_expired_callback = locks_expired_callback.dyn_into::<js_sys::Function>().ok();
            };
            |format_list_response_callback: JsValue| {
                self.0.borrow_mut().format_list_response_callback = format_list_response_callback.dyn_into::<js_sys::Function>().ok();
            };
            |print_job_stream_callbacks: JsValue| {
                let mut inner = self.0.borrow_mut();
                match parse_print_job_stream_callbacks(print_job_stream_callbacks) {
                    Ok(callbacks) => {
                        inner.invalid_print_job_stream_callbacks = false;
                        inner.print_job_stream_callbacks = Some(callbacks);
                    }
                    Err(error) => {
                        inner.invalid_print_job_stream_callbacks = true;
                        inner.print_job_stream_callbacks = None;
                        warn!(%error, "Invalid print_job_stream_callbacks; printer streaming requires onJobData and onJobComplete functions");
                    }
                }
            };
            |printer_name: String| {
                let mut inner = self.0.borrow_mut();
                inner.printer_name = if printer_name.is_empty() { None } else { Some(printer_name) };
            };
            |printer_device_id: f64| {
                let id = if printer_device_id >= 0.0 && printer_device_id <= f64::from(u32::MAX) {
                    #[expect(clippy::as_conversions, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    { printer_device_id as u32 }
                } else {
                    warn!(printer_device_id, "Invalid printer_device_id; falling back to default");
                    0
                };
                let mut inner = self.0.borrow_mut();
                inner.printer_device_id = if id > 0 { Some(id) } else { None };
            };
            |printer_driver_name: String| {
                let mut inner = self.0.borrow_mut();
                inner.printer_driver_name = if printer_driver_name.is_empty() {
                    None
                } else {
                    Some(printer_driver_name)
                };
            };
            |drive_share: JsValue| {
                let mut inner = self.0.borrow_mut();
                match parse_drive_share(drive_share) {
                    Ok(config) => {
                        inner.invalid_drive_share = false;
                        inner.drive_share = Some(config);
                    }
                    Err(error) => {
                        inner.invalid_drive_share = true;
                        inner.drive_share = None;
                        warn!(%error, "Invalid drive_share; drive redirection requires a valid {{ handle, shareName }} object");
                    }
                }
            };
        }

        self.clone()
    }

    async fn connect(&self) -> Result<Self::Session, Self::Error> {
        self.connect_with(None, None).await
    }
}

impl SessionBuilder {
    /// Connects, either fresh (`redirect` and `reuse` both `None`) or following a
    /// Server Redirection PDU, in which case the live session's input channel and
    /// drive slot carry over so nothing JS holds goes stale.
    async fn connect_with(&self, redirect: Option<Redirect>, reuse: Option<Reuse>) -> Result<Session, IronError> {
        let (
            username,
            destination,
            server_domain,
            password,
            proxy_address,
            vmconnect,
            kdc_proxy_url,
            client_name,
            desktop_size,
            render_canvas,
            set_cursor_style_callback,
            set_cursor_style_callback_context,
            remote_clipboard_changed_callback,
            force_clipboard_update_callback,
            files_available_callback,
            file_contents_request_callback,
            file_contents_response_callback,
            lock_callback,
            unlock_callback,
            locks_expired_callback,
            format_list_response_callback,
            invalid_print_job_stream_callbacks,
            print_job_stream_callbacks,
            printer_name,
            printer_device_id,
            printer_driver_name,
            invalid_drive_share,
            drive_share,
            outbound_message_size_limit,
            legacy_graphics,
            security,
        );

        {
            let inner = self.0.borrow();

            username = inner.username.clone().context("username missing")?;
            destination = inner.destination.clone().context("destination missing")?;
            server_domain = inner.server_domain.clone();
            password = inner.password.clone().context("password missing")?;
            proxy_address = inner.proxy_address.clone().context("proxy_address missing")?;
            vmconnect = inner.vmconnect.clone();
            kdc_proxy_url = inner.kdc_proxy_url.clone();
            client_name = inner.client_name.clone();
            // A redirected connection continues the session at the size it had
            // reached, not the one requested at connect: nothing on the JS side
            // re-sends a resize the previous connection already applied.
            desktop_size = reuse.as_ref().map_or(inner.desktop_size, |reuse| reuse.desktop_size);

            render_canvas = inner.render_canvas.clone().context("render_canvas missing")?;

            set_cursor_style_callback = inner
                .set_cursor_style_callback
                .clone()
                .context("set_cursor_style_callback missing")?;
            set_cursor_style_callback_context = inner
                .set_cursor_style_callback_context
                .clone()
                .context("set_cursor_style_callback_context missing")?;
            remote_clipboard_changed_callback = inner.remote_clipboard_changed_callback.clone();
            force_clipboard_update_callback = inner.force_clipboard_update_callback.clone();
            files_available_callback = inner.files_available_callback.clone();
            file_contents_request_callback = inner.file_contents_request_callback.clone();
            file_contents_response_callback = inner.file_contents_response_callback.clone();
            lock_callback = inner.lock_callback.clone();
            unlock_callback = inner.unlock_callback.clone();
            locks_expired_callback = inner.locks_expired_callback.clone();
            format_list_response_callback = inner.format_list_response_callback.clone();
            invalid_print_job_stream_callbacks = inner.invalid_print_job_stream_callbacks;
            print_job_stream_callbacks = inner.print_job_stream_callbacks.clone();
            printer_name = inner.printer_name.clone();
            printer_device_id = inner.printer_device_id;
            printer_driver_name = inner.printer_driver_name.clone();
            invalid_drive_share = inner.invalid_drive_share;
            drive_share = inner.drive_share.clone();
            outbound_message_size_limit = inner.outbound_message_size_limit;
            legacy_graphics = inner.legacy_graphics;
            security = SecurityConfig {
                enable_credssp: inner.enable_credssp,
                enable_standard_rdp_security: inner.enable_standard_rdp_security,
            };
        }

        // A redirection hands over its own identity; the password is only replaced
        // when the server sent one the client can read (RDSTLS carries the opaque
        // one instead).
        let (username, password, server_domain) = match &redirect {
            Some(redirect) => (
                redirect.username.clone().unwrap_or(username),
                redirect.password.clone().unwrap_or(password),
                redirect.domain.clone().or(server_domain),
            ),
            None => (username, password, server_domain),
        };

        let redirect = redirect.map(|redirect| RedirectConnector {
            routing_token: redirect.routing_token,
            rdstls: redirect
                .rdstls
                .map(|(redirection_guid, password)| RdstlsAuthenticationRequest {
                    redirection_guid,
                    username: username.clone(),
                    domain: server_domain.clone(),
                    password,
                }),
        });

        info!(redirected = redirect.is_some(), "Connect to RDP host");

        // Read before `build_config`: the EGFX early capability flag has to go out
        // in the GCC client core data during the connection sequence, long before
        // the DRDYNVC attach below decides whether to register the channel.
        // `legacy_graphics` asks for a codec-free session, so it wins over EGFX.
        let use_egfx = self.0.borrow().use_egfx && !legacy_graphics;

        let mut config = build_config(
            username,
            password,
            server_domain,
            client_name.clone(),
            desktop_size,
            legacy_graphics,
            security,
            use_egfx,
        );

        let enable_server_pointer = self.0.borrow().enable_server_pointer;
        config.enable_server_pointer = enable_server_pointer;
        if let Some(performance_flags) = self.0.borrow().performance_flags {
            config.performance_flags = performance_flags;
        }
        if let Some(desktop_scale_factor) = self.0.borrow().desktop_scale_factor {
            config.desktop_scale_factor = desktop_scale_factor;
        }

        let (input_events_tx, input_events_rx, drive_slot) = match reuse {
            Some(Reuse {
                input_events_tx,
                input_events_rx,
                drive_slot,
                desktop_size: _,
            }) => (input_events_tx, input_events_rx, drive_slot),
            None => {
                let (tx, rx) = mpsc::unbounded();
                // Shared with the live composite backend so a folder picked mid-session can be
                // installed into it (see `invoke_extension`'s `drive_share` arm).
                let drive_slot: DriveSlot = Rc::new(RefCell::new(None));
                (tx, rx, drive_slot)
            }
        };

        // Create the AudioContext as early as possible, while the page's user activation from
        // the Connect click is freshest (autoplay policy). Audio failing must not fail the
        // connection: fall back to the no-op backend and leave `INFO_NOAUDIOPLAYBACK` set, so the
        // server neither redirects audio nor plays it (MS-RDPBCGR 2.2.1.11.1.1).
        let rdpsnd_backend = if self.0.borrow().enable_audio_playback {
            match WasmRdpsndBackend::new() {
                Ok(backend) => Some(backend),
                Err(error) => {
                    warn!(%error, "Audio playback unavailable; continuing without sound");
                    None
                }
            }
        } else {
            None
        };
        config.enable_audio_playback = rdpsnd_backend.is_some();

        // Created before anything draws: the canvas carries one context type for good.
        let gpu_backend = if use_egfx && self.0.borrow().use_gpu_rendering {
            match crate::gpu::GlBackend::new(&render_canvas, desktop_size.width, desktop_size.height) {
                Ok(gl) => {
                    info!("Rendering EGFX on the GPU (WebGL2)");
                    Some(gl)
                }
                Err(error) => {
                    warn!(%error, "GPU rendering unavailable; drawing on the CPU");
                    None
                }
            }
        } else {
            None
        };
        let frame_output = if gpu_backend.is_some() {
            crate::h264::FrameOutput::Gpu
        } else {
            crate::h264::FrameOutput::Readback
        };

        // Probed after the AudioContext so audio keeps the Connect click's user activation.
        let h264_decoder: Option<Box<dyn H264Decoder>> = if use_egfx && self.0.borrow().use_h264 {
            if crate::h264::probe_support().await {
                let wake_tx = input_events_tx.clone();
                Some(Box::new(crate::h264::WebCodecsH264Decoder::new(
                    move || {
                        // The session loop owns the receiver; a closed channel means the session ended.
                        let _ = wake_tx.unbounded_send(RdpInputEvent::GraphicsDecoded);
                    },
                    frame_output,
                )))
            } else {
                info!("WebCodecs H.264 unavailable; EGFX stays on the CPU codecs");
                None
            }
        } else {
            None
        };

        let clipboard = remote_clipboard_changed_callback.clone().map(|callback| {
            WasmClipboard::new(
                clipboard::WasmClipboardMessageProxy::new(input_events_tx.clone()),
                clipboard::JsClipboardCallbacks {
                    on_remote_clipboard_changed: callback,
                    on_force_clipboard_update: force_clipboard_update_callback,
                    on_files_available: files_available_callback,
                    on_file_contents_request: file_contents_request_callback,
                    on_file_contents_response: file_contents_response_callback,
                    on_lock: lock_callback,
                    on_unlock: unlock_callback,
                    on_locks_expired: locks_expired_callback,
                    on_format_list_response: format_list_response_callback,
                },
            )
        });

        if invalid_print_job_stream_callbacks {
            return Err(IronError::from(anyhow::anyhow!(
                "printer redirection requires valid print_job_stream_callbacks"
            )));
        }

        // Build the virtual-printer pair when JS printer callbacks were
        // registered via extension(). Backend is Send (holds the mpsc proxy
        // only) and goes into the SVC processor below; the front-end
        // `WasmPrinter` owns the JS callbacks and lives on `Session`.
        let (printer_backend, printer) = match print_job_stream_callbacks {
            Some(callbacks) => {
                let (backend, printer) = wasm_printer_pair(input_events_tx.clone(), callbacks);
                (Some(backend), Some(printer))
            }
            None => (None, None),
        };

        // Default to 2 to avoid a potential collision if drive redirection is
        // enabled in the same session.
        let printer_device_id = printer_device_id.unwrap_or(2);
        let printer_name = printer_name.unwrap_or_else(|| "IronRDP Virtual Printer".to_owned());
        let printer_driver_name = printer_driver_name.unwrap_or_else(default_printer_driver_name);

        if invalid_drive_share {
            return Err(IronError::from(anyhow::anyhow!(
                "drive redirection requires a valid driveShare extension ({{ handle, shareName }})"
            )));
        }

        // Build the virtual-drive pair when a `driveShare` extension was
        // registered via extension(). `wasm_drive_pair`'s sender carries
        // `DriveBackendMessage`, a type deliberately decoupled from
        // `RdpInputEvent` (see that enum's own doc comment) — this task's job is
        // exactly this adapter, forwarding each completion onto the session's
        // shared event channel. The forwarding task ends on its own once
        // `drive_backend` (moved into the Rdpdr static channel in `connect`
        // below) is dropped and closes `drive_tx`, so nothing here can leak a
        // forever-loop past the session's lifetime.
        let drive = match drive_share {
            Some(config) => {
                let fs: Rc<dyn DriveFs> = Rc::new(WebFsDrive::new(config.handle));
                let (drive_tx, mut drive_rx) = mpsc::unbounded();
                let drive_backend = wasm_drive_pair(drive_tx, fs, config.read_only);

                let forward_tx = input_events_tx.clone();
                spawn_local(async move {
                    while let Some(message) = drive_rx.next().await {
                        if forward_tx
                            .unbounded_send(RdpInputEvent::DriveBackend(SyncDriveBackendMessage(message)))
                            .is_err()
                        {
                            break;
                        }
                    }
                });

                Some((drive_backend, config.share_name))
            }
            None => None,
        };

        let ws = WebSocket::open(&proxy_address).context("couldn't open WebSocket")?;

        // NOTE: ideally, when the WebSocket can't be opened, the above call should fail with details on why is that
        // (e.g., the proxy hostname could not be resolved, proxy service is not running), but errors are neved
        // bubbled up in practice, so instead we poll the WebSocket state until we know its connected (i.e., the
        // WebSocket handshake is a success and user data can be exchanged).
        loop {
            match ws.state() {
                websocket::State::Closing | websocket::State::Closed => {
                    return Err(IronError::from(anyhow::anyhow!(
                        "failed to connect to {proxy_address} (WebSocket is `{:?}`)",
                        ws.state()
                    ))
                    .with_kind(IronErrorKind::ProxyConnect));
                }
                websocket::State::Connecting => {
                    trace!("WebSocket is connecting to proxy at {proxy_address}...");
                    gloo_timers::future::sleep(Duration::from_millis(50)).await;
                }
                websocket::State::Open => {
                    debug!("WebSocket connected to {proxy_address} with success");
                    break;
                }
            }
        }

        let use_display_control = self.0.borrow().use_display_control;

        let (connection_result, stream, rdp_leftover) = connect(ConnectParams {
            ws,
            config,
            destination,
            vmconnect,
            kdc_proxy_url,
            clipboard_backend: clipboard.as_ref().map(|clip| clip.backend()),
            printer_backend,
            printer_device_id,
            printer_name,
            printer_driver_name,
            drive,
            drive_slot: Rc::clone(&drive_slot),
            computer_name: client_name.clone(),
            use_display_control,
            use_egfx,
            h264_decoder,
            gpu_backend: gpu_backend.clone(),
            frame_ack_policy: if self.0.borrow().frame_ack_suspend {
                FrameAckPolicy::SuspendWhenCaughtUp
            } else {
                FrameAckPolicy::EveryFrame
            },
            redirect,
            rdpsnd_backend,
        })
        .await?;

        info!("Connected!");

        let (rdp_reader, rdp_writer) = futures_util::AsyncReadExt::split(stream);

        let (writer_tx, writer_rx) = mpsc::unbounded();

        spawn_local(writer_task(writer_rx, rdp_writer, outbound_message_size_limit));

        Ok(Session {
            builder: self.clone(),
            desktop_size: Cell::new(connection_result.desktop_size),
            input_database: RefCell::new(ironrdp::input::Database::new()),
            writer_tx: RefCell::new(writer_tx),
            input_events_tx,

            render_canvas,
            gpu: RefCell::new(gpu_backend),
            set_cursor_style_callback,
            set_cursor_style_callback_context,

            input_events_rx: RefCell::new(Some(input_events_rx)),
            rdp_reader: RefCell::new(Some((rdp_reader, rdp_leftover))),
            connection_result: RefCell::new(Some(connection_result)),
            clipboard: RefCell::new(Some(clipboard)),
            printer: RefCell::new(Some(printer)),
            drive_slot,
            drive_device_id: resolve_drive_device_id(DRIVE_DEVICE_ID, printer_device_id),
        })
    }
}

pub(crate) type FastPathInputEvents = smallvec::SmallVec<[FastPathInputEvent; 2]>;

/// Wraps a [`DriveBackendMessage`] so `RdpInputEvent` stays `Sync`, which
/// `anyhow::Context`'s blanket impl requires at every other `.context(...)`
/// call site on an `unbounded_send` result in this file (the bound is on the
/// whole enum, not the particular variant in use). `DriveBackendMessage`
/// carries `ironrdp_svc::SvcMessage`, which boxes a `dyn SvcEncode` trait
/// object; `Send` is a supertrait of `SvcEncode` (so it *is* inherited by the
/// trait object automatically), but `Sync` is not, so `DriveBackendMessage`
/// itself isn't `Sync`. Safe to assert here for the same reason
/// `WasmDriveBackend`'s own `unsafe impl Send` is safe (see its doc comment):
/// this wasm client never shares a value across a real OS thread.
#[derive(Debug)]
pub(crate) struct SyncDriveBackendMessage(DriveBackendMessage);
// SAFETY: see the doc comment above.
unsafe impl Sync for SyncDriveBackendMessage {}

#[derive(Debug)]
pub(crate) enum RdpInputEvent {
    Cliprdr(ClipboardMessage),
    ClipboardBackend(WasmClipboardBackendMessage),
    /// Printer backend → event loop: a print job finished and its bytes are
    /// ready for delivery to JS. See [`crate::printer::PrinterBackendMessage`].
    Printer(crate::printer::PrinterBackendMessage),
    /// Drive backend → event loop (via the forwarding task spawned in
    /// `SessionBuilder::connect`): an async `DriveFs` operation completed and
    /// produced RDPDR completion PDU(s) that must be injected on the Rdpdr
    /// static channel. See [`DriveBackendMessage`].
    DriveBackend(SyncDriveBackendMessage),
    /// A folder was shared mid-session: the backend is already staged in the
    /// session's [`DriveSlot`], so the loop only has to announce the device.
    DriveHotplug { device_id: u32, share_name: String },
    FastPath(FastPathInputEvents),
    /// The WebCodecs H.264 decoder finished a frame the EGFX pipeline is waiting on.
    GraphicsDecoded,
    Resize {
        width: u32,
        height: u32,
        scale_factor: Option<u32>,
        physical_size: Option<(u32, u32)>,
    },
    TerminateSession,
}

pub(crate) struct SessionTerminationInfo {
    reason: GracefulDisconnectReason,
}

impl iron_remote_desktop::SessionTerminationInfo for SessionTerminationInfo {
    fn reason(&self) -> String {
        self.reason.to_string()
    }
}

/// What a Server Redirection PDU asks of the next connection.
///
/// The target is always the same relay: in direct mode the WebSocket address names
/// the RDP host, so a redirection that names a different host cannot be followed
/// and is attempted against the current one (GNOME Remote Desktop, the case this
/// exists for, hands over on the same address and names no target).
struct Redirect {
    routing_token: Option<String>,
    username: Option<String>,
    domain: Option<String>,
    /// Readable password, for a redirection that continues with CredSSP.
    password: Option<String>,
    /// `(redirection GUID, password blob)` when the password is opaque and the
    /// next connection must authenticate with RDSTLS.
    rdstls: Option<(Vec<u8>, Vec<u8>)>,
}

impl Redirect {
    fn from_pdu(pdu: ServerRedirectionPdu) -> Self {
        if pdu.target_net_address.is_some() || pdu.target_fqdn.is_some() || pdu.target_net_addresses.is_some() {
            warn!(
                target_net_address = ?pdu.target_net_address,
                target_fqdn = ?pdu.target_fqdn,
                "Server Redirection names a target host; direct mode can only reconnect through the same relay"
            );
        }

        let rdstls = pdu.flags.contains(RedirectionFlags::PASSWORD_IS_PK_ENCRYPTED).then(|| {
            (
                pdu.redirection_guid.clone().unwrap_or_default(),
                pdu.password.clone().unwrap_or_default(),
            )
        });

        Self {
            routing_token: pdu.routing_token(),
            password: pdu.plaintext_password(),
            username: pdu.username,
            domain: pdu.domain,
            rdstls,
        }
    }
}

/// [`Redirect`] resolved against the builder's own credentials, for the connector.
struct RedirectConnector {
    routing_token: Option<String>,
    rdstls: Option<RdstlsAuthenticationRequest>,
}

/// State a redirected connection inherits from the session it replaces.
struct Reuse {
    input_events_tx: mpsc::UnboundedSender<RdpInputEvent>,
    input_events_rx: mpsc::UnboundedReceiver<RdpInputEvent>,
    drive_slot: DriveSlot,
    /// The desktop size the replaced connection last applied.
    desktop_size: DesktopSize,
}

/// How one connection of a session ended.
enum ConnectionEnd {
    Terminated(GracefulDisconnectReason),
    Redirected(Box<ServerRedirectionPdu>),
}

pub(crate) struct Session {
    /// Kept to reconnect when the server redirects the session.
    builder: SessionBuilder,
    desktop_size: Cell<connector::DesktopSize>,
    input_database: RefCell<ironrdp::input::Database>,
    /// Replaced when a Server Redirection moves the session to a new connection.
    writer_tx: RefCell<mpsc::UnboundedSender<Vec<u8>>>,
    input_events_tx: mpsc::UnboundedSender<RdpInputEvent>,

    render_canvas: HtmlCanvasElement,
    /// The GPU backend EGFX draws with, shared with the presenter; `None` draws on the CPU.
    /// Replaced, with the EGFX client using it, when a Server Redirection reconnects.
    gpu: RefCell<Option<crate::gpu::GlBackend>>,
    set_cursor_style_callback: js_sys::Function,
    set_cursor_style_callback_context: JsValue,

    // Consumed when `run` is called
    input_events_rx: RefCell<Option<mpsc::UnboundedReceiver<RdpInputEvent>>>,
    connection_result: RefCell<Option<connector::ConnectionResult>>,
    rdp_reader: RefCell<Option<(ReadHalf<RdpStream>, bytes::BytesMut)>>,
    clipboard: RefCell<Option<Option<WasmClipboard>>>,
    printer: RefCell<Option<Option<WasmPrinter>>>,

    /// Live drive member of the composite RDPDR backend, shared with it so a folder picked
    /// mid-session can be installed without reaching into `Rdpdr`'s private backend slot.
    drive_slot: DriveSlot,
    /// Device id a mid-session drive is announced under — the same value `connect` resolves,
    /// recomputed here from the same pure function of the printer id.
    drive_device_id: u32,
}

impl Session {
    /// Drives one connection until it ends, returning why. The connection's
    /// reader and per-connection state are taken from `self`, and the input
    /// channel is put back on the way out so a redirected session can resume.
    async fn run_connection(&self) -> Result<ConnectionEnd, IronError> {
        let (rdp_reader, rdp_leftover) = self
            .rdp_reader
            .borrow_mut()
            .take()
            .context("RDP session can be started only once")?;

        let mut input_events = self
            .input_events_rx
            .borrow_mut()
            .take()
            .context("RDP session can be started only once")?;

        let connection_result = self
            .connection_result
            .borrow_mut()
            .take()
            .expect("run called only once");

        let mut clipboard = self.clipboard.borrow_mut().take().expect("run called only once");
        let mut wasm_printer = self.printer.borrow_mut().take().expect("run called only once");

        // Any post-finalization PDUs that coalesced into the connector's last read
        // (typically the server's DVC capabilities request) are in `rdp_leftover`
        // and must be replayed ahead of fresh socket reads — see `connect()`.
        let mut framed = ironrdp_futures::LocalFuturesFramed::new_with_leftover(rdp_reader, rdp_leftover);

        debug!("Initialize canvas");

        let desktop_width =
            NonZeroU32::new(u32::from(connection_result.desktop_size.width)).context("desktop width is zero")?;
        let desktop_height =
            NonZeroU32::new(u32::from(connection_result.desktop_size.height)).context("desktop height is zero")?;

        let mut gui = match self.gpu.borrow().as_ref() {
            Some(gl) => {
                gl.resize_output(desktop_width.get(), desktop_height.get());
                Presenter::Gl(gl.clone())
            }
            None => Presenter::Canvas2d(
                Canvas::new(self.render_canvas.clone(), desktop_width, desktop_height)
                    .context("canvas initialization")?,
            ),
        };

        debug!("Canvas initialized");

        info!("Start RDP session");

        let mut image = DecodedImage::new(
            PixelFormat::RgbA32,
            connection_result.desktop_size.width,
            connection_result.desktop_size.height,
        );

        let mut requested_resize = None;

        // Reused across frames so per-region extraction doesn't allocate on every draw.
        let mut draw_buffer = WriteBuf::new();

        // We retain the factory to drive the Deactivation-Reactivation Sequence locally.
        let activation_factory = connection_result.activation_factory;

        let mut active_stage = ActiveStageBuilder {
            static_channels: connection_result.static_channels,
            user_channel_id: connection_result.user_channel_id,
            io_channel_id: connection_result.io_channel_id,
            message_channel_id: connection_result.message_channel_id,
            share_id: connection_result.share_id,
            compression_type: connection_result.compression_type,
            enable_server_pointer: connection_result.enable_server_pointer,
            pointer_software_rendering: connection_result.pointer_software_rendering,
        }
        .build();

        // Timer interval for driving clipboard lock timeouts (5 second interval)
        let mut cleanup_interval = IntervalStream::new(5_000).fuse();

        let connection_end = 'outer: loop {
            let outputs = select! {
                frame = framed.read_pdu().fuse() => {
                    let (action, payload) = frame.context("read frame")?;
                    trace!(?action, frame_length = payload.len(), "Frame received");

                    active_stage.process_received(&mut image, action, &payload, framed.last_read_at())?
                }
                input_events = input_events.next() => {
                    let event = input_events.context("read next input events")?;

                    match event {
                        RdpInputEvent::Cliprdr(message) => {
                            if let Some(cliprdr) = active_stage.get_svc_processor_mut::<CliprdrClient>() {
                                if let Some(svc_messages) = match message {
                                    ClipboardMessage::SendInitiateCopy(formats) => Some(
                                        cliprdr.initiate_copy(&formats)
                                            .context("cliprdr initiate copy")?
                                    ),
                                    ClipboardMessage::SendInitiateFileCopy(files) => Some(
                                        cliprdr.initiate_file_copy(files)
                                            .context("cliprdr initiate file copy")?
                                    ),
                                    ClipboardMessage::SendFormatData(response) => Some(
                                        cliprdr.submit_format_data(response)
                                            .context("cliprdr submit format data")?
                                    ),
                                    ClipboardMessage::SendInitiatePaste(format) => Some(
                                        cliprdr.initiate_paste(format)
                                            .context("cliprdr initiate paste")?
                                    ),
                                    ClipboardMessage::SendFileContentsRequest(request) => Some(
                                        cliprdr.request_file_contents(request)
                                            .context("cliprdr request file contents")?
                                    ),
                                    ClipboardMessage::SendFileContentsResponse(response) => Some(
                                        cliprdr.submit_file_contents(response)
                                            .context("cliprdr submit file contents")?
                                    ),
                                    ClipboardMessage::Error(e) => {
                                        error!(error = %e, "Clipboard backend error");
                                        None
                                    }
                                } {
                                    let frame = active_stage.process_svc_processor_messages(svc_messages)?;
                                    // Send the messages to the server
                                    vec![ActiveStageOutput::ResponseFrame(frame)]
                                } else {
                                    // No messages to send to the server
                                    Vec::new()
                                }
                            } else  {
                                warn!("Clipboard event received, but Cliprdr is not available");
                                Vec::new()
                            }
                        }
                        RdpInputEvent::ClipboardBackend(event) => {
                            use crate::clipboard::WasmClipboardBackendMessage;

                            // Handle messages that need direct cliprdr access
                            match event {
                                WasmClipboardBackendMessage::FileContentsRequestSend { stream_id, index, flags, position, size, clip_data_id } => {
                                    if let Some(cliprdr) = active_stage.get_svc_processor_mut::<CliprdrClient>() {
                                        let request = FileContentsRequest {
                                            stream_id,
                                            index,
                                            flags,
                                            position,
                                            requested_size: size,
                                            data_id: clip_data_id,
                                        };
                                        match cliprdr.request_file_contents(request) {
                                            Ok(svc_messages) => {
                                                let frame = active_stage.process_svc_processor_messages(svc_messages)?;
                                                vec![ActiveStageOutput::ResponseFrame(frame)]
                                            }
                                            Err(e) => {
                                                error!(error = %e, "File contents request failed");
                                                Vec::new()
                                            }
                                        }
                                    } else {
                                        warn!("Request file contents received, but Cliprdr is not available");
                                        Vec::new()
                                    }
                                }
                                WasmClipboardBackendMessage::FileContentsResponseSend { stream_id, is_error, data } => {
                                    if let Some(cliprdr) = active_stage.get_svc_processor_mut::<CliprdrClient>() {
                                        let response = if is_error {
                                            FileContentsResponse::new_error(stream_id)
                                        } else {
                                            FileContentsResponse::new_data_response(stream_id, data)
                                        };
                                        match cliprdr.submit_file_contents(response) {
                                            Ok(svc_messages) => {
                                                let frame = active_stage.process_svc_processor_messages(svc_messages)?;
                                                vec![ActiveStageOutput::ResponseFrame(frame)]
                                            }
                                            Err(e) => {
                                                error!(error = %e, "File contents submit failed");
                                                Vec::new()
                                            }
                                        }
                                    } else {
                                        warn!("Submit file contents received, but Cliprdr is not available");
                                        Vec::new()
                                    }
                                }
                                WasmClipboardBackendMessage::InitiateFileCopy { files } => {
                                    if let Some(cliprdr) = active_stage.get_svc_processor_mut::<CliprdrClient>() {
                                        // Convert FileMetadata to FileDescriptor using the
                                        // validated conversion that checks name length/emptiness
                                        // and sets proper file attributes.
                                        let file_descriptors: Vec<FileDescriptor> = files
                                            .into_iter()
                                            .filter_map(|f| match f.to_file_descriptor() {
                                                Ok(desc) => Some(desc),
                                                Err(e) => {
                                                    warn!(error = format!("{e:#}"), "Skipping file with invalid metadata");
                                                    None
                                                }
                                            })
                                            .collect();

                                        match cliprdr.initiate_file_copy(file_descriptors) {
                                            Ok(svc_messages) => {
                                                let frame = active_stage.process_svc_processor_messages(svc_messages)?;
                                                vec![ActiveStageOutput::ResponseFrame(frame)]
                                            }
                                            Err(e) => {
                                                error!(error = %e, "Initiate file copy failed");
                                                Vec::new()
                                            }
                                        }
                                    } else {
                                        warn!("Initiate file copy received, but Cliprdr is not available");
                                        Vec::new()
                                    }
                                }
                                // All other messages are forwarded to clipboard backend
                                other => {
                                    if let Some(clipboard) = &mut clipboard {
                                        clipboard.process_event(other)?;
                                    }
                                    Vec::new()
                                }
                            }
                        }
                        RdpInputEvent::FastPath(events) => {
                            active_stage.process_fastpath_input(&mut image, &events)
                                .context("fast path input events processing")?
                        }
                        RdpInputEvent::GraphicsDecoded => {
                            active_stage.resume_graphics_pipeline(&mut image)
                                .context("resume graphics pipeline")?
                        }
                        RdpInputEvent::Resize { width, height, scale_factor, physical_size } => {
                            debug!(width, height, scale_factor, "Resize event received");
                            if width == 0 || height == 0 {
                                warn!("Resize event ignored: width or height is zero");
                                Vec::new()
                            } else {
                                // RDPEDISP 2.2.2.2.1: even width, both sides in 200..=8192.
                                // Adjusting here keeps the request and the remembered size in
                                // agreement, and a size outside the range (a pane under 200px)
                                // no longer fails the encode, which used to end the session.
                                let (width, height) = MonitorLayoutEntry::adjust_display_size(width, height);
                                match active_stage.encode_resize(width, height, scale_factor, physical_size) {
                                    Some(Ok(response_frame)) => {
                                        let width = NonZeroU32::new(width).expect("adjusted width is at least 200");
                                        let height = NonZeroU32::new(height).expect("adjusted height is at least 200");

                                        requested_resize = Some((width, height));
                                        vec![ActiveStageOutput::ResponseFrame(response_frame)]
                                    }
                                    Some(Err(error)) => {
                                        warn!(%error, width, height, "Resize request could not be encoded, ignoring");
                                        Vec::new()
                                    }
                                    None => {
                                        debug!("Resize event ignored");
                                        Vec::new()
                                    }
                                }
                            }
                        },
                        RdpInputEvent::Printer(message) => {
                            // The printer backend lives inside the Rdpdr SVC
                            // processor (Send-only); the front-end
                            // `WasmPrinter` owns the JS callback (!Send) and
                            // lives here. Just forward the message.
                            if let Some(ref mut wasm_printer) = wasm_printer {
                                wasm_printer.process_message(message);
                            } else {
                                warn!("Printer event received, but no printer is configured");
                            }
                            Vec::new()
                        }
                        RdpInputEvent::DriveBackend(SyncDriveBackendMessage(DriveBackendMessage::IoCompleted(messages))) => {
                            // Mirrors the Cliprdr arms above: encode the already-built
                            // completion PDU(s) onto the Rdpdr static channel and send the
                            // resulting frame. Unlike Cliprdr's arms, `messages` is not
                            // produced by calling a method on the processor here — it was
                            // already built by `WasmDriveBackend` — so there is no
                            // `get_svc_processor_mut` call needed to obtain it, only to
                            // confirm the channel is actually attached.
                            if active_stage.get_svc_processor_mut::<Rdpdr>().is_some() {
                                match active_stage.process_svc_processor_messages::<Rdpdr>(messages.into()) {
                                    Ok(frame) if !frame.is_empty() => vec![ActiveStageOutput::ResponseFrame(frame)],
                                    Ok(_) => Vec::new(),
                                    Err(e) => {
                                        error!(error = %e, "Drive IRP completion failed to encode onto the Rdpdr channel");
                                        Vec::new()
                                    }
                                }
                            } else {
                                warn!("Drive IRP completion received, but Rdpdr is not available");
                                Vec::new()
                            }
                        }
                        RdpInputEvent::DriveHotplug { device_id, share_name } => {
                            // The backend is already staged in the shared slot (see the
                            // `drive_share` arm of `invoke_extension`); `add_dynamic_drive`
                            // activates it through the composite's `add_drive` and returns the
                            // device announcement to put on the wire.
                            match active_stage.get_svc_processor_mut::<Rdpdr>() {
                                Some(rdpdr) => match rdpdr.add_dynamic_drive(device_id, share_name) {
                                    Ok(messages) if messages.is_empty() => Vec::new(),
                                    Ok(messages) => {
                                        match active_stage.process_svc_processor_messages::<Rdpdr>(messages.into()) {
                                            Ok(frame) if !frame.is_empty() => {
                                                info!(device_id, "drive shared mid-session");
                                                vec![ActiveStageOutput::ResponseFrame(frame)]
                                            }
                                            Ok(_) => Vec::new(),
                                            Err(e) => {
                                                error!(error = %e, "Failed to encode dynamic drive announcement");
                                                Vec::new()
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        error!(error = %e, "Dynamic drive announcement rejected");
                                        Vec::new()
                                    }
                                },
                                None => {
                                    warn!("Drive share requested, but Rdpdr is not available");
                                    Vec::new()
                                }
                            }
                        }
                        RdpInputEvent::TerminateSession => {
                            active_stage.graceful_shutdown()
                                .context("graceful shutdown")?
                        }
                    }
                }
                _ = cleanup_interval.next() => {
                    // Drive clipboard lock timeout cleanup
                    if let Some(cliprdr) = active_stage.get_svc_processor_mut::<CliprdrClient>() {
                        match cliprdr.drive_timeouts() {
                            Ok(svc_messages) => {
                                let frame = active_stage.process_svc_processor_messages(svc_messages)?;
                                if !frame.is_empty() {
                                    vec![ActiveStageOutput::ResponseFrame(frame)]
                                } else {
                                    Vec::new()
                                }
                            }
                            Err(e) => {
                                warn!(error = %e, "Clipboard timeout cleanup failed");
                                Vec::new()
                            }
                        }
                    } else {
                        Vec::new()
                    }
                }
            };

            // A lost GPU context takes every surface and cache texture with it, while the
            // server still trusts its cache: the session cannot continue, and reconnects.
            if let Some(reason) = gui.failure() {
                return Err(anyhow::anyhow!(reason).into());
            }

            // On the GPU, ResetGraphics resizes the canvas inside the EGFX backend and no
            // GraphicsUpdate follows, so the resize bookkeeping of that arm happens here.
            if let Presenter::Gl(_) = &gui {
                let (width, height) = gui.size();
                let output = connector::DesktopSize {
                    width: u16::try_from(width).unwrap_or(u16::MAX),
                    height: u16::try_from(height).unwrap_or(u16::MAX),
                };
                if self.desktop_size.get() != output {
                    debug!(width, height, "GPU output size changed");
                    requested_resize = None;
                    // Remembered so a Server Redirection reconnects at this size.
                    self.desktop_size.set(output);
                }
            }

            for out in outputs {
                match out {
                    ActiveStageOutput::ResponseFrame(frame) => {
                        self.writer_tx
                            .borrow()
                            .unbounded_send(frame)
                            .context("Send frame to writer task")?;
                    }
                    ActiveStageOutput::GraphicsUpdate(region) => {
                        // With EGFX active the server resizes through ResetGraphics, not the
                        // Deactivation-Reactivation Sequence. The active stage has already reset
                        // `image` to the new extent and widened this update to the whole frame,
                        // so the canvas only has to follow. This also completes any resize
                        // requested via Display Control.
                        let image_size = (u32::from(image.width()), u32::from(image.height()));
                        if gui.size() != image_size {
                            debug!(
                                width = image_size.0,
                                height = image_size.1,
                                "Framebuffer size changed, resizing canvas"
                            );
                            gui.resize(
                                NonZeroU32::new(image_size.0).context("desktop width is zero")?,
                                NonZeroU32::new(image_size.1).context("desktop height is zero")?,
                            );
                            requested_resize = None;
                            // Remembered so a Server Redirection reconnects at this size.
                            self.desktop_size.set(connector::DesktopSize {
                                width: image.width(),
                                height: image.height(),
                            });
                        }

                        let region = extract_partial_image(&image, region, &mut draw_buffer);
                        gui.draw(draw_buffer.filled_mut(), region)
                            .context("draw updated region")?;
                        draw_buffer.clear();
                    }
                    ActiveStageOutput::PointerDefault => {
                        self.set_cursor_style(CursorStyle::Default)?;
                    }
                    ActiveStageOutput::PointerHidden => {
                        self.set_cursor_style(CursorStyle::Hidden)?;
                    }
                    ActiveStageOutput::PointerPosition { .. } => {
                        // Not applicable for web.
                    }
                    ActiveStageOutput::PointerBitmap(pointer) => {
                        // Maximum allowed cursor size for browsers is 32x32, because bigger sizes
                        // will cause the following issues:
                        // - cursors bigger than 128x128 are not supported in browsers.
                        // - cursors bigger than 32x32 will default to the system cursor if their
                        //   sprite does not fit in the browser's viewport, introducing an abrupt
                        //   cursor style change when the cursor is moved to the edge of the
                        //   browser window.
                        //
                        // Therefore, we need to scale the cursor sprite down to 32x32 if it is
                        // bigger than that.
                        const MAX_CURSOR_SIZE: u16 = 32;
                        // INVARIANT: 0 < scale <= 1.0
                        // INVARIANT: pointer.width * scale <= MAX_CURSOR_SIZE
                        // INVARIANT: pointer.height * scale <= MAX_CURSOR_SIZE
                        let scale = if pointer.width >= pointer.height && pointer.width > MAX_CURSOR_SIZE {
                            Some(f64::from(MAX_CURSOR_SIZE) / f64::from(pointer.width))
                        } else if pointer.height > MAX_CURSOR_SIZE {
                            Some(f64::from(MAX_CURSOR_SIZE) / f64::from(pointer.height))
                        } else {
                            None
                        };

                        let (png_width, png_height, hotspot_x, hotspot_y, rgba_buffer) = if let Some(scale) = scale {
                            // Per invariants: Following conversions will never saturate.
                            let scaled_width = f64_to_u16_saturating_cast(f64::from(pointer.width) * scale);
                            let scaled_height = f64_to_u16_saturating_cast(f64::from(pointer.height) * scale);
                            let hotspot_x = f64_to_u16_saturating_cast(f64::from(pointer.hotspot_x) * scale);
                            let hotspot_y = f64_to_u16_saturating_cast(f64::from(pointer.hotspot_y) * scale);

                            // Per invariants: scaled_width * scaled_height * 4 <= 32 * 32 * 4 < usize::MAX
                            #[expect(clippy::arithmetic_side_effects)]
                            let resized_rgba_buffer_size = usize::from(scaled_width * scaled_height * 4);

                            let mut rgba_resized = vec![0u8; resized_rgba_buffer_size];
                            let mut resizer = resize::new(
                                usize::from(pointer.width),
                                usize::from(pointer.height),
                                usize::from(scaled_width),
                                usize::from(scaled_height),
                                resize::Pixel::RGBA8P,
                                resize::Type::Lanczos3,
                            )
                            .context("failed to initialize cursor resizer")?;

                            resizer
                                .resize(pointer.bitmap_data.as_pixels(), rgba_resized.as_pixels_mut())
                                .context("failed to resize cursor")?;

                            (
                                scaled_width,
                                scaled_height,
                                hotspot_x,
                                hotspot_y,
                                Cow::Owned(rgba_resized),
                            )
                        } else {
                            (
                                pointer.width,
                                pointer.height,
                                pointer.hotspot_x,
                                pointer.hotspot_y,
                                Cow::Borrowed(pointer.bitmap_data.as_slice()),
                            )
                        };

                        // Encode PNG.
                        let mut png_buffer = Vec::new();
                        {
                            let mut encoder =
                                png::Encoder::new(&mut png_buffer, u32::from(png_width), u32::from(png_height));

                            encoder.set_color(png::ColorType::Rgba);
                            encoder.set_depth(png::BitDepth::Eight);
                            encoder.set_compression(png::Compression::Fast);
                            let mut writer = encoder.write_header().context("PNG encoder header write failed")?;
                            writer
                                .write_image_data(&rgba_buffer)
                                .context("failed to encode pointer PNG")?;
                        }

                        // Encode PNG into Base64 data URL.
                        let mut style = "data:image/png;base64,".to_owned();
                        base64::engine::general_purpose::STANDARD.encode_string(png_buffer, &mut style);

                        self.set_cursor_style(CursorStyle::Url {
                            data: style,
                            hotspot_x,
                            hotspot_y,
                        })?;
                    }
                    ActiveStageOutput::MonitorLayout(monitors) => {
                        debug!(
                            monitor_count = monitors.len(),
                            "Received remote monitor layout without multi-monitor support"
                        );
                    }
                    ActiveStageOutput::WindowingOrders(_) => {
                        // Windowing orders are not meaningful to the protocol-agnostic web component.
                    }
                    ActiveStageOutput::DeactivateAll => {
                        // Execute the Deactivation-Reactivation Sequence:
                        // https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/dfc234ce-481a-4674-9a5d-2a7bafb14432
                        debug!("Received Server Deactivate All PDU, executing Deactivation-Reactivation Sequence");

                        // We need to perform resize after receiving the Deactivate All PDU, because there may be frames
                        // with the previous dimensions arriving between the resize request and this message.
                        if let Some((width, height)) = requested_resize {
                            gui.resize(width, height);
                            requested_resize = None;
                        }

                        let mut connection_activation = activation_factory.create();
                        let mut buf = WriteBuf::new();
                        'activation_seq: loop {
                            let written =
                                single_sequence_step_read(&mut framed, &mut connection_activation, &mut buf).await?;

                            if written.size().is_some() {
                                self.writer_tx
                                    .borrow()
                                    .unbounded_send(buf.filled().to_vec())
                                    .context("Send frame to writer task")?;
                            }

                            if let ConnectionActivationState::Finalized {
                                desktop_size,
                                share_id,
                                input_flags: _,
                                enable_server_pointer,
                                pointer_software_rendering,
                                static_channel_chunk_size,
                                window_support_level,
                                ..
                            } = connection_activation.connection_activation_state()
                            {
                                debug!("Deactivation-Reactivation Sequence completed");
                                image = DecodedImage::new(PixelFormat::RgbA32, desktop_size.width, desktop_size.height);
                                if !active_stage.reactivate(
                                    connection_activation.io_channel_id(),
                                    connection_activation.user_channel_id(),
                                    share_id,
                                    enable_server_pointer,
                                    pointer_software_rendering,
                                    static_channel_chunk_size,
                                ) {
                                    return Err(IronError::from(anyhow::anyhow!("invalid static channel chunk size")));
                                }
                                active_stage.set_window_support_level(window_support_level);
                                break 'activation_seq;
                            }
                        }
                    }
                    ActiveStageOutput::MultitransportRequest(pdu) => {
                        debug!(
                            request_id = pdu.request_id,
                            requested_protocol = ?pdu.requested_protocol,
                            "Multitransport request received (UDP transport not implemented)"
                        );
                    }
                    ActiveStageOutput::AutoDetect(request) => {
                        debug!(?request, "Auto-detect");
                    }
                    ActiveStageOutput::AutoReconnectCookie(_) => {
                        debug!("Server Auto-Reconnect Cookie received (automatic reconnection not implemented)");
                    }
                    ActiveStageOutput::AutoReconnectFailed => {
                        return Err(anyhow::Error::msg("automatic reconnect rejected by server").into());
                    }
                    ActiveStageOutput::SaveSessionInfo { logon_complete: true } => {
                        debug!("RDP login complete");
                    }
                    ActiveStageOutput::SaveSessionInfo { logon_complete: false } => {
                        debug!("RDP session info notification");
                    }
                    ActiveStageOutput::ServerRedirection(pdu) => {
                        if pdu.flags.contains(RedirectionFlags::NO_REDIRECT) {
                            debug!("Server Redirection PDU flagged NO_REDIRECT, ignoring");
                        } else {
                            break 'outer ConnectionEnd::Redirected(pdu);
                        }
                    }
                    ActiveStageOutput::Terminate(reason) => break 'outer ConnectionEnd::Terminated(reason),
                }
            }
        };

        // A redirected session keeps going on a new connection, fed by the same
        // input channel.
        *self.input_events_rx.borrow_mut() = Some(input_events);

        Ok(connection_end)
    }

    /// Replaces this session's connection with the one a Server Redirection PDU
    /// points at, keeping the input channel so JS-held handles stay valid.
    async fn follow_redirection(&self, pdu: ServerRedirectionPdu) -> Result<(), IronError> {
        let redirect = Redirect::from_pdu(pdu);

        // The username is a one-time handover credential on g-r-d; keep it
        // out of logs alongside the password.
        info!(
            routing_token = ?redirect.routing_token,
            has_username = redirect.username.is_some(),
            rdstls = redirect.rdstls.is_some(),
            "Following Server Redirection"
        );

        let input_events_rx = self
            .input_events_rx
            .borrow_mut()
            .take()
            .context("input channel missing while following a redirection")?;

        let Session {
            desktop_size,
            writer_tx,
            input_events_rx,
            connection_result,
            rdp_reader,
            clipboard,
            printer,
            gpu,
            ..
        } = self
            .builder
            .connect_with(
                Some(redirect),
                Some(Reuse {
                    input_events_tx: self.input_events_tx.clone(),
                    input_events_rx,
                    drive_slot: Rc::clone(&self.drive_slot),
                    desktop_size: DesktopSize {
                        width: self.desktop_size.get().width,
                        height: self.desktop_size.get().height,
                    },
                }),
            )
            .await?;

        self.desktop_size.set(desktop_size.get());
        // Dropping the previous sender ends the old writer task, which closes the
        // old connection's write half.
        *self.writer_tx.borrow_mut() = writer_tx.into_inner();
        *self.input_events_rx.borrow_mut() = input_events_rx.into_inner();
        *self.connection_result.borrow_mut() = connection_result.into_inner();
        *self.rdp_reader.borrow_mut() = rdp_reader.into_inner();
        *self.clipboard.borrow_mut() = clipboard.into_inner();
        *self.printer.borrow_mut() = printer.into_inner();
        // The new connection's EGFX client draws with the new backend; dropping the old one
        // frees its textures.
        *self.gpu.borrow_mut() = gpu.into_inner();

        Ok(())
    }

    fn h_send_inputs(&self, inputs: smallvec::SmallVec<[FastPathInputEvent; 2]>) -> Result<(), IronError> {
        if !inputs.is_empty() {
            trace!("Inputs: {inputs:?}");

            self.input_events_tx
                .unbounded_send(RdpInputEvent::FastPath(inputs))
                .context("Send input events to writer task")?;
        }

        Ok(())
    }

    fn set_cursor_style(&self, style: CursorStyle) -> Result<(), IronError> {
        let (kind, data, hotspot_x, hotspot_y) = match style {
            CursorStyle::Default => ("default", None, None, None),
            CursorStyle::Hidden => ("hidden", None, None, None),
            CursorStyle::Url {
                data,
                hotspot_x,
                hotspot_y,
            } => ("url", Some(data), Some(hotspot_x), Some(hotspot_y)),
        };

        let args = js_sys::Array::from_iter([
            JsValue::from_str(kind),
            JsValue::from(data),
            JsValue::from_f64(hotspot_x.unwrap_or_default().into()),
            JsValue::from_f64(hotspot_y.unwrap_or_default().into()),
        ]);

        let _ret = self
            .set_cursor_style_callback
            .apply(&self.set_cursor_style_callback_context, &args)
            .map_err(|e| anyhow::Error::msg(format!("set cursor style callback failed: {e:?}")))?;

        Ok(())
    }
}

impl iron_remote_desktop::Session for Session {
    type SessionTerminationInfo = SessionTerminationInfo;
    type InputTransaction = InputTransaction;
    type ClipboardData = ClipboardData;
    type Error = IronError;

    async fn run(&self) -> Result<Self::SessionTerminationInfo, Self::Error> {
        loop {
            match self.run_connection().await? {
                ConnectionEnd::Terminated(reason) => {
                    info!(%reason, "RDP session terminated");
                    return Ok(SessionTerminationInfo { reason });
                }
                ConnectionEnd::Redirected(pdu) => self.follow_redirection(*pdu).await?,
            }
        }
    }

    fn desktop_size(&self) -> DesktopSize {
        DesktopSize {
            width: self.desktop_size.get().width,
            height: self.desktop_size.get().height,
        }
    }

    fn apply_inputs(&self, transaction: Self::InputTransaction) -> Result<(), Self::Error> {
        let inputs = self.input_database.borrow_mut().apply(transaction);
        self.h_send_inputs(inputs)
    }

    fn release_all_inputs(&self) -> Result<(), Self::Error> {
        let inputs = self.input_database.borrow_mut().release_all();
        self.h_send_inputs(inputs)
    }

    fn synchronize_lock_keys(
        &self,
        scroll_lock: bool,
        num_lock: bool,
        caps_lock: bool,
        kana_lock: bool,
    ) -> Result<(), Self::Error> {
        use ironrdp::pdu::input::fast_path::FastPathInput;

        let event = ironrdp::input::synchronize_event(scroll_lock, num_lock, caps_lock, kana_lock);
        let fastpath_input = FastPathInput::single(event);

        let frame = ironrdp::core::encode_vec(&fastpath_input).context("FastPathInput encoding")?;

        self.writer_tx
            .borrow()
            .unbounded_send(frame)
            .context("Send frame to writer task")?;

        Ok(())
    }

    fn shutdown(&self) -> Result<(), Self::Error> {
        self.input_events_tx
            .unbounded_send(RdpInputEvent::TerminateSession)
            .context("failed to send terminate session event to writer task")?;

        Ok(())
    }

    async fn on_clipboard_paste(&self, content: &Self::ClipboardData) -> Result<(), Self::Error> {
        self.input_events_tx
            .unbounded_send(RdpInputEvent::ClipboardBackend(
                WasmClipboardBackendMessage::LocalClipboardChanged(content.clone()),
            ))
            .context("send clipboard backend event")?;

        Ok(())
    }

    fn resize(
        &self,
        width: u32,
        height: u32,
        scale_factor: Option<u32>,
        physical_width: Option<u32>,
        physical_height: Option<u32>,
    ) {
        if self
            .input_events_tx
            .unbounded_send(RdpInputEvent::Resize {
                width,
                height,
                scale_factor,
                physical_size: physical_width.and_then(|width| physical_height.map(|height| (width, height))),
            })
            .is_err()
        {
            warn!("Failed to send resize event, receiver is closed");
        }
    }

    fn supports_unicode_keyboard_shortcuts(&self) -> bool {
        // RDP does not support Unicode keyboard shortcuts.
        // When key combinations are executed, only plain scancode events are allowed to function correctly.
        false
    }

    fn invoke_extension(&self, ext: Extension) -> Result<JsValue, Self::Error> {
        // File transfer operations are protocol-specific (RDPECLIP) and routed
        // through invoke_extension rather than dedicated Session trait methods
        // to keep the iron-remote-desktop trait surface protocol-agnostic.
        iron_remote_desktop::extension_match! {
            match ext;
            |request_file_contents: JsValue| {
                let obj = into_object(request_file_contents)?;
                let stream_id = get_u32(&obj, "stream_id")?;
                let file_index = get_i32(&obj, "file_index")?;
                let flags = get_u32(&obj, "flags")?;
                let position = get_u64(&obj, "position")?;
                let size = get_u32(&obj, "size")?;
                let clip_data_id = get_u32_opt(&obj, "clip_data_id")?;

                self.input_events_tx
                    .unbounded_send(RdpInputEvent::ClipboardBackend(
                        WasmClipboardBackendMessage::FileContentsRequestSend {
                            stream_id,
                            index: file_index,
                            flags: FileContentsFlags::from_bits_truncate(flags),
                            position,
                            size,
                            clip_data_id,
                        },
                    ))
                    .context("send file contents request")
                    .map_err(IronError::from)?;

                return Ok(JsValue::NULL);
            };
            |submit_file_contents: JsValue| {
                let obj = into_object(submit_file_contents)?;
                let stream_id = get_u32(&obj, "stream_id")?;
                let is_error = get_bool(&obj, "is_error")?;
                let data_val = js_sys::Reflect::get(&obj, &JsValue::from_str("data"))
                    .map_err(|e| IronError::from(anyhow::anyhow!("get property `data`: {e:?}")))?;
                let data = js_sys::Uint8Array::new(&data_val).to_vec();

                self.input_events_tx
                    .unbounded_send(RdpInputEvent::ClipboardBackend(
                        WasmClipboardBackendMessage::FileContentsResponseSend {
                            stream_id,
                            is_error,
                            data,
                        },
                    ))
                    .context("send file contents response")
                    .map_err(IronError::from)?;

                return Ok(JsValue::NULL);
            };
            |drive_share: JsValue| {
                // Mid-session folder share. The same `driveShare` extension the config builder
                // accepts, invoked on a live session: MS-RDPEFS allows announcing a device after
                // connect, and `Rdpdr::add_dynamic_drive` implements that sequencing — but it is
                // handed only a device id, so the filesystem has to be staged in the shared
                // `DriveSlot` here, before the event loop announces it.
                if self.drive_slot.borrow().is_some() {
                    return Err(IronError::from(anyhow::anyhow!(
                        "a folder is already shared with this session; reconnect to change it"
                    ))
                    .with_kind(IronErrorKind::General));
                }

                let config = parse_drive_share(drive_share).map_err(IronError::from)?;
                let share_name = config.share_name.clone();
                let fs: Rc<dyn DriveFs> = Rc::new(WebFsDrive::new(config.handle));
                let (drive_tx, mut drive_rx) = mpsc::unbounded();
                let backend = wasm_drive_pair(drive_tx, fs, config.read_only);

                // Same completion-forwarding adapter the connect-time path installs; it ends on
                // its own when the backend is dropped.
                let forward_tx = self.input_events_tx.clone();
                spawn_local(async move {
                    while let Some(message) = drive_rx.next().await {
                        if forward_tx
                            .unbounded_send(RdpInputEvent::DriveBackend(SyncDriveBackendMessage(message)))
                            .is_err()
                        {
                            break;
                        }
                    }
                });

                *self.drive_slot.borrow_mut() = Some((backend, self.drive_device_id));

                self.input_events_tx
                    .unbounded_send(RdpInputEvent::DriveHotplug {
                        device_id: self.drive_device_id,
                        share_name,
                    })
                    .context("send drive hotplug")
                    .map_err(IronError::from)?;

                return Ok(JsValue::NULL);
            };
            |initiate_file_copy: JsValue| {
                let file_list = parse_file_metadata_array(initiate_file_copy)?;

                self.input_events_tx
                    .unbounded_send(RdpInputEvent::ClipboardBackend(
                        WasmClipboardBackendMessage::InitiateFileCopy { files: file_list },
                    ))
                    .context("send initiate file copy")
                    .map_err(IronError::from)?;

                return Ok(JsValue::NULL);
            };
        }

        Err(
            IronError::from(anyhow::Error::msg(format!("unknown extension: {}", ext.ident())))
                .with_kind(IronErrorKind::General),
        )
    }
}

fn into_object(val: JsValue) -> Result<js_sys::Object, IronError> {
    val.dyn_into::<js_sys::Object>()
        .map_err(|_| anyhow::anyhow!("expected object").into())
}

fn get_u32(obj: &js_sys::Object, key: &str) -> Result<u32, IronError> {
    let val = js_sys::Reflect::get(obj, &JsValue::from_str(key))
        .map_err(|e| anyhow::anyhow!("get property `{key}`: {e:?}"))?;
    let f = val
        .as_f64()
        .with_context(|| format!("invalid type for property `{key}`"))?;
    Ok(f64_to_u32_saturating_cast(f))
}

fn get_i32(obj: &js_sys::Object, key: &str) -> Result<i32, IronError> {
    let val = js_sys::Reflect::get(obj, &JsValue::from_str(key))
        .map_err(|e| anyhow::anyhow!("get property `{key}`: {e:?}"))?;
    let f = val
        .as_f64()
        .with_context(|| format!("invalid type for property `{key}`"))?;
    Ok(f64_to_i32_saturating_cast(f))
}

fn get_u64(obj: &js_sys::Object, key: &str) -> Result<u64, IronError> {
    let val = js_sys::Reflect::get(obj, &JsValue::from_str(key))
        .map_err(|e| anyhow::anyhow!("get property `{key}`: {e:?}"))?;
    let f = val
        .as_f64()
        .with_context(|| format!("invalid type for property `{key}`"))?;
    // Validate integer precision before casting
    const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
    if !f.is_finite() || f < 0.0 || f.fract() != 0.0 || f > MAX_SAFE_INTEGER {
        return Err(anyhow::anyhow!(
            "property `{key}` must be a finite non-negative integer <= Number.MAX_SAFE_INTEGER (got: {f})"
        )
        .into());
    }
    #[expect(clippy::as_conversions, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(f as u64)
}

fn get_bool(obj: &js_sys::Object, key: &str) -> Result<bool, IronError> {
    let val = js_sys::Reflect::get(obj, &JsValue::from_str(key))
        .map_err(|e| anyhow::anyhow!("get property `{key}`: {e:?}"))?;
    val.as_bool()
        .with_context(|| format!("invalid type for property `{key}`"))
        .map_err(Into::into)
}

fn get_u32_opt(obj: &js_sys::Object, key: &str) -> Result<Option<u32>, IronError> {
    let val = js_sys::Reflect::get(obj, &JsValue::from_str(key))
        .map_err(|e| anyhow::anyhow!("get property `{key}`: {e:?}"))?;
    if val.is_undefined() || val.is_null() {
        return Ok(None);
    }
    let f = val
        .as_f64()
        .with_context(|| format!("invalid type for property `{key}`"))?;
    Ok(Some(f64_to_u32_saturating_cast(f)))
}

fn parse_print_job_stream_callbacks(callbacks: JsValue) -> anyhow::Result<JsPrinterStreamCallbacks> {
    let callbacks = callbacks
        .dyn_into::<js_sys::Object>()
        .map_err(|_| anyhow::anyhow!("expected object"))?;

    Ok(JsPrinterStreamCallbacks {
        on_job_start: get_optional_function(&callbacks, "onJobStart")?,
        on_job_data: get_required_function(&callbacks, "onJobData")?,
        on_job_complete: get_required_function(&callbacks, "onJobComplete")?,
        on_job_error: get_optional_function(&callbacks, "onJobError")?,
    })
}

/// Parsed `driveShare` extension payload — see [`parse_drive_share`].
#[derive(Clone)]
struct DriveShareConfig {
    handle: FileSystemDirectoryHandle,
    share_name: String,
    read_only: bool,
}

fn parse_drive_share(value: JsValue) -> anyhow::Result<DriveShareConfig> {
    let obj = value.dyn_into::<js_sys::Object>().map_err(|_| anyhow::anyhow!("expected object"))?;

    let handle = js_sys::Reflect::get(&obj, &JsValue::from_str("handle"))
        .map_err(|e| anyhow::anyhow!("get property `handle`: {e:?}"))?
        .dyn_into::<FileSystemDirectoryHandle>()
        .map_err(|_| anyhow::anyhow!("property `handle` must be a FileSystemDirectoryHandle"))?;

    let share_name = js_sys::Reflect::get(&obj, &JsValue::from_str("shareName"))
        .ok()
        .and_then(|value| value.as_string())
        .filter(|name| !name.is_empty())
        .context("missing or empty `shareName`")?;

    let read_only = js_sys::Reflect::get(&obj, &JsValue::from_str("readOnly"))
        .ok()
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

    Ok(DriveShareConfig {
        handle,
        share_name,
        read_only,
    })
}

fn get_required_function(obj: &js_sys::Object, key: &str) -> anyhow::Result<js_sys::Function> {
    get_optional_function(obj, key)?.with_context(|| format!("missing function `{key}`"))
}

fn get_optional_function(obj: &js_sys::Object, key: &str) -> anyhow::Result<Option<js_sys::Function>> {
    let val = js_sys::Reflect::get(obj, &JsValue::from_str(key))
        .map_err(|e| anyhow::anyhow!("get property `{key}`: {e:?}"))?;

    if val.is_undefined() || val.is_null() {
        return Ok(None);
    }

    val.dyn_into::<js_sys::Function>()
        .map(Some)
        .map_err(|_| anyhow::anyhow!("property `{key}` must be a function"))
}

#[expect(clippy::as_conversions, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn f64_to_u32_saturating_cast(f: f64) -> u32 {
    f.clamp(0.0, f64::from(u32::MAX)) as u32
}

#[expect(clippy::as_conversions, clippy::cast_possible_truncation)]
fn f64_to_i32_saturating_cast(f: f64) -> i32 {
    f.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

/// Parse a JsValue (expected to be a JS array of file metadata objects)
/// into a `Vec<FileMetadata>`.
fn parse_file_metadata_array(files: JsValue) -> Result<Vec<FileMetadata>, IronError> {
    let js_array = js_sys::Array::from(&files);
    #[expect(
        clippy::as_conversions,
        reason = "JavaScript array length is u32, safe to convert to usize"
    )]
    let mut file_list = Vec::with_capacity(js_array.length() as usize);

    for i in 0..js_array.length() {
        let file_obj = js_array.get(i);
        let name = js_sys::Reflect::get(&file_obj, &JsValue::from_str("name"))
            .ok()
            .and_then(|v| v.as_string())
            .context("file name is required")?;
        let size_f64 = js_sys::Reflect::get(&file_obj, &JsValue::from_str("size"))
            .ok()
            .and_then(|v| v.as_f64())
            .context("file size is required")?;
        // JS numbers are f64; reject fractional or out-of-safe-integer-range values
        // to avoid silent truncation when casting to u64
        const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
        if !size_f64.is_finite() || size_f64 < 0.0 || size_f64.fract() != 0.0 || size_f64 > MAX_SAFE_INTEGER {
            return Err(anyhow::anyhow!(
                "file size must be a finite non-negative integer <= Number.MAX_SAFE_INTEGER (got: {size_f64})"
            )
            .into());
        }
        #[expect(clippy::as_conversions, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let size = size_f64 as u64;

        let last_modified_f64 = js_sys::Reflect::get(&file_obj, &JsValue::from_str("lastModified"))
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        // Store as JS timestamp (ms since Unix epoch). FileMetadata::to_file_descriptor()
        // handles the conversion to Windows FILETIME for the wire format.
        const MAX_SAFE_TS: f64 = 9_007_199_254_740_991.0;
        #[expect(clippy::as_conversions, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let last_modified = if last_modified_f64.is_finite()
            && (0.0..=MAX_SAFE_TS).contains(&last_modified_f64)
            && last_modified_f64.fract() == 0.0
        {
            last_modified_f64 as u64
        } else {
            0
        };

        let path = js_sys::Reflect::get(&file_obj, &JsValue::from_str("path"))
            .ok()
            .and_then(|v| v.as_string())
            .filter(|s| !s.is_empty());

        let is_directory = js_sys::Reflect::get(&file_obj, &JsValue::from_str("isDirectory"))
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        file_list.push(FileMetadata {
            name,
            path,
            size,
            last_modified,
            is_directory,
        });
    }

    Ok(file_list)
}

/// Client Info `performanceFlags` from a JS number, or `None` unless it is an integer
/// in `0..=u32::MAX`. Unknown bits are retained.
///
/// A named function rather than an inline block in `extension` so the conversion is
/// unit-testable.
fn performance_flags_from_js(value: f64) -> Option<PerformanceFlags> {
    if !(0.0..=f64::from(u32::MAX)).contains(&value) || value.fract() != 0.0 {
        return None;
    }

    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "checked above: a non-negative integer no larger than u32::MAX"
    )]
    let bits = value as u32;
    Some(PerformanceFlags::from_bits_retain(bits))
}

/// Client core data `desktopScaleFactor` from a JS number of percent, rounded, or
/// `None` outside the 100..=500 range MS-RDPBCGR 2.2.1.3.2 allows.
///
/// A named function rather than an inline block in `extension` so the conversion is
/// unit-testable.
fn desktop_scale_factor_from_js(percent: f64) -> Option<u32> {
    let percent = percent.round();
    if !(100.0..=500.0).contains(&percent) {
        return None;
    }

    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "checked above: a whole number between 100 and 500"
    )]
    Some(percent as u32)
}

fn build_config(
    username: String,
    password: String,
    domain: Option<String>,
    client_name: String,
    desktop_size: DesktopSize,
    legacy_graphics: bool,
    security: SecurityConfig,
    support_dyn_vc_gfx_protocol: bool,
) -> connector::Config {
    // Win7-class servers need 32-bpp lossless bitmaps and no advertised codecs.
    let bitmap = if legacy_graphics {
        connector::BitmapConfig {
            color_depth: 32,
            lossy_compression: false,
            codecs: BitmapCodecs(Vec::new()),
        }
    } else {
        connector::BitmapConfig {
            // 32 bpp, as mstsc asks, so the session runs in full color; with 16 the
            // connector does not request WANT_32_BPP_SESSION.
            color_depth: 32,
            lossy_compression: true,
            codecs: client_codecs_capabilities(&[]).expect("can't panic for &[]"),
        }
    };

    let enable_credssp = security.enable_credssp && !security.enable_standard_rdp_security;

    connector::Config {
        credentials: Credentials::UsernamePassword { username, password },
        domain,
        enable_tls: !security.enable_standard_rdp_security,
        enable_credssp,
        enable_standard_rdp_security: security.enable_standard_rdp_security,
        keyboard_type: ironrdp::pdu::gcc::KeyboardType::IBM_ENHANCED,
        keyboard_subtype: 0,
        keyboard_layout: 0, // the server SHOULD use the default active input locale identifier
        keyboard_functional_keys_count: 12,
        connection_type: ironrdp::pdu::gcc::ConnectionType::Lan,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: connector::DesktopSize {
            width: desktop_size.width,
            height: desktop_size.height,
        },
        monitor_layout: None,
        bitmap: Some(bitmap),
        #[expect(
            clippy::arithmetic_side_effects,
            reason = "fine unless we end up with an insanely big version"
        )]
        client_build: semver::Version::parse(env!("CARGO_PKG_VERSION"))
            .map_or(0, |version| version.major * 100 + version.minor * 10 + version.patch)
            .pipe(u32::try_from)
            .expect("fine until major ~42949672"),
        client_name,
        // NOTE: hardcode this value like in freerdp
        // https://github.com/FreeRDP/FreeRDP/blob/4e24b966c86fdf494a782f0dfcfc43a057a2ea60/libfreerdp/core/settings.c#LL49C34-L49C70
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        platform: ironrdp::pdu::rdp::capability_sets::MajorPlatformType::UNSPECIFIED,
        compression_type: None,
        enable_server_pointer: true,
        // Without CredSSP the Client Info PDU is the only place the credentials travel, and
        // Windows ignores its password unless auto-logon is requested, showing its own sign-in
        // screen instead. Entra ID accounts (`AzureAD\user@domain`) depend on this: they cannot
        // authenticate over NLA from a client that is not Entra-joined.
        autologon: !enable_credssp,
        enable_audio_playback: false,
        enable_audio_capture: false,
        request_data: None,
        pointer_software_rendering: false,
        multitransport_flags: None,
        support_dyn_vc_gfx_protocol,
        performance_flags: PerformanceFlags::default(),
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        remote_application_mode: false,
        rail_support_level: ironrdp::pdu::rdp::capability_sets::RailSupportLevel::SUPPORTED,
    }
}

async fn writer_task(
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    rdp_writer: WriteHalf<RdpStream>,
    outbound_limit: Option<usize>,
) {
    debug!("writer task started");

    async fn inner(
        mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
        mut rdp_writer: WriteHalf<RdpStream>,
        outbound_limit: Option<usize>,
    ) -> anyhow::Result<()> {
        while let Some(frame) = rx.next().await {
            match outbound_limit {
                Some(max_size) if frame.len() > max_size => {
                    // Send in chunks.
                    for chunk in frame.chunks(max_size) {
                        rdp_writer.write_all(chunk).await.context("couldn't write chunk")?;
                        rdp_writer.flush().await.context("couldn't flush chunk")?;
                    }
                }
                _ => {
                    // Send complete frame (default case).
                    rdp_writer.write_all(&frame).await.context("couldn't write frame")?;
                    rdp_writer.flush().await.context("couldn't flush frame")?;
                }
            }
        }

        Ok(())
    }

    match inner(rx, rdp_writer, outbound_limit).await {
        Ok(()) => debug!("writer task ended gracefully"),
        Err(e) => error!("writer task ended unexpectedly: {e:#}"),
    }
}

struct ConnectParams {
    ws: WebSocket,
    config: connector::Config,
    destination: String,
    /// Hyper-V VMConnect PCB payload (e.g. `<vm-guid>;EnhancedMode=1`).
    /// When set, the connection targets a Hyper-V host console (port 2179)
    /// using the PCB → TLS → CredSSP → X.224 ordering.
    vmconnect: Option<String>,
    kdc_proxy_url: Option<String>,
    clipboard_backend: Option<WasmClipboardBackend>,
    printer_backend: Option<WasmPrinterBackend>,
    printer_device_id: u32,
    printer_name: String,
    printer_driver_name: String,
    /// `Some((backend, share_name))` when a `driveShare` extension configured
    /// a redirected drive for this session. Coexists with `printer_backend` —
    /// both are announced on the same RDPDR channel via
    /// [`WasmCompositeBackend`] when both are configured. See
    /// [`RdpInputEvent::DriveBackend`] for how this backend's completions
    /// reach the event loop.
    drive: Option<(WasmDriveBackend, String)>,
    /// Shared with the `Session` so a folder picked mid-session can be installed
    /// into the live composite backend — see [`crate::drive::composite::DriveSlot`]
    /// and the `drive_share` arm of `invoke_extension`.
    drive_slot: DriveSlot,
    /// Matches the `client_name` in the connector config; used as the
    /// `computer_name` when constructing the `Rdpdr` processor.
    computer_name: String,
    use_display_control: bool,
    /// Attach the EGFX graphics pipeline. Both this and `use_display_control`
    /// ride the same DRDYNVC static channel, so either one alone is enough to
    /// warrant attaching it.
    use_egfx: bool,
    /// WebCodecs H.264 decoder for EGFX; `None` keeps AVC unadvertised.
    h264_decoder: Option<Box<dyn H264Decoder>>,
    /// GPU surface backend for EGFX; `None` keeps the CPU compositor.
    gpu_backend: Option<crate::gpu::GlBackend>,
    frame_ack_policy: FrameAckPolicy,
    /// Set when this connection follows a Server Redirection PDU.
    redirect: Option<RedirectConnector>,
    /// `Some` when the `enable_audio_playback` extension is on and Web Audio is available.
    rdpsnd_backend: Option<WasmRdpsndBackend>,
}

fn default_printer_driver_name() -> String {
    printer_driver_name_for_macos_major_version(browser_macos_major_version()).to_owned()
}

fn printer_driver_name_for_macos_major_version(macos_major_version: Option<u32>) -> &'static str {
    if macos_major_version.is_some_and(|major| 14 <= major) {
        MICROSOFT_PRINT_TO_PDF_DRIVER_NAME
    } else {
        DEFAULT_PRINTER_DRIVER_NAME
    }
}

#[cfg(target_arch = "wasm32")]
fn browser_macos_major_version() -> Option<u32> {
    let user_agent = web_sys::window()?.navigator().user_agent().ok()?;
    macos_major_version_from_user_agent(&user_agent)
}

#[cfg(not(target_arch = "wasm32"))]
fn browser_macos_major_version() -> Option<u32> {
    None
}

#[cfg(any(target_arch = "wasm32", test))]
fn macos_major_version_from_user_agent(user_agent: &str) -> Option<u32> {
    let (_, version) = user_agent.split_once("Mac OS X ")?;
    version
        .split(|ch: char| !ch.is_ascii_digit())
        .next()
        .and_then(|major| major.parse().ok())
}

async fn connect(
    ConnectParams {
        ws,
        config,
        destination,
        vmconnect,
        kdc_proxy_url,
        clipboard_backend,
        printer_backend,
        printer_device_id,
        printer_name,
        printer_driver_name,
        drive,
        drive_slot,
        computer_name,
        use_display_control,
        use_egfx,
        h264_decoder,
        gpu_backend,
        frame_ack_policy,
        redirect,
        rdpsnd_backend,
    }: ConnectParams,
) -> Result<(connector::ConnectionResult, RdpStream, bytes::BytesMut), IronError> {
    let framed = ironrdp_futures::LocalFuturesFramed::new(ws);

    // In web browser environments, we do not have an easy access to the local address of the socket.
    let dummy_client_addr = core::net::SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 33899));

    let mut connector = ClientConnector::new(config, dummy_client_addr);

    if let Some(RedirectConnector { routing_token, rdstls }) = redirect {
        if let Some(routing_token) = routing_token {
            connector = connector.with_load_balance_info(routing_token);
        }
        if let Some(rdstls) = rdstls {
            connector = connector.with_rdstls(rdstls);
        }
    }

    if let Some(clipboard_backend) = clipboard_backend {
        connector.attach_static_channel(CliprdrClient::new(Box::new(clipboard_backend)));
    }

    // RDPSND is attached unconditionally, like every real-world client (mstsc,
    // FreeRDP). Windows gates more than audio on its presence:
    //   - RDPDR: servers only speak on it when RDPSND is advertised too
    //     (MS-RDPEFS Appendix A<1>).
    //   - DRDYNVC: observed empirically against Windows Server — with only
    //     cliprdr + drdynvc announced, the server completed the connection and
    //     then never initiated the DVC capability exchange at all (no EGFX, no
    //     DisplayControl), going silent after Font Map and dropping the session
    //     60 seconds later. The identical build with RDPSND announced gets the
    //     DVC caps request immediately after finalization.
    // With audio playback enabled it gets the Web Audio backend; otherwise the no-op
    // processor exists only to satisfy those dependencies.
    let rdpsnd_backend: Box<dyn RdpsndClientHandler> = match rdpsnd_backend {
        Some(backend) => Box::new(backend),
        None => Box::new(NoopRdpsndBackend),
    };
    connector.attach_static_channel(Rdpsnd::new(rdpsnd_backend));

    // Drive and printer redirection coexist on the one RDPDR channel `Rdpdr` accepts a single
    // backend for, via `WasmCompositeBackend` — see `drive::composite`'s module doc comment for
    // how it routes each IRP-handling method to whichever member is configured. Both device
    // classes are announced together in one `Rdpdr` when both are configured.
    // RDPDR is attached unconditionally, not only when a device is configured up front: the
    // drive capability must be negotiated with the server BEFORE its capability exchange for
    // `add_dynamic_drive` to be legal later (it rejects late capability configuration outright),
    // so a session that starts with no redirection would otherwise be permanently unable to
    // mount a folder the user picks mid-session. Announcing zero devices on an otherwise idle
    // channel is what mstsc does when redirection is enabled with nothing to redirect.
    //
    // RDPSND used to be attached here, inside this block; it now goes up
    // unconditionally with the other static channels above, because Windows
    // gates the DVC capability exchange on it and not just RDPDR.
    {
        // `DRIVE_DEVICE_ID` is a fixed constant but `printer_device_id` is caller-settable via
        // the `printerDeviceId()` extension — resolve a collision deterministically before
        // either device is announced (see `resolve_drive_device_id`'s doc comment). Mandatory
        // per review: `Rdpdr` has no deduplication of its own, so two devices sharing an id would
        // otherwise both get announced under the same identifier.
        let (drive_backend, drive_share_name, drive_device_id) = match drive {
            Some((backend, share_name)) => {
                let device_id = resolve_drive_device_id(DRIVE_DEVICE_ID, printer_device_id);
                (Some(backend), Some(share_name), Some(device_id))
            }
            None => (None, None, None),
        };

        let printer_configured = printer_backend.is_some();
        let printer_member = printer_backend.map(|backend| (backend, printer_device_id));
        *drive_slot.borrow_mut() = drive_backend.zip(drive_device_id);

        let composite = WasmCompositeBackend::new(printer_member, Rc::clone(&drive_slot));

        let mut rdpdr = Rdpdr::new(Box::new(composite), computer_name);
        if printer_configured {
            rdpdr = rdpdr.with_printer_driver(printer_device_id, printer_name, printer_driver_name);
        }
        // `with_drives` is called either way: with the initial drive when one is configured, and
        // with `None` otherwise purely to enable the capability so a later `add_dynamic_drive`
        // is accepted (see the attach comment above).
        match (drive_share_name, drive_device_id) {
            (Some(share_name), Some(device_id)) => {
                rdpdr = rdpdr.with_drives(Some(vec![(device_id, share_name)]));
            }
            _ => {
                rdpdr = rdpdr.with_drives(None);
            }
        }
        connector.attach_static_channel(rdpdr);
    }

    // DisplayControl and the graphics pipeline are both dynamic channels riding
    // the one DRDYNVC static channel, so it is attached when either is wanted —
    // EGFX must not be nested under the DisplayControl gate.
    if use_display_control || use_egfx {
        let mut drdynvc = DrdynvcClient::new();

        if use_display_control {
            drdynvc = drdynvc.with_dynamic_channel(DisplayControlClient::new(|_| Ok(Vec::new())));
        }

        if use_egfx {
            // The client-side compositor (ironrdp-egfx) holds the surface pixel
            // state and the shared `ActiveStage::process` drains it into the
            // framebuffer, so the pipeline's notification callbacks carry no
            // rendering duty — the native client's `EgfxHandler` implements none
            // of them. The three overridden here are pure breadcrumbs: whether a
            // session actually renders via EGFX or silently fell back to legacy
            // bitmaps is otherwise invisible from the outside.
            //
            // Without an H.264 decoder `start()` drops every AVC capability set,
            // leaving only codecs decodable in wasm. With the WebCodecs one, V8_1's
            // AVC420 goes out and Windows may encode the desktop as H.264.
            struct WebEgfxHandler {
                /// An H.264 decoder is attached, so offer the 10.x set Windows needs for AVC.
                h264: bool,
                logged_first_frame: bool,
            }

            impl GraphicsPipelineHandler for WebEgfxHandler {
                fn capabilities(&self) -> Vec<CapabilitySet> {
                    // Same sets as the trait default — this override exists only to
                    // prove the channel opened. `start()` calls this the moment the
                    // server creates the DVC, and every DVC-layer log in
                    // ironrdp-dvc is debug!, which Chrome hides.
                    //
                    // AVC420_ENABLED stays on V8_1 exactly as upstream has it;
                    // `start()` filters that set out when no H.264 decoder is
                    // configured, leaving V8 as what actually goes on the wire.
                    let mut caps = vec![
                        CapabilitySet::V8_1 {
                            flags: CapabilitiesV81Flags::AVC420_ENABLED | CapabilitiesV81Flags::SMALL_CACHE,
                        },
                        CapabilitySet::V8 {
                            flags: CapabilitiesV8Flags::SMALL_CACHE,
                        },
                    ];
                    // Windows 11 confirms V8_1 with AVC420_ENABLED cleared: it only enables
                    // H.264 for 10.x clients, and then sends AVC444, shown as its 4:2:0 main
                    // view. AVC_THIN_CLIENT is left off: with it Windows sent luma only but at
                    // the same heavy quantization.
                    if self.h264 {
                        caps.insert(
                            0,
                            CapabilitySet::V10_7 {
                                flags: CapabilitiesV107Flags::SMALL_CACHE,
                            },
                        );
                    }
                    info!(?caps, "EGFX channel OPENED — advertising capabilities");
                    caps
                }

                fn on_capabilities_confirmed(&mut self, caps: &CapabilitySet) {
                    // The definitive "EGFX is live" signal, and it names the
                    // version the server settled on.
                    info!(?caps, "EGFX capabilities confirmed — rendering via graphics pipeline");
                }

                fn on_reset_graphics(&mut self, width: u32, height: u32) {
                    // Proves a resize took the ResetGraphics path rather than a
                    // deactivate-reactivate round trip.
                    info!(width, height, "EGFX ResetGraphics");
                }

                fn on_frame_complete(&mut self, frame_id: u32) {
                    // First frame only: this fires for every frame in the session.
                    if !self.logged_first_frame {
                        self.logged_first_frame = true;
                        info!(frame_id, "EGFX first frame complete — pixels flowing");
                    }
                }

                fn on_close(&mut self) {
                    // A server that opens the channel and then closes it is a very
                    // different failure from one that never opens it at all.
                    info!("EGFX channel CLOSED by server");
                }
            }

            egfx_capture::install_if_requested();

            info!(h264 = h264_decoder.is_some(), "Attaching EGFX graphics pipeline");
            let handler = WebEgfxHandler {
                h264: h264_decoder.is_some(),
                logged_first_frame: false,
            };
            let graphics = GraphicsPipelineClient::new(Box::new(handler), h264_decoder)
                .with_frame_ack_policy(frame_ack_policy, crate::clock_ms);
            let graphics = match gpu_backend {
                Some(gl) => graphics.with_surface_backend(Box::new(gl)),
                None => graphics,
            };
            drdynvc = drdynvc.with_dynamic_channel(graphics);
        }

        connector.attach_static_channel(drdynvc);
    }

    let mut network_client = WasmNetworkClient;
    let server_name = connector::ServerName::from(destination.as_str());
    let kerberos_config = url::Url::parse(kdc_proxy_url.unwrap_or_default().as_str())
        .ok()
        .map(|url| KerberosConfig {
            kdc_proxy_url: Some(url),
            hostname: destination,
        });

    // Direct connection mode: the WebSocket carries raw RDP bytes and TLS is
    // terminated right here in the browser (see `connect_direct`).
    let (mut framed, upgraded, server_public_key) = connect_direct(
        framed,
        &mut connector,
        vmconnect,
        &mut network_client,
        server_name.clone(),
        kerberos_config.clone(),
    )
    .await?;

    let connection_result = ironrdp_futures::connect_finalize(
        upgraded,
        connector,
        &mut framed,
        &mut network_client,
        server_name,
        server_public_key,
        kerberos_config,
    )
    .await?;

    // The leftover MUST travel with the stream. Windows sends its first
    // post-connection PDUs — the DRDYNVC capabilities request, CLIPRDR
    // capabilities, RDPSND formats — immediately after the Font Map, and over a
    // buffered transport they routinely coalesce into the same read. Whatever
    // arrived beyond the last finalization PDU is sitting in this framed's
    // buffer; the previous `into_inner_no_leftover()` silently discarded it
    // (its debug_assert is compiled out in release wasm). Losing the DVC caps
    // request means the server never gets a caps response, initiates nothing
    // further — no EGFX, no DisplayControl, no clipboard — and drops the
    // session after its 60-second timeout. Whether the burst coalesced was a
    // network-timing race, which is why this presented as sessions that
    // sometimes worked and usually sat on a black canvas until disconnect.
    // The native client has always threaded leftovers through every handoff
    // (`TokioFramed::new_with_leftover`).
    let (stream, leftover) = framed.into_inner();

    Ok((connection_result, stream, leftover))
}

/// Diagnostic EGFX capture for the browser, the wasm stand-in for the native
/// `IRONRDP_DUMP_EGFX=<dir>` file dump.
///
/// Off unless the page defines `globalThis.__ironrdpEgfxDump(seq, bytes)`
/// before connecting (the test rig does, behind a build flag). Each raw server
/// message is handed to it in order; saved as `egfx-<seq>.bin`, the files
/// replay with the `egfx_replay` example exactly like a native capture.
mod egfx_capture {
    use core::cell::{Cell, RefCell};

    use wasm_bindgen::JsValue;

    const DUMP_FN: &str = "__ironrdpEgfxDump";

    thread_local! {
        static DUMP: RefCell<Option<js_sys::Function>> = const { RefCell::new(None) };
        static SEQ: Cell<u32> = const { Cell::new(0) };
    }

    /// Looks for the page's dump function and, when present, routes every EGFX
    /// message of the next session to it. Numbering restarts at zero because
    /// ZGFX state is per connection: a capture is only replayable from its start.
    pub(super) fn install_if_requested() {
        let dump = js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str(DUMP_FN))
            .ok()
            .and_then(|value| wasm_bindgen::JsCast::dyn_into::<js_sys::Function>(value).ok());

        let enabled = dump.is_some();
        DUMP.with(|slot| *slot.borrow_mut() = dump);
        SEQ.with(|seq| seq.set(0));
        let hook: ironrdp_egfx::client::CaptureHook = forward;
        ironrdp_egfx::client::set_capture_hook(enabled.then_some(hook));

        if enabled {
            tracing::warn!("EGFX capture enabled: every server graphics message goes to {DUMP_FN}");
        }
    }

    fn forward(payload: &[u8]) {
        DUMP.with(|slot| {
            if let Some(dump) = slot.borrow().as_ref() {
                let seq = SEQ.with(|seq| {
                    let current = seq.get();
                    seq.set(current.wrapping_add(1));
                    current
                });
                // `from` copies out of wasm memory, so the JS side may hold the
                // bytes past this call (it posts them asynchronously).
                let bytes = js_sys::Uint8Array::from(payload);
                let _ = dump.call2(&JsValue::NULL, &JsValue::from(seq), &bytes);
            }
        });
    }
}

/// Direct connection mode (no RDCleanPath, no TLS-terminating proxy).
///
/// The WebSocket is a plain byte pipe to the RDP target — anything that relays bytes
/// will do. This function drives the pre-TLS part of the sequence over that raw pipe,
/// performs the TLS handshake in the browser (`crate::tls`), and returns a fresh
/// [`ironrdp_futures::Framed`] over the TLS stream for CredSSP and everything after it.
///
/// 1. X.224 connection request/confirm, in the clear, until the server asks for the
///    security upgrade.
/// 2. TLS handshake, terminated here. The peer certificate yields the server public
///    key CredSSP binds to — previously asserted by the relay, now observed directly.
/// 3. `connect_finalize` continues on the TLS stream.
///
/// When `vmconnect` is set, the target is a Hyper-V host console (port 2179) and the
/// ordering changes per MS-RDPEPS: the Preconnection Blob goes out first, in the clear,
/// TLS follows immediately, and CredSSP runs *before* X.224 via
/// [`ironrdp_vmconnect::connect_front`].
async fn connect_direct(
    mut framed: ironrdp_futures::LocalFuturesFramed<WebSocket>,
    connector: &mut ClientConnector,
    vmconnect: Option<String>,
    network_client: &mut WasmNetworkClient,
    server_name: connector::ServerName,
    kerberos_config: Option<KerberosConfig>,
) -> Result<(RdpFramed, ironrdp_futures::Upgraded, Vec<u8>), IronError> {
    if let Some(pcb_payload) = vmconnect {
        info!("Begin direct VMConnect connection procedure");

        // Hyper-V ordering: PCB → TLS → CredSSP → X.224. The PCB is sent in the clear,
        // before the handshake, and the host replies to it with the TLS ServerHello.
        let pcb_bytes = ironrdp_vmconnect::encode_preconnection_blob_payload(pcb_payload)
            .context("encode preconnection blob")?;

        framed
            .write_all(&pcb_bytes)
            .await
            .context("couldn't write preconnection blob")?;

        let (mut framed, server_public_key) = upgrade_to_tls(framed, &server_name).await?;

        let upgraded = ironrdp_vmconnect::connect_front(
            // The PCB was written above, on this stream, rather than by a proxy on our
            // behalf; `pcb_sent_via_proxy` is only how the token is minted outside
            // `send_preconnection_blob`, which wants a VM id and mode we do not have here.
            ironrdp_vmconnect::pcb_sent_via_proxy(),
            &mut framed,
            connector,
            network_client,
            server_name,
            &server_public_key,
            kerberos_config,
        )
        .await
        .context("connect Hyper-V front over direct WebSocket")?;

        return Ok((framed, upgraded, server_public_key));
    }

    info!("Begin direct connection procedure");

    // Step 1: X.224 connection request/confirm, in the clear.
    let mut buf = WriteBuf::new();

    while !connector.should_perform_security_upgrade() {
        single_sequence_step(&mut framed, connector, &mut buf).await?;
    }

    // Step 2: TLS handshake, terminated in the browser.
    let (framed, server_public_key) = upgrade_to_tls(framed, &server_name).await?;

    // Step 3: tell the connector the upgrade happened. `connect_begin`'s own loop is
    // skipped because step 1 above already ran it.
    let should_upgrade = ironrdp_futures::skip_connect_begin(connector);
    let upgraded = ironrdp_futures::mark_as_upgraded(should_upgrade, connector);

    Ok((framed, upgraded, server_public_key))
}

/// Takes the raw WebSocket out of `framed`, performs the TLS handshake over it, and
/// returns a [`ironrdp_futures::Framed`] over the resulting TLS stream.
async fn upgrade_to_tls(
    framed: ironrdp_futures::LocalFuturesFramed<WebSocket>,
    server_name: &connector::ServerName,
) -> Result<(RdpFramed, Vec<u8>), IronError> {
    let (ws, leftover) = framed.into_inner();

    // Nothing is expected between the last pre-TLS PDU and the ServerHello: the server
    // does not speak again until it has seen our ClientHello. Bytes here would be part
    // of the TLS stream we are about to open, and there is no way to feed them to
    // rustls after the fact — so fail loudly rather than silently dropping them.
    if !leftover.is_empty() {
        return Err(IronError::from(anyhow::anyhow!(
            "{} unread bytes before the TLS handshake",
            leftover.len()
        ))
        .with_kind(IronErrorKind::General));
    }

    let (tls_stream, server_public_key) = crate::tls::upgrade(ws, server_name.as_str())
        .await
        .context("TLS handshake")?;

    Ok((
        ironrdp_futures::LocalFuturesFramed::new(tls_stream),
        server_public_key,
    ))
}

#[expect(clippy::as_conversions, clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn f64_to_u16_saturating_cast(value: f64) -> u16 {
    value as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_sessions_ask_for_32_bit_color() {
        // mstsc asks for 32 bpp; 16 bpp left Windows free to run the session in 65,536 colors.
        let config = connector_config(SecurityConfig {
            enable_credssp: true,
            enable_standard_rdp_security: false,
        });
        assert_eq!(config.bitmap.map(|bitmap| bitmap.color_depth), Some(32));
    }

    #[test]
    fn desktop_scale_factor_from_js_accepts_the_rdp_range() {
        assert_eq!(desktop_scale_factor_from_js(200.0), Some(200));
        assert_eq!(desktop_scale_factor_from_js(100.0), Some(100));
        assert_eq!(desktop_scale_factor_from_js(500.0), Some(500));
        // Fractional device pixel ratios round to whole percents.
        assert_eq!(desktop_scale_factor_from_js(149.6), Some(150));
        for value in [99.0, 501.0, 0.0, -200.0, f64::NAN, f64::INFINITY] {
            assert_eq!(desktop_scale_factor_from_js(value), None, "{value}");
        }
    }

    #[test]
    fn performance_flags_from_js_takes_any_u32_bit_set() {
        assert_eq!(
            performance_flags_from_js(f64::from(0x0000_0180u32)),
            Some(PerformanceFlags::ENABLE_FONT_SMOOTHING | PerformanceFlags::ENABLE_DESKTOP_COMPOSITION)
        );
        assert_eq!(performance_flags_from_js(0.0), Some(PerformanceFlags::empty()));
        // Unknown bits are kept, not dropped (STYLE.md "Decoding unknown values").
        assert_eq!(
            performance_flags_from_js(f64::from(u32::MAX)).map(|flags| flags.bits()),
            Some(u32::MAX)
        );
    }

    #[test]
    fn performance_flags_from_js_rejects_values_that_are_not_a_u32() {
        for value in [-1.0, 1.5, f64::from(u32::MAX) + 1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(performance_flags_from_js(value), None, "{value}");
        }
    }

    // Test helpers
    fn create_test_input_channel() -> (
        mpsc::UnboundedSender<RdpInputEvent>,
        mpsc::UnboundedReceiver<RdpInputEvent>,
    ) {
        mpsc::unbounded()
    }

    fn connector_config(security: SecurityConfig) -> connector::Config {
        build_config(
            String::new(),
            String::new(),
            None,
            "ironrdp-web-test".to_owned(),
            DesktopSize {
                width: DEFAULT_WIDTH,
                height: DEFAULT_HEIGHT,
            },
            false,
            security,
            false,
        )
    }

    #[test]
    fn printer_driver_defaults_to_postscript_when_macos_version_is_unknown() {
        assert_eq!(
            printer_driver_name_for_macos_major_version(None),
            DEFAULT_PRINTER_DRIVER_NAME
        );
    }

    #[test]
    fn printer_driver_uses_pdf_for_macos_14_and_newer() {
        assert_eq!(
            printer_driver_name_for_macos_major_version(Some(13)),
            DEFAULT_PRINTER_DRIVER_NAME
        );
        assert_eq!(
            printer_driver_name_for_macos_major_version(Some(14)),
            MICROSOFT_PRINT_TO_PDF_DRIVER_NAME
        );
        assert_eq!(
            printer_driver_name_for_macos_major_version(Some(15)),
            MICROSOFT_PRINT_TO_PDF_DRIVER_NAME
        );
    }

    #[test]
    fn macos_major_version_is_parsed_from_user_agent() {
        assert_eq!(
            macos_major_version_from_user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 14_6_1) AppleWebKit/605.1.15"),
            Some(14)
        );
        assert_eq!(
            macos_major_version_from_user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36"),
            Some(10)
        );
        assert_eq!(
            macos_major_version_from_user_agent("Mozilla/5.0 (Windows NT 10.0)"),
            None
        );
    }

    #[test]
    fn standard_rdp_security_overrides_enhanced_security() {
        for (case, enable_credssp, enable_standard_rdp_security, expected_tls, expected_credssp, expected_autologon) in [
            ("secure defaults", true, false, true, true, false),
            ("credssp disabled", false, false, true, false, true),
            ("standard RDP security", true, true, false, false, true),
        ] {
            let config = connector_config(SecurityConfig {
                enable_credssp,
                enable_standard_rdp_security,
            });

            assert_eq!(config.enable_tls, expected_tls, "{case}");
            assert_eq!(config.enable_credssp, expected_credssp, "{case}");
            assert_eq!(config.autologon, expected_autologon, "{case}");
        }
    }

    #[test]
    fn test_request_file_contents_parameter_marshalling() {
        let (tx, mut rx) = create_test_input_channel();

        // Send request with various parameters
        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::FileContentsRequestSend {
                stream_id: 123,
                index: 5,
                flags: FileContentsFlags::RANGE,
                position: 1024,
                size: 4096,
                clip_data_id: Some(42),
            },
        ))
        .unwrap();

        // Verify message parameters
        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::FileContentsRequestSend {
                stream_id,
                index,
                flags,
                position,
                size,
                clip_data_id,
            })) => {
                assert_eq!(stream_id, 123);
                assert_eq!(index, 5);
                assert_eq!(flags, FileContentsFlags::RANGE);
                assert_eq!(position, 1024);
                assert_eq!(size, 4096);
                assert_eq!(clip_data_id, Some(42));
            }
            _ => panic!("Expected FileContentsRequestSend with correct parameters"),
        }
    }

    #[test]
    fn test_request_file_contents_size_flag() {
        let (tx, mut rx) = create_test_input_channel();

        // Send SIZE request
        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::FileContentsRequestSend {
                stream_id: 1,
                index: 0,
                flags: FileContentsFlags::SIZE,
                position: 0,
                size: 8,
                clip_data_id: Some(1),
            },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::FileContentsRequestSend {
                flags, ..
            })) => {
                assert_eq!(flags, FileContentsFlags::SIZE);
            }
            _ => panic!("Expected SIZE request"),
        }
    }

    #[test]
    fn test_request_file_contents_without_clip_data_id() {
        let (tx, mut rx) = create_test_input_channel();

        // Send request without clip_data_id
        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::FileContentsRequestSend {
                stream_id: 10,
                index: 0,
                flags: FileContentsFlags::RANGE,
                position: 0,
                size: 1024,
                clip_data_id: None,
            },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::FileContentsRequestSend {
                clip_data_id,
                ..
            })) => {
                assert_eq!(clip_data_id, None);
            }
            _ => panic!("Expected request without clip_data_id"),
        }
    }

    #[test]
    fn test_submit_file_contents_success_response() {
        let (tx, mut rx) = create_test_input_channel();

        let data = vec![1, 2, 3, 4, 5];
        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::FileContentsResponseSend {
                stream_id: 42,
                is_error: false,
                data: data.clone(),
            },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::FileContentsResponseSend {
                stream_id,
                is_error,
                data: received_data,
            })) => {
                assert_eq!(stream_id, 42);
                assert!(!is_error);
                assert_eq!(received_data, data);
            }
            _ => panic!("Expected FileContentsResponseSend success"),
        }
    }

    #[test]
    fn test_submit_file_contents_error_response() {
        let (tx, mut rx) = create_test_input_channel();

        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::FileContentsResponseSend {
                stream_id: 99,
                is_error: true,
                data: vec![],
            },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::FileContentsResponseSend {
                is_error,
                ..
            })) => {
                assert!(is_error);
            }
            _ => panic!("Expected error response"),
        }
    }

    #[test]
    fn test_submit_file_contents_size_response() {
        let (tx, mut rx) = create_test_input_channel();

        // 8-byte size response (little-endian)
        let size_data = vec![0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]; // 4096 bytes

        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::FileContentsResponseSend {
                stream_id: 1,
                is_error: false,
                data: size_data.clone(),
            },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::FileContentsResponseSend {
                data, ..
            })) => {
                assert_eq!(data.len(), 8);
                assert_eq!(data, size_data);
            }
            _ => panic!("Expected size response"),
        }
    }

    #[test]
    fn test_initiate_file_copy_message() {
        let (tx, mut rx) = create_test_input_channel();

        let files = vec![
            FileMetadata {
                name: "file1.txt".to_owned(),
                path: None,
                size: 1024,
                last_modified: 1_700_000_000_000,
                is_directory: false,
            },
            FileMetadata {
                name: "file2.pdf".to_owned(),
                path: Some("docs".to_owned()),
                size: 2048,
                last_modified: 1_700_000_001_000,
                is_directory: false,
            },
        ];

        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::InitiateFileCopy { files },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::InitiateFileCopy {
                files: received_files,
            })) => {
                assert_eq!(received_files.len(), 2);
                assert_eq!(received_files[0].name, "file1.txt");
                assert_eq!(received_files[0].path, None);
                assert_eq!(received_files[0].size, 1024);
                assert_eq!(received_files[1].name, "file2.pdf");
                assert_eq!(received_files[1].path, Some("docs".to_owned()));
                assert_eq!(received_files[1].size, 2048);
            }
            _ => panic!("Expected InitiateFileCopy message"),
        }
    }

    #[test]
    fn test_large_position_value_marshalling() {
        let (tx, mut rx) = create_test_input_channel();

        // Test with large position value (near u64 max)
        let large_position = u64::MAX - 1000;

        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::FileContentsRequestSend {
                stream_id: 1,
                index: 0,
                flags: FileContentsFlags::RANGE,
                position: large_position,
                size: 1024,
                clip_data_id: Some(1),
            },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::FileContentsRequestSend {
                position,
                ..
            })) => {
                assert_eq!(position, large_position);
            }
            _ => panic!("Expected correct position marshalling"),
        }
    }

    #[test]
    fn test_zero_size_file() {
        let (tx, mut rx) = create_test_input_channel();

        let files = vec![FileMetadata {
            name: "empty.txt".to_owned(),
            path: None,
            size: 0,
            last_modified: 0,
            is_directory: false,
        }];

        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::InitiateFileCopy { files },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::InitiateFileCopy {
                files: received_files,
            })) => {
                assert_eq!(received_files[0].size, 0);
            }
            _ => panic!("Expected zero-size file"),
        }
    }

    #[test]
    fn test_file_with_special_characters_in_name() {
        let (tx, mut rx) = create_test_input_channel();

        let files = vec![FileMetadata {
            name: "test file (1) [copy].txt".to_owned(),
            path: None,
            size: 100,
            last_modified: 0,
            is_directory: false,
        }];

        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::InitiateFileCopy { files },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::InitiateFileCopy {
                files: received_files,
            })) => {
                assert_eq!(received_files[0].name, "test file (1) [copy].txt");
            }
            _ => panic!("Expected file with special characters"),
        }
    }

    #[test]
    fn test_empty_file_list() {
        let (tx, mut rx) = create_test_input_channel();

        let files: Vec<FileMetadata> = vec![];

        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::InitiateFileCopy { files },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::InitiateFileCopy {
                files: received_files,
            })) => {
                assert!(received_files.is_empty());
            }
            _ => panic!("Expected empty file list"),
        }
    }

    #[test]
    fn test_flags_bits_conversion() {
        let (tx, mut rx) = create_test_input_channel();

        // Test SIZE flag (0x1)
        let size_flags = FileContentsFlags::SIZE;
        assert_eq!(size_flags.bits(), 0x1);

        // Test DATA flag (0x2)
        let data_flags = FileContentsFlags::RANGE;
        assert_eq!(data_flags.bits(), 0x2);

        // Test that flags convert correctly through the channel
        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::FileContentsRequestSend {
                stream_id: 1,
                index: 0,
                flags: FileContentsFlags::from_bits_truncate(0x1),
                position: 0,
                size: 8,
                clip_data_id: None,
            },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::FileContentsRequestSend {
                flags, ..
            })) => {
                assert_eq!(flags.bits(), 0x1);
            }
            _ => panic!("Expected correct flags conversion"),
        }
    }
}
