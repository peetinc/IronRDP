//! Drive redirection (RDPDR) groundwork plus its `RdpdrBackend` implementation.
//!
//! * [`fs::DriveFs`] is the async filesystem abstraction [`backend::WasmDriveBackend`]
//!   drives — `stat`/`list`/`open_file`/`read`/`write`/`close`/`rename`/`delete`/`mkdir`.
//!   [`web_fs::WebFsDrive`] implements it against the browser File System Access API;
//!   [`fs::MockFs`] is an in-memory test double for exercising the rest of this module
//!   without a browser.
//! * [`state::DriveState`] tracks per-RDPDR-`file_id` state (the open path,
//!   the underlying `DriveFs` handle, and — for directories —  a cached
//!   listing plus read cursor for `IRP_MJ_DIRECTORY_CONTROL`), and owns the
//!   path-normalization logic that turns a raw RDPDR path into share-root
//!   relative components.
//! * [`backend::WasmDriveBackend`] answers server drive IRPs by driving `DriveFs`
//!   asynchronously, bookkeeping open handles via `DriveState`. Wired into a live session by
//!   `crate::session` — see [`backend::wasm_drive_pair`]'s doc comment and the `driveShare`
//!   extension parsed in `crate::session::SessionBuilder::extension`.
//! * [`composite::WasmCompositeBackend`] lets [`backend::WasmDriveBackend`] and
//!   [`crate::printer::WasmPrinterBackend`] coexist on the single backend slot `Rdpdr` accepts —
//!   wired in at `crate::session`'s attach site whenever either is configured for a session.
//!
//! `fs`, `state`, and `composite` are pure Rust with no wasm dependency and are exercised
//! natively (`cargo test -p ironrdp-web --lib drive::`); `backend` itself is also fully testable
//! natively via its injected-spawner constructor, even though its public `wasm_drive_pair`
//! factory calls into `wasm_bindgen_futures`. `web_fs` calls real browser APIs it has no
//! way to exercise outside one, so it has no unit tests of its own — compiling clean
//! (`cargo check --target wasm32-unknown-unknown`) is its gate; it happens to also compile
//! natively (wasm-bindgen's non-wasm32 stubs type-check even though they'd panic if invoked),
//! which is why it isn't `#[cfg(target_arch = "wasm32")]`-gated.

pub(crate) mod backend;
pub(crate) mod composite;
pub(crate) mod fs;
pub(crate) mod state;
pub(crate) mod web_fs;
