import { writable } from 'svelte/store';

export const showLogin = writable(true);

// Auto-resize lives in a store rather than component state so it can be set on
// the login form BEFORE connecting. remote-screen.svelte binds the same store,
// so its in-session toolbar checkbox and the login checkbox stay in sync.
//
// This is independent of the EGFX toggle: it controls who asks for a size
// change (RDPEDISP), not how pixels are encoded. Turning it off still renders
// through the graphics pipeline.
//
// Off is for comparing renders at a fixed desktop size without a mount-time
// ResizeObserver resize racing the first frames. The ResetGraphics half of the
// EGFX matrix needs it ON — that path only runs in response to an RDPEDISP
// request.
// Known loose end: the rig can fire several identical resize requests in the
// first second (the dedupe in 2f336c09 does not always hold) — harmless to
// EGFX, which proved out on a fixed surface, but worth tightening.
export const autoResizeEnabled = writable(import.meta.env.VITE_RIG_AUTO_RESIZE !== 'false');

// HiDPI: size the desktop in device pixels and ask Windows to scale to match
// (200% on a 2x screen), for sharp text. Latched at connect; remote-screen.svelte
// reads it for every auto-resize so the desktop keeps the same density.
export const hidpiEnabled = writable(import.meta.env.VITE_RIG_HIDPI === 'true');

/** Device pixels per CSS pixel for a HiDPI desktop, clamped to what RDP scaling covers. */
export function hidpiRatio(): number {
    return Math.min(3, Math.max(1, window.devicePixelRatio || 1));
}
