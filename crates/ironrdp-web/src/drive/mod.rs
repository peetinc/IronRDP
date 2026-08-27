//! Drive redirection (RDPDR) groundwork.
//!
//! This module is pure Rust with no wasm dependency:
//!
//! * [`fs::DriveFs`] is the async filesystem abstraction a drive-redirection
//!   RDPDR backend drives — `stat`/`list`/`open_file`/`read`/`write`/
//!   `close`/`rename`/`delete`/`mkdir`. A later task implements it against
//!   the browser File System Access API; here it's exercised by
//!   [`fs::MockFs`], an in-memory test double.
//! * [`state::DriveState`] tracks per-RDPDR-`file_id` state (the open path,
//!   the underlying `DriveFs` handle, and — for directories —  a cached
//!   listing plus read cursor for `IRP_MJ_DIRECTORY_CONTROL`), and owns the
//!   path-normalization logic that turns a raw RDPDR path into share-root
//!   relative components.
//!
//! Neither type is wired into a `RdpdrBackend` yet — that's `WasmDriveBackend`,
//! built on top of these in a later task. Everything here is exercised
//! natively (`cargo test -p ironrdp-web --lib`); no wasm APIs are involved.
#![allow(dead_code)] // Groundwork: not wired into a RdpdrBackend yet (later task).

pub(crate) mod fs;
pub(crate) mod state;
