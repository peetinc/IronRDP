import { describe, it, expect, vi, afterEach } from 'vitest';
import { mount, unmount, flushSync } from 'svelte';
import IronRemoteDesktop from './iron-remote-desktop.svelte';
import type { RemoteDesktopModule } from './interfaces/RemoteDesktopModule';

/**
 * A dynamic resize sizes the viewer to the new desktop in CSS pixels. With a scale factor (a HiDPI
 * desktop sized in device pixels, e.g. 200 on a 2x screen) the desktop is that many times denser than
 * CSS pixels, so the viewer must be divided down, or the page shows only its top-left corner.
 */

function createIdleModule(): RemoteDesktopModule {
    const SessionBuilder = class {
        constructor() {
            return new Proxy(this, { get: () => () => undefined });
        }
    };
    return {
        SessionBuilder,
        DesktopSize: class {},
        InputTransaction: class {},
        ClipboardData: class {},
        DeviceEvent: {},
    } as unknown as RemoteDesktopModule;
}

describe('dynamic resize', () => {
    let component: ReturnType<typeof mount> | null = null;

    afterEach(() => {
        if (component) {
            unmount(component);
            component = null;
        }
        vi.unstubAllGlobals();
        document.body.innerHTML = '';
    });

    async function mountViewer() {
        vi.stubGlobal('isSecureContext', false);
        const target = document.createElement('div');
        document.body.appendChild(target);
        const ready = vi.fn();
        target.addEventListener('ready', ready);
        component = mount(IronRemoteDesktop, {
            target,
            props: { scale: 'fit', verbose: 'false', flexcenter: 'true', module: createIdleModule() },
        });
        await vi.waitFor(() => expect(ready).toHaveBeenCalledTimes(1));
        const ui = (ready.mock.calls[0][0] as CustomEvent).detail.irgUserInteraction;
        const viewer = target.querySelector('.screen-viewer') as HTMLElement;
        return { ui, viewer };
    }

    it('sizes the viewer in CSS pixels when a scale factor is given', async () => {
        const { ui, viewer } = await mountViewer();

        ui.resize(2000, 1200, 200);
        flushSync();

        expect(viewer.style.width).toBe('1000px');
        expect(viewer.style.height).toBe('600px');
    });

    it('sizes the viewer to the desktop without a scale factor', async () => {
        const { ui, viewer } = await mountViewer();

        ui.resize(1600, 900);
        flushSync();

        expect(viewer.style.width).toBe('1600px');
        expect(viewer.style.height).toBe('900px');
    });
});
