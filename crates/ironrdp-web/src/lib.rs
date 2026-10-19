#![cfg_attr(doc, doc = include_str!("../README.md"))]
#![cfg_attr(
    doc,
    doc(
        html_logo_url = "https://cdnweb.devolutions.net/images/projects/devolutions/logos/devolutions-icon-shadow.svg"
    )
)]
#![allow(clippy::new_without_default)] // Default trait can’t be used by wasm consumer anyway.

// Silence the unused_crate_dependencies lint.
// These crates are added just to enable additional WASM features.
extern crate chrono as _;
extern crate getrandom as _;
extern crate getrandom2 as _;
extern crate getrandom4 as _;
extern crate time as _;
// Pulled in only to turn on `rustls-pki-types`' `web` feature: without it, the
// `UnixTime::now()` rustls calls during a handshake resolves to `SystemTime::now()`,
// which panics on `wasm32-unknown-unknown`.
extern crate rustls_pki_types as _;
// Ditto for `ring`'s `wasm32_unknown_unknown_js`, which is what gives its `SystemRandom`
// an implementation on this target — see the Cargo.toml comment.
extern crate ring as _;

mod audio;
mod bench;
mod canvas;
mod clipboard;
mod drive;
mod error;
mod gpu;
mod h264;
mod image;
mod input;
mod network_client;
mod printer;
mod rdp_file;
mod session;
mod tls;

/// The browser's wall clock in whole milliseconds; times EGFX QoE frame acknowledgements.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Date.now() is a non-negative count of milliseconds, well inside u64"
)]
fn clock_ms() -> u64 {
    js_sys::Date::now() as u64
}

mod wasm_bridge {
    use tracing::debug;

    struct Api;

    impl iron_remote_desktop::RemoteDesktopApi for Api {
        type Session = crate::session::Session;
        type SessionBuilder = crate::session::SessionBuilder;
        type SessionTerminationInfo = crate::session::SessionTerminationInfo;
        type DeviceEvent = crate::input::DeviceEvent;
        type InputTransaction = crate::input::InputTransaction;
        type ClipboardData = crate::clipboard::ClipboardData;
        type ClipboardItem = crate::clipboard::ClipboardItem;
        type Error = crate::error::IronError;

        fn post_setup() {
            debug!("IronRDP is ready");
        }
    }

    iron_remote_desktop::make_bridge!(Api);
}
