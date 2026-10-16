import wasm_init, {
    setup,
    DesktopSize,
    DeviceEvent,
    InputTransaction,
    SessionBuilder,
    ClipboardData,
    Extension,
    RdpFile,
} from '../../../crates/ironrdp-web/pkg/ironrdp_web';

export async function init(log_level: string) {
    await wasm_init();
    setup(log_level);
}

export { RdpFile };

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
