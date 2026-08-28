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

export function kdcProxyUrl(url: string): Extension {
    return new Extension('kdc_proxy_url', url);
}

export function outboundMessageSizeLimit(limit: number): Extension {
    return new Extension('outbound_message_size_limit', limit);
}

export function enableCredssp(enable: boolean): Extension {
    return new Extension('enable_credssp', enable);
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
