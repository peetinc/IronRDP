<script lang="ts">
    import { onMount } from 'svelte';
    import { userInteractionService } from '../../services/session.service';
    import { showLogin } from '$lib/login/login-store';
    import type { UserInteraction } from '../../../static/iron-remote-desktop';
    import { Backend } from '../../../static/iron-remote-desktop-rdp';

    let uiService: UserInteraction;
    let cursorOverrideActive = false;
    let showDebugPanel = false;
    let autoResize = true;
    let screenEl: HTMLElement | null = null;
    let resizeTimer: ReturnType<typeof setTimeout> | undefined;
    let verifyTimer: ReturnType<typeof setTimeout> | undefined;
    let lastRequested: { width: number; height: number } | null = null;
    let retriesLeft = 0;

    userInteractionService.subscribe((uis) => {
        if (uis != null) {
            uiService = uis;
        }
    });

    // Ask the server (via the Display Control DVC) to match the desktop size
    // to the space available for the canvas. Debounced: RDPEDISP resizes are
    // expensive server-side, so wait for the drag to settle.
    //
    // Measure the AVAILABLE space from the parent container, not the
    // iron-remote-desktop element: the component pins its viewer to a fixed
    // pixel size after each dynamic resize, so the element itself stops
    // tracking window resizes.
    function scheduleResize() {
        if (!autoResize) {
            return;
        }
        clearTimeout(resizeTimer);
        resizeTimer = setTimeout(() => {
            if (uiService == null || screenEl == null || screenEl.parentElement == null) {
                return;
            }
            const parentRect = screenEl.parentElement.getBoundingClientRect();
            const top = screenEl.getBoundingClientRect().top;
            const width = Math.floor(parentRect.width);
            const height = Math.floor(parentRect.bottom - top);
            if (width > 0 && height > 0 && (lastRequested?.width !== width || lastRequested?.height !== height)) {
                lastRequested = { width, height };
                retriesLeft = 5;
                requestResize();
            }
        }, 500);
    }

    function requestResize() {
        if (uiService == null || lastRequested == null) {
            return;
        }
        uiService.resize(lastRequested.width, lastRequested.height);
        // The request is dropped client-side if the Display Control channel
        // is not ready yet (e.g. right after connect), and there is no
        // feedback either way. Verify the canvas picked up the new size and
        // retry a few times if it did not.
        clearTimeout(verifyTimer);
        verifyTimer = setTimeout(() => {
            if (screenEl == null || lastRequested == null || retriesLeft <= 0) {
                return;
            }
            const canvas = (screenEl.shadowRoot ?? screenEl).querySelector('canvas');
            if (canvas == null) {
                return;
            }
            // The client rounds odd widths down; accept a small delta.
            const applied =
                Math.abs(canvas.width - lastRequested.width) <= 2 &&
                Math.abs(canvas.height - lastRequested.height) <= 2;
            if (!applied) {
                retriesLeft -= 1;
                requestResize();
            }
        }, 1000);
    }

    // Fire once when the session becomes visible so the desktop immediately
    // matches the canvas instead of the size requested at connect time.
    $: if (!$showLogin) {
        scheduleResize();
    }

    function onUnicodeModeChange(e: MouseEvent) {
        if (e.target == null) {
            return;
        }

        let element = e.target as HTMLInputElement;

        if (element == null) {
            return;
        }

        uiService.setKeyboardUnicodeMode(element.checked);
    }

    function toggleCursorKind() {
        if (cursorOverrideActive) {
            uiService.setCursorStyleOverride(null);
        } else {
            uiService.setCursorStyleOverride('url("crosshair.png") 7 7, default');
        }

        cursorOverrideActive = !cursorOverrideActive;
    }

    onMount(async () => {
        let el = document.querySelector('iron-remote-desktop');

        if (el == null) {
            throw '`iron-remote-desktop` element not found';
        }

        el.addEventListener('ready', (e) => {
            const event = e as CustomEvent;
            userInteractionService.set(event.detail.irgUserInteraction);
        });

        screenEl = el as HTMLElement;
        // Observe the flex container: it follows the window; the pinned
        // iron-remote-desktop element does not (see scheduleResize).
        const observer = new ResizeObserver(() => scheduleResize());
        observer.observe(screenEl.parentElement ?? screenEl);
        window.addEventListener('resize', () => scheduleResize());
    });
</script>

<div style="display: flex; height: 100%; flex-direction: column; background-color: #2e2e2e;" class:hideall={$showLogin}>
    <div>
        <div style="text-align: center; padding: 10px; background: black;">
            <button on:click={() => (showDebugPanel = !showDebugPanel)}>Toggle debug panel</button>
            <button on:click={() => uiService.setScale(1)}>Fit</button>
            <button on:click={() => uiService.setScale(2)}>Full</button>
            <button on:click={() => uiService.setScale(3)}>Real</button>
            <button on:click={() => uiService.ctrlAltDel()}>Ctrl+Alt+Del</button>
            <button on:click={() => uiService.metaKey()}
                >Meta
                <svg xmlns="http://www.w3.org/2000/svg" width="26" height="26" viewBox="0 0 512 512"
                    ><title> ionicons-v5_logos</title>
                    <path d="M480,265H232V444l248,36V265Z" />
                    <path d="M216,265H32V415l184,26.7V265Z" />
                    <path d="M480,32,232,67.4V249H480V32Z" />
                    <path d="M216,69.7,32,96V249H216V69.7Z" />
                </svg>
            </button>
            <button on:click={() => toggleCursorKind()}>Toggle cursor kind</button>
            <button on:click={() => uiService.shutdown()}>Terminate Session</button>
            <label style="color: white;">
                <input on:click={(e) => onUnicodeModeChange(e)} type="checkbox" />
                Unicode keyboard mode
            </label>
            <label style="color: white;">
                <input type="checkbox" bind:checked={autoResize} on:change={() => scheduleResize()} />
                Auto resize
            </label>
        </div>

        {#if showDebugPanel}
            <div id="debug-panel" style="background: black; color: white; padding: 10px;">
                debug-panel
                <input
                    type="text"
                    id="debug-panel-input"
                    style="width: 100%; height: 100%; background: black; color: white;"
                    placeholder="see if focus moves correctly"
                />

                <p>see if text selection works correctly</p>
            </div>
        {/if}
    </div>
    <iron-remote-desktop verbose="true" scale="fit" flexcenter="true" module={Backend} />
</div>

<style>
    .hideall {
        display: none !important;
    }
</style>
