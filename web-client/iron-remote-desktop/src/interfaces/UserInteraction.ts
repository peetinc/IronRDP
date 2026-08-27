import type { ScreenScale } from '../enums/ScreenScale';
import type { NewSessionInfo } from './NewSessionInfo';
import { ConfigBuilder } from '../services/ConfigBuilder';
import type { Config } from '../services/Config';
import type { Extension } from './Extension';
import type { Callback } from '../lib/Observable';
import type { FileTransferProvider } from './FileTransferProvider';

export interface UserInteraction {
    setVisibility(state: boolean): void;

    setScale(scale: ScreenScale): void;

    configBuilder(): ConfigBuilder;

    connect(config: Config): Promise<NewSessionInfo>;

    setKeyboardUnicodeMode(useUnicode: boolean): void;

    ctrlAltDel(): void;

    metaKey(): void;

    ctrlC(): void;

    ctrlV(): void;

    /**
     * Send a raw keyboard scan code press or release event, bypassing the local keyboard
     * event pipeline (layout mapping, dead-key composition, autorepeat filtering).
     *
     * `scancode` uses the same PS/2 Set 1 scan code form taken by the underlying
     * `DeviceEvent.keyPressed` / `keyReleased` bridge calls (see
     * `ironrdp_input::Scancode::from_u16`): the low byte is the make code, and the
     * extended (`E0`-prefixed) flag is encoded by setting the `0xE000` bits — e.g. left
     * Ctrl is `0x001D`, Delete is `0xE053`, left Windows key is `0xE05B`.
     *
     * This is a raw press/release primitive: the caller is responsible for pairing
     * presses with releases, e.g. to implement caller-managed sticky modifiers
     * (Shift/Ctrl/Win/Alt held across separate key events).
     */
    sendKey(scancode: number, pressed: boolean): void;

    /**
     * Type a string as a sequence of Unicode key press/release pairs
     * (`DeviceEvent.unicodePressed` / `unicodeReleased`), one pair per Unicode code
     * point.
     *
     * Iterates `text` by Unicode code point rather than UTF-16 code unit, so characters
     * outside the Basic Multilingual Plane (represented as UTF-16 surrogate pairs) are
     * sent as a single event instead of two malformed halves.
     *
     * Unlike `sendKey`, this path is keyboard-layout independent — the server applies
     * the Unicode code point directly rather than requiring a matching scan code — which
     * makes it the correct way to inject text (e.g. a password) on surfaces where
     * clipboard redirection does not run, such as the Windows logon/lock/UAC secure
     * desktop.
     */
    typeText(text: string): void;

    shutdown(): void;

    setCursorStyleOverride(style: string | null): void;

    onWarningCallback(callback: Callback<string>): void;

    onClipboardRemoteUpdateCallback(callback: Callback<void>): void;

    resize(width: number, height: number, scale?: number): void;

    setEnableClipboard(enable: boolean): void;

    setEnableAutoClipboard(enable: boolean): void;

    saveRemoteClipboardData(): Promise<void>;

    sendClipboardData(): Promise<void>;

    invokeExtension(ext: Extension): void;

    /**
     * Enable file transfer support. Must be called before connect().
     * The provider becomes active after connect() resolves.
     * Implicitly enables clipboard (required for file transfer protocol).
     *
     * @param provider - Protocol-specific file transfer provider (e.g., RdpFileTransferProvider)
     * @returns The same provider, with monitoring hooks composed in
     */
    enableFileTransfer(provider: FileTransferProvider): FileTransferProvider;
}
