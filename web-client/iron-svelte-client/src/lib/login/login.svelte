<script lang="ts">
    import { currentSession, setCurrentSessionActive, userInteractionService } from '../../services/session.service';
    import type { IronError, UserInteraction } from '../../../static/iron-remote-desktop';
    import type { Session } from '../../models/session';
    import type { PrintJobEntry } from '../../models/print-job';
    import {
        displayControl,
        kdcProxyUrl,
        init,
        driveShare,
        printerName,
        printerDriverName,
        printJobStreamCallbacks,
    } from '../../../static/iron-remote-desktop-rdp';
    import { toast } from '$lib/messages/message-store';
    import { showLogin } from '$lib/login/login-store';
    import { onMount } from 'svelte';

    // e2e test-rig hook: reports RDPDR printer job progress up to the
    // session page, which owns the visible job list (Login unmounts on
    // connect, so it cannot render it itself).
    export let onPrintJobUpdate: (fileId: number, patch: Partial<PrintJobEntry>) => void = () => {};

    let username = 'artichoke';
    let password = '';
    let gatewayAddress = 'ws://localhost:9095';
    let hostname = '10.10.100.78';
    let domain = 'peetinc';
    let kdc_proxy_url = '';
    let desktopSize = { width: 1280, height: 720 };
    let pop_up = false;
    let enable_clipboard = true;

    // e2e test-rig hook: RDPDR folder share. Populated via
    // window.showDirectoryPicker(); kept component-scoped (no module-level
    // store) so it never leaks between sessions.
    let folderHandle: FileSystemDirectoryHandle | null = null;
    let folderName = '';
    let folderReadOnly = false;

    async function pickFolder() {
        try {
            // `showDirectoryPicker` is not in the default lib.dom typings used
            // here; guarded by the `'showDirectoryPicker' in window` check below.
            const handle = await (window as unknown as { showDirectoryPicker: (opts: { mode: string }) => Promise<FileSystemDirectoryHandle> }).showDirectoryPicker({
                mode: 'readwrite',
            });
            folderHandle = handle;
            folderName = handle.name;
        } catch (err) {
            // User cancelled the picker or denied permission — nothing to report.
            console.warn('Folder share picker dismissed:', err);
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
            .withExtension(displayControl(true));

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
        await init('INFO');
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
