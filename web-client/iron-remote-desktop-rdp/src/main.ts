import wasm_init, {
    setup,
    DesktopSize,
    DeviceEvent,
    InputTransaction,
    SessionBuilder,
    ClipboardData,
    Extension,
    RdpFile,
    EgfxBench,
} from '../../../crates/ironrdp-web/pkg/ironrdp_web';

export async function init(log_level: string) {
    await wasm_init();
    setup(log_level);
}

export { RdpFile };

/** Replays a recorded EGFX capture through the real pipeline (test rig only). */
export { EgfxBench };

export const Backend = {
    DesktopSize: DesktopSize,
    InputTransaction: InputTransaction,
    SessionBuilder: SessionBuilder,
    ClipboardData: ClipboardData,
    DeviceEvent: DeviceEvent,
};

// --- Pre-connection configuration extensions ---

export function preConnectionBlob(pcb: string): Extension {
    return new Extension('pcb', pcb);
}

export type VmConnectMode = 'enhanced' | 'basic';

export function vmConnect(vmId: string, mode: VmConnectMode = 'enhanced'): Extension {
    if (vmId.trim() === '') {
        throw new Error('vmconnect requires a VM ID');
    }

    const payload = mode === 'enhanced' ? `${vmId};EnhancedMode=1` : vmId;
    return new Extension('vmconnect', payload);
}

export function displayControl(enable: boolean): Extension {
    return new Extension('display_control', enable);
}

/**
 * Toggle the EGFX graphics pipeline (progressive RemoteFX / ClearCodec).
 *
 * Enabled by default — EGFX is negotiated, so a host that does not offer it
 * never opens the channel and the legacy bitmap path is used untouched. Pass
 * `false` as a kill-switch if rendering regresses.
 *
 * No H.264 is involved: the pipeline is built without a decoder, so every AVC
 * capability set is filtered out at advertisement time.
 */
export function egfx(enable: boolean): Extension {
    return new Extension('egfx', enable);
}

/**
 * Decode H.264 (AVC420) with the browser's WebCodecs `VideoDecoder`, in hardware where
 * available, so Windows can encode the desktop as H.264. Off by default. Ignored when EGFX
 * is off or the browser cannot decode H.264, in which case the CPU codecs are used as before.
 */
export function h264(enable: boolean): Extension {
    return new Extension('h264', enable);
}

/**
 * Renders EGFX on the GPU (WebGL2): surfaces live in textures and H.264 frames are drawn
 * without being read back. Off by default. Falls back to the 2D canvas without WebGL2, or
 * when the desktop is larger than the GPU's texture limit. A lost GPU context ends the session.
 */
export function gpuRendering(enable: boolean): Extension {
    return new Extension('gpu_rendering', enable);
}

/**
 * Acknowledge EGFX frames the way Windows App does. Off by default. When on, once the client
 * keeps up it suspends frame acknowledgements so Windows does not pace itself to them, sends a
 * QoE report per frame instead, and resumes acknowledging when its decode queue backs up. It
 * sees only its own queue, not the network's: over a slow link (a tunnel across the internet)
 * Windows can then outrun the link and audio on the same connection breaks up. Use it on a LAN.
 */
export function frameAckSuspend(enable: boolean): Extension {
    return new Extension('frame_ack_suspend', enable);
}

/**
 * Play remote session audio (RDPSND) in the browser through Web Audio. Off by default.
 * Browsers only start audio after a user gesture on the page; connecting from a click satisfies that.
 */
export function enableAudioPlayback(enable: boolean): Extension {
    return new Extension('enable_audio_playback', enable);
}

/**
 * Client Info `performanceFlags` bits (MS-RDPBCGR 2.2.1.11.1.1.1): which desktop
 * effects the server may render. `DISABLE_*` bits turn an effect off, `ENABLE_*`
 * bits turn one on.
 */
export const PerformanceFlag = {
    DISABLE_WALLPAPER: 0x0000_0001,
    DISABLE_FULLWINDOWDRAG: 0x0000_0002,
    DISABLE_MENUANIMATIONS: 0x0000_0004,
    DISABLE_THEMING: 0x0000_0008,
    DISABLE_CURSOR_SHADOW: 0x0000_0020,
    DISABLE_CURSORSETTINGS: 0x0000_0040,
    ENABLE_FONT_SMOOTHING: 0x0000_0080,
    ENABLE_DESKTOP_COMPOSITION: 0x0000_0100,
} as const;

/** What is sent when no `performanceFlags` extension is given: no window contents while dragging, no menu animations, font smoothing on. */
export const DEFAULT_PERFORMANCE_FLAGS =
    PerformanceFlag.DISABLE_FULLWINDOWDRAG |
    PerformanceFlag.DISABLE_MENUANIMATIONS |
    PerformanceFlag.ENABLE_FONT_SMOOTHING;

/**
 * Replace the Client Info performance flags, a bitwise OR of {@link PerformanceFlag}
 * values. Applies at connect. A value that is not an integer in the u32 range is
 * ignored and the defaults ({@link DEFAULT_PERFORMANCE_FLAGS}) are sent.
 */
export function performanceFlags(flags: number): Extension {
    return new Extension('performance_flags', flags);
}

/**
 * Desktop scale factor sent at connect, in percent (100 to 500; MS-RDPBCGR 2.2.1.3.2),
 * e.g. 200 for a desktop sized in device pixels on a 2x HiDPI screen. Later scale
 * changes travel with each `resize` call. Out-of-range values are ignored.
 */
export function desktopScaleFactor(percent: number): Extension {
    return new Extension('desktop_scale_factor', percent);
}

export function kdcProxyUrl(url: string): Extension {
    return new Extension('kdc_proxy_url', url);
}

export function outboundMessageSizeLimit(limit: number): Extension {
    return new Extension('outbound_message_size_limit', limit);
}

export function enableCredssp(enable: boolean): Extension {
    return new Extension('enable_credssp', enable);
}

/**
 * Allow unencrypted Standard RDP Security over a trusted RDCleanPath transport.
 *
 * Enabling this disables TLS and CredSSP protocol negotiation. Use it only when
 * the WebSocket and proxy-to-server transports already provide the required
 * confidentiality and peer authentication.
 */
export function enableStandardRdpSecurity(enable: boolean): Extension {
    return new Extension('enable_standard_rdp_security', enable);
}

export function enableServerPointer(enable: boolean): Extension {
    return new Extension('enable_server_pointer', enable);
}

export function legacyGraphics(enable: boolean): Extension {
    return new Extension('legacy_graphics', enable);
}

// --- File transfer (RDP-specific) ---

export { RdpFileTransferProvider } from './RdpFileTransferProvider';
export type {
    RdpFileTransferProviderOptions,
    TransferProgress,
    FileTransferError,
    DownloadHandle,
    UploadHandle,
    DroppedFile,
} from './RdpFileTransferProvider';
export type { FileInfo, FileContentsRequest, FileContentsResponse } from './FileTransfer';
export { FileContentsFlags } from './FileContentsFlags';

// --- Storage backends ---
// Re-export for consumers who want to configure the storageBackend
// option on RdpFileTransferProviderOptions, implement a custom backend,
// or construct a specific backend instance directly.
export type { FileStorageBackend, FileWriteHandle, StorageBackendPreference } from './storage';
export { BlobStorageBackend } from './storage';
export { OpfsStorageBackend } from './storage';
export { detectStorageBackend } from './storage';

// Re-export extension factories for advanced consumers who want to
// register callbacks or invoke file transfer operations directly.
export {
    filesAvailableCallback,
    fileContentsRequestCallback,
    fileContentsResponseCallback,
    lockCallback,
    unlockCallback,
    locksExpiredCallback,
    requestFileContents,
    submitFileContents,
    initiateFileCopy,
    printJobStreamCallbacks,
    PrinterDriverName,
    printerName,
    printerDeviceId,
    printerDriverName,
    driveShare,
} from './extensions';
export type { PrintJobStreamCallbacks, DriveShareParams } from './extensions';
