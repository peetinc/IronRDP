// e2e test-rig only: mirror console output to a local sink so logs can be read
// from a file instead of copied out of DevTools.
//
// Why this exists: a single console.warn from the wasm module carries a 21-frame
// stack trace, and every frame prints the 4.7 MB inlined base64 wasm as its
// source URL. Copying a short session out of DevTools produced a 116 MB paste.
// Chrome also hides console.debug behind "Default levels", so debug!() output
// was invisible for two rounds of debugging.
//
// Only the formatted message text is forwarded — never stack traces, never the
// bundle URL. Anything longer than MAX_LINE is truncated.

const SINK = 'http://localhost:9097';
const MAX_LINE = 2000;
const FLUSH_MS = 500;

let queue: string[] = [];
let timer: ReturnType<typeof setTimeout> | undefined;

function flush() {
    timer = undefined;
    if (queue.length === 0) {
        return;
    }
    const batch = queue;
    queue = [];
    // keepalive so a batch queued during teardown still leaves the page.
    void fetch(SINK, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(batch),
        keepalive: true,
    }).catch(() => {
        // Sink not running is fine — the rig must never depend on it.
    });
}

function format(level: string, args: unknown[]): string {
    const text = args
        .map((arg) => {
            if (typeof arg === 'string') {
                return arg;
            }
            // Errors stringify to "{}" via JSON, which loses the message.
            if (arg instanceof Error) {
                return `${arg.name}: ${arg.message}`;
            }
            try {
                return JSON.stringify(arg);
            } catch {
                return String(arg);
            }
        })
        .join(' ')
        .replace(/\s+/g, ' ')
        .trim();
    const line = `[${level}] ${text}`;
    return line.length > MAX_LINE ? `${line.slice(0, MAX_LINE)}…<truncated>` : line;
}

export function installLogForwarding() {
    for (const level of ['log', 'info', 'warn', 'error', 'debug'] as const) {
        const original = console[level].bind(console);
        console[level] = (...args: unknown[]) => {
            original(...args);
            queue.push(format(level, args));
            if (timer === undefined) {
                timer = setTimeout(flush, FLUSH_MS);
            }
        };
    }
    window.addEventListener('error', (e) => {
        queue.push(format('uncaught', [e.message]));
        flush();
    });
    window.addEventListener('unhandledrejection', (e) => {
        queue.push(format('unhandledrejection', [String(e.reason)]));
        flush();
    });
}
