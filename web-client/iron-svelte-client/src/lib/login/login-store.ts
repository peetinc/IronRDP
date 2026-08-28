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
export const autoResizeEnabled = writable(true);
