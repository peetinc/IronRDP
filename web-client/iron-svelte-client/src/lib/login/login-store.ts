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
export const autoResizeEnabled = writable(true);
