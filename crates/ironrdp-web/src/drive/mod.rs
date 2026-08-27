//! Drive redirection (RDPDR) groundwork plus its `RdpdrBackend` implementation.
//!
//! * [`fs::DriveFs`] is the async filesystem abstraction [`backend::WasmDriveBackend`]
//!   drives — `stat`/`list`/`open_file`/`read`/`write`/`close`/`rename`/`delete`/`mkdir`.
//!   A later task implements it against the browser File System Access API; here it's
//!   exercised by [`fs::MockFs`], an in-memory test double.
//! * [`state::DriveState`] tracks per-RDPDR-`file_id` state (the open path,
//!   the underlying `DriveFs` handle, and — for directories —  a cached
//!   listing plus read cursor for `IRP_MJ_DIRECTORY_CONTROL`), and owns the
//!   path-normalization logic that turns a raw RDPDR path into share-root
//!   relative components.
//! * [`backend::WasmDriveBackend`] answers server drive IRPs by driving `DriveFs`
//!   asynchronously, bookkeeping open handles via `DriveState`. Not yet wired into a live
//!   session — `backend::wasm_drive_pair`'s caller (the session builder) is a later task.
//!
//! `fs` and `state` are pure Rust with no wasm dependency and are exercised natively
//! (`cargo test -p ironrdp-web --lib drive::`); `backend` itself is also fully testable
//! natively via its injected-spawner constructor, even though its public `wasm_drive_pair`
//! factory calls into `wasm_bindgen_futures`.
#![allow(dead_code)] // Groundwork: `wasm_drive_pair` not called from a live session yet (later task).

pub(crate) mod backend;
pub(crate) mod fs;
pub(crate) mod state;
