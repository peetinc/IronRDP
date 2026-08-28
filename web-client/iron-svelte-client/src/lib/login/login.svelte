<script lang="ts">
    import { currentSession, setCurrentSessionActive, userInteractionService } from '../../services/session.service';
    import type { IronError, UserInteraction } from '../../../static/iron-remote-desktop';
    import type { Session } from '../../models/session';
    import type { PrintJobEntry } from '../../models/print-job';
    import {
        displayControl,
        egfx,
        kdcProxyUrl,
        init,
        driveShare,
        printerName,
        printerDriverName,
        printJobStreamCallbacks,
    } from '../../../static/iron-remote-desktop-rdp';
    import { toast } from '$lib/messages/message-store';
    import { autoResizeEnabled, showLogin } from '$lib/login/login-store';
    import { onMount } from 'svelte';

    // e2e test-rig hook: reports RDPDR printer job progress up to the
    // session page, which owns the visible job list (Login unmounts on
    // connect, so it cannot render it itself).
    export let onPrintJobUpdate: (fileId: number, patch: Partial<PrintJobEntry>) => void = () => {};

    let username = 'artichoke';
    let password = '';
    // 9096, not 9095: the RDPDR rig on the direct-websocket-mode checkout owns
    // 9095 (and vite 5180), and concurrent sessions must not fight over ports.
    let gatewayAddress = 'ws://localhost:9096';
    let hostname = '10.10.100.78';
    let domain = 'peetinc';
    let kdc_proxy_url = '';
    let desktopSize = { width: 1280, height: 720 };
    let pop_up = false;
    let enable_clipboard = true;
    // EGFX kill-switch for the T4 test matrix: unticking this must fall back to
    // the legacy bitmap path and still render.
    let enable_egfx = true;

    // e2e test-rig hook: RDPDR folder share. Populated via
    // window.showDirectoryPicker(); kept component-scoped (no module-level
    // store) so it never leaks between sessions.
    let folderHandle: FileSystemDirectoryHandle | null = null;
    let folderName = '';
    let folderReadOnly = false;

    // The File System Access API cannot open a picker at an arbitrary path, but a picked
    // directory HANDLE can be persisted (IndexedDB) and re-armed on the next visit with a
    // permission re-grant instead of a fresh picker. Tick "Allow on every visit" in Chrome's
    // re-grant prompt and subsequent loads restore the share with zero clicks.
    const RIG_DB = 'ironrdp-rig';
    const RIG_STORE = 'handles';
    const RIG_KEY = 'share-folder';

    function rigDb(): Promise<IDBDatabase> {
        return new Promise((resolve, reject) => {
            const open = indexedDB.open(RIG_DB, 1);
            open.onupgradeneeded = () => open.result.createObjectStore(RIG_STORE);
            open.onsuccess = () => resolve(open.result);
            open.onerror = () => reject(open.error);
        });
    }

    async function saveStoredFolder(handle: FileSystemDirectoryHandle) {
        try {
            const db = await rigDb();
            await new Promise<void>((resolve, reject) => {
                const tx = db.transaction(RIG_STORE, 'readwrite');
                tx.objectStore(RIG_STORE).put(handle, RIG_KEY);
                tx.oncomplete = () => resolve();
                tx.onerror = () => reject(tx.error);
            });
        } catch (err) {
            console.warn('Could not persist share-folder handle:', err);
        }
    }

    async function loadStoredFolder(): Promise<FileSystemDirectoryHandle | null> {
        try {
            const db = await rigDb();
            return await new Promise((resolve, reject) => {
                const req = db.transaction(RIG_STORE, 'readonly').objectStore(RIG_STORE).get(RIG_KEY);
                req.onsuccess = () => resolve(req.result ?? null);
                req.onerror = () => reject(req.error);
            });
        } catch {
            return null;
        }
    }

    type PermissionCapable = FileSystemDirectoryHandle & {
        queryPermission(opts: { mode: string }): Promise<PermissionState>;
        requestPermission(opts: { mode: string }): Promise<PermissionState>;
    };

    async function pickFolder() {
        try {
            // First choice: re-arm the previously shared folder with a permission prompt only
            // (no picker). Falls through to the full picker when nothing is stored or the
            // re-grant is denied.
            const stored = (await loadStoredFolder()) as PermissionCapable | null;
            if (stored && folderHandle === null) {
                if ((await stored.requestPermission({ mode: 'readwrite' })) === 'granted') {
                    folderHandle = stored;
                    folderName = stored.name;
                    return;
                }
            }
            // `showDirectoryPicker` is not in the default lib.dom typings used
            // here; guarded by the `'showDirectoryPicker' in window` check below.
            const handle = await (
                window as unknown as {
                    showDirectoryPicker: (opts: {
                        mode: string;
                        startIn?: FileSystemDirectoryHandle;
                    }) => Promise<FileSystemDirectoryHandle>;
                }
            ).showDirectoryPicker({
                mode: 'readwrite',
                ...(stored ? { startIn: stored } : {}),
            });
            folderHandle = handle;
            folderName = handle.name;
            void saveStoredFolder(handle);
        } catch (err) {
            // User cancelled the picker or denied permission — nothing to report.
            console.warn('Folder share picker dismissed:', err);
        }
    }

    async function restoreFolderIfPermitted() {
        const stored = (await loadStoredFolder()) as PermissionCapable | null;
        if (stored && (await stored.queryPermission({ mode: 'readwrite' })) === 'granted') {
            folderHandle = stored;
            folderName = stored.name;
        }
    }

    function clearFolder() {
        folderHandle = null;
        folderName = '';
        folderReadOnly = false;
    }

    let userInteraction: UserInteraction;

    userInteractionService.subscribe((val) => {
        userInteraction = val;
    });

    const isIronError = (error: unknown): error is IronError => {
        return (
            typeof error === 'object' &&
            error !== null &&
            typeof (error as IronError).backtrace === 'function' &&
            typeof (error as IronError).kind === 'function'
        );
    };

    const StartSession = async () => {
        toast.set({
            type: 'info',
            message: 'Connection in progress...',
        });

        if (pop_up) {
            const data = JSON.stringify({
                username,
                password,
                hostname,
                gatewayAddress,
                domain,
                desktopSize,
                kdc_proxy_url,
                enable_clipboard,
            });
            const base64Data = btoa(data);
            window.open(
                `/popup-session?data=${base64Data}`,
                '_blank',
                `width=${desktopSize.width},height=${desktopSize.height},resizable=yes,scrollbars=yes,status=yes`,
            );
            return;
        }

        userInteraction.setEnableClipboard(enable_clipboard);

        const configBuilder = userInteraction
            .configBuilder()
            .withUsername(username)
            .withPassword(password)
            .withDestination(hostname)
            .withProxyAddress(gatewayAddress)
            .withServerDomain(domain)
            .withAuthToken('')
            .withDesktopSize(desktopSize)
            .withExtension(displayControl(true))
            .withExtension(egfx(enable_egfx));

        if (kdc_proxy_url !== '') {
            configBuilder.withExtension(kdcProxyUrl(kdc_proxy_url));
        }

        if (folderHandle != null) {
            configBuilder.withExtension(
                driveShare({ handle: folderHandle, shareName: folderName, readOnly: folderReadOnly }),
            );
        }

        // e2e test-rig: always wire up the virtual printer so a print from
        // inside the session can be captured and surfaced as a downloadable
        // PDF. Note: driveShare and the printer extensions are mutually
        // exclusive in the RDPDR backend today (drive share wins if both are
        // configured) — see iron-remote-desktop-rdp/src/extensions.ts.
        const printJobChunks = new Map<number, Uint8Array[]>();

        // e2e bisect toggle: set false to test the drive share with NO printer
        // device announced (both ride the same RDPDR channel via the composite
        // backend, so a printer-side failure would take the drive down too).
        const ENABLE_PRINTER = false;

        if (ENABLE_PRINTER)
        configBuilder
            .withExtension(printerName('LithiumBridge Printer'))
            .withExtension(printerDriverName('Microsoft Print to PDF'))
            .withExtension(
                printJobStreamCallbacks({
                    onJobStart: (fileId) => {
                        printJobChunks.set(fileId, []);
                        onPrintJobUpdate(fileId, { status: 'printing' });
                    },
                    onJobData: (fileId, chunk) => {
                        const chunks = printJobChunks.get(fileId);
                        if (chunks != null) {
                            chunks.push(chunk);
                        } else {
                            printJobChunks.set(fileId, [chunk]);
                        }
                    },
                    onJobComplete: (fileId) => {
                        const chunks = printJobChunks.get(fileId) ?? [];
                        printJobChunks.delete(fileId);
                        const blob = new Blob(chunks, { type: 'application/pdf' });
                        const url = URL.createObjectURL(blob);
                        onPrintJobUpdate(fileId, { status: 'ready', url });
                    },
                    onJobError: (fileId) => {
                        printJobChunks.delete(fileId);
                        onPrintJobUpdate(fileId, { status: 'error', error: 'Print job failed' });
                    },
                }),
            );

        const config = configBuilder.build();

        try {
            const session_info = await userInteraction.connect(config);

            toast.set({
                type: 'info',
                message: 'Success',
            });

            const updater = (session: Session): Session => ({
                ...session,
                sessionId: session_info.sessionId,
                desktopSize: session_info.initialDesktopSize,
                active: true,
            });

            currentSession.update(updater);

            showLogin.set(false);

            userInteraction.setVisibility(true);

            const sessionTerminationInfo = await session_info.run();

            toast.set({
                type: 'info',
                message: `Session terminated gracefully: ${sessionTerminationInfo.reason()}`,
            });
        } catch (err) {
            setCurrentSessionActive(false);
            showLogin.set(true);

            if (isIronError(err)) {
                toast.set({
                    type: 'error',
                    message: err.backtrace(),
                });
            } else {
                toast.set({
                    type: 'error',
                    message: `${err}`,
                });
            }
        }
    };

    onMount(async () => {
        // Bump to 'DEBUG' when diagnosing the drive share — every IRP logs at debug!, and the
        // DevTools level filter cannot reveal lines the wasm tracing level never emits. Left at
        // INFO normally: DEBUG floods the console hard enough to stall the tab on big transfers.
        await init('INFO');
        // Zero-click share restore when Chrome granted persistent permission
        // ("Allow on every visit"); otherwise the Share Folder button re-arms
        // the stored handle with a one-click prompt instead of a picker.
        void restoreFolderIfPermitted();
    });
</script>

<main class="responsive login-container">
    <div class="login-content">
        <div class="grid">
            <div class="s2" />
            <div class="s8">
                <article class="primary-container">
                    <h5>Login</h5>
                    <div class="medium-space" />
                    <div>
                        <div class="field label border">
                            <input id="hostname" type="text" bind:value={hostname} />
                            <label for="hostname">Hostname</label>
                        </div>
                        <div class="field label border">
                            <input id="domain" type="text" bind:value={domain} />
                            <label for="domain">Domain</label>
                        </div>
                        <div class="field label border">
                            <input id="username" type="text" bind:value={username} />
                            <label for="username">Username</label>
                        </div>
                        <div class="field label border">
                            <input id="password" type="password" bind:value={password} />
                            <label for="password">Password</label>
                        </div>
                        <div class="field label border">
                            <input id="gatewayAddress" type="text" bind:value={gatewayAddress} />
                            <label for="gatewayAddress">Gateway Address</label>
                        </div>
                        <div class="field label border">
                            <input id="desktopSizeW" type="text" bind:value={desktopSize.width} />
                            <label for="desktopSizeW">Desktop Width</label>
                        </div>
                        <div class="field label border">
                            <input id="desktopSizeH" type="text" bind:value={desktopSize.height} />
                            <label for="desktopSizeH">Desktop Height</label>
                        </div>
                        <div class="field label border">
                            <input id="kdc_proxy_url" type="text" bind:value={kdc_proxy_url} />
                            <label for="kdc_proxy_url">KDC Proxy URL</label>
                        </div>
                        {#if 'showDirectoryPicker' in window}
                            <div class="folder-share-row">
                                {#if folderName === ''}
                                    <button type="button" on:click={pickFolder}>Share Folder</button>
                                {:else}
                                    <span class="folder-share-chip">
                                        📁 {folderName}
                                        <label class="folder-share-readonly">
                                            <input type="checkbox" bind:checked={folderReadOnly} />
                                            Read-only
                                        </label>
                                        <button
                                            type="button"
                                            class="folder-share-clear"
                                            on:click={clearFolder}
                                            aria-label="Clear folder share"
                                        >
                                            ×
                                        </button>
                                    </span>
                                {/if}
                            </div>
                        {/if}
                        <div class="field label border checkbox-container">
                            <div class="checkbox-wrapper">
                                <input
                                    id="use_pop_up"
                                    type="checkbox"
                                    bind:checked={pop_up}
                                    style="width: 1.5em; height: 1.5em; margin-right: 0.5em;"
                                />
                                <label for="use_pop_up">Use Pop Up</label>
                            </div>
                            <div class="checkbox-wrapper">
                                <input
                                    id="enable_clipboard"
                                    type="checkbox"
                                    bind:checked={enable_clipboard}
                                    style="width: 1.5em; height: 1.5em; margin-right: 0.5em;"
                                />
                                <label for="enable_clipboard">Enable Clipboard</label>
                            </div>
                            <div class="checkbox-wrapper">
                                <input
                                    id="enable_egfx"
                                    type="checkbox"
                                    bind:checked={enable_egfx}
                                    style="width: 1.5em; height: 1.5em; margin-right: 0.5em;"
                                />
                                <label for="enable_egfx">Enable EGFX</label>
                            </div>
                            <div class="checkbox-wrapper">
                                <input
                                    id="auto_resize"
                                    type="checkbox"
                                    bind:checked={$autoResizeEnabled}
                                    style="width: 1.5em; height: 1.5em; margin-right: 0.5em;"
                                />
                                <label for="auto_resize">Auto Resize</label>
                            </div>
                        </div>
                    </div>
                    <nav class="center-align">
                        <button on:click={StartSession}>Login</button>
                    </nav>
                </article>
            </div>
            <div class="s2" />
        </div>
    </div>
</main>

<style>
    @import './login.css';
</style>
