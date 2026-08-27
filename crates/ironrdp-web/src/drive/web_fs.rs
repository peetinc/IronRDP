//! [`WebFsDrive`]: [`super::fs::DriveFs`] over the browser File System Access API — the
//! [`FileSystemDirectoryHandle`] tree a user grants via `window.showDirectoryPicker()`.
//!
//! Every [`super::fs::DriveFs`] method interprets an already-[`super::state::normalize_path`]d
//! path by walking [`FileSystemDirectoryHandle::get_directory_handle`] one component at a time
//! from `root`; the final component is resolved as either a file or a directory handle depending
//! on the operation. Three FSAA quirks shape this implementation, all called out where they bite:
//!
//! * **No rename primitive.** [`DriveFs::rename`] copies a file's bytes to the destination and
//!   deletes the source; renaming a directory returns [`FsError::AccessDenied`] (Explorer surfaces
//!   this as an error — acceptable per the plan this task implements).
//! * **Write-then-read isn't visible until close.** A [`web_sys::FileSystemWritableFileStream`] is
//!   swap-file-backed: bytes written through it are only visible to [`FileSystemFileHandle::get_file`]
//!   (and thus [`DriveFs::read`], [`DriveFs::stat`], [`DriveFs::list`]) once
//!   [`web_sys::FileSystemWritableFileStream::close`] resolves. A client that reads a handle it
//!   still has open for write will see the pre-write content.
//! * **`getFileHandle` on a directory name throws `TypeMismatchError`, not `NotFoundError`.** This
//!   is how [`stat`](DriveFs::stat) and [`rename`](DriveFs::rename) distinguish "doesn't exist"
//!   from "exists, but as the other kind of entry" without a separate directory probe on every
//!   call.
//!
//! Sizes and timestamps cross the `f64` boundary the File API represents them with (see
//! [`file_size_to_u64`] and [`offset_to_f64`] for the precision caveat, which is theoretical, not
//! practical, at any file size a browser can hold today).

use core::cell::{Cell, RefCell};
use std::collections::HashMap;

use futures_util::future::LocalBoxFuture;
use js_sys::{Array, IteratorNext, Reflect, Uint8Array};
use tracing::warn;
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    File, FileSystemCreateWritableOptions, FileSystemDirectoryHandle, FileSystemFileHandle,
    FileSystemGetDirectoryOptions, FileSystemGetFileOptions, FileSystemRemoveOptions, FileSystemWritableFileStream,
};

use super::fs::{DriveFs, FsEntry, FsError};
use super::state::normalize_path;

/// One outstanding [`WebFsDrive::open_file`] handle: the resolved file handle, plus — for a
/// write-opened file — the writable stream held open across `write`/`close` calls (see this
/// module's doc comment on why reads through `file_handle` won't observe unclosed writes).
struct OpenWebFile {
    file_handle: FileSystemFileHandle,
    writable: Option<FileSystemWritableFileStream>,
    /// The normalized path this handle was opened against. Not read back today; kept for parity
    /// with `state::OpenEntry::path` and because a `tracing` breadcrumb on an FSAA error is far
    /// more useful with the path attached than a bare numeric handle.
    #[expect(dead_code, reason = "debugging aid — see doc comment; not read back today")]
    path: String,
}

/// [`DriveFs`] implementation backed by a browser [`FileSystemDirectoryHandle`] tree.
pub(crate) struct WebFsDrive {
    root: FileSystemDirectoryHandle,
    handles: RefCell<HashMap<u32, OpenWebFile>>,
    /// Monotonic handle id counter — same wrap-past-`u32::MAX`-back-to-1, never-hand-out-0 scheme
    /// as `MockFs::allocate_handle` / `DriveState::allocate_file_id`.
    next_handle: Cell<u32>,
}

impl WebFsDrive {
    pub(crate) fn new(root: FileSystemDirectoryHandle) -> Self {
        Self {
            root,
            handles: RefCell::new(HashMap::new()),
            next_handle: Cell::new(1),
        }
    }

    fn allocate_handle(&self) -> u32 {
        let handle = self.next_handle.get();
        let mut next = handle.wrapping_add(1);
        if next == 0 {
            next = 1;
        }
        self.next_handle.set(next);
        handle
    }

    /// Walks `components` from the share root, resolving each as an *existing* directory —
    /// never creating one. Used for a path's parent (every operation but `list`) and, for
    /// `list`, the listed path itself.
    async fn resolve_dir(&self, components: &[String]) -> Result<FileSystemDirectoryHandle, FsError> {
        let mut current = self.root.clone();
        for name in components {
            let value = JsFuture::from(current.get_directory_handle(name))
                .await
                .map_err(fs_error_from_js)?;
            current = value.unchecked_into();
        }
        Ok(current)
    }
}

impl DriveFs for WebFsDrive {
    fn stat(&self, path: &str) -> LocalBoxFuture<'_, Result<FsEntry, FsError>> {
        let path = path.to_owned();
        Box::pin(async move {
            let components = normalize_path(&path)?;
            let Some((name, parent_components)) = components.split_last() else {
                // The share root itself: always a directory.
                return Ok(dir_entry(String::new()));
            };
            let parent = self.resolve_dir(parent_components).await?;

            match JsFuture::from(parent.get_file_handle(name)).await {
                Ok(value) => {
                    let file_handle: FileSystemFileHandle = value.unchecked_into();
                    let file = get_file(&file_handle).await?;
                    Ok(file_entry(name.clone(), &file))
                }
                // `getFileHandle` on a name that exists but is a directory throws
                // `TypeMismatchError`, not `NotFoundError` — see this module's doc comment.
                Err(err) if is_type_mismatch(&err) => {
                    JsFuture::from(parent.get_directory_handle(name))
                        .await
                        .map_err(fs_error_from_js)?;
                    Ok(dir_entry(name.clone()))
                }
                Err(err) => Err(fs_error_from_js(err)),
            }
        })
    }

    fn list(&self, path: &str) -> LocalBoxFuture<'_, Result<Vec<FsEntry>, FsError>> {
        let path = path.to_owned();
        Box::pin(async move {
            let components = normalize_path(&path)?;
            let dir = self.resolve_dir(&components).await?;
            let iterator = dir.entries();

            let mut out = Vec::new();
            loop {
                let next_promise = iterator.next().map_err(fs_error_from_js)?;
                let next_value = JsFuture::from(next_promise).await.map_err(fs_error_from_js)?;
                let next: IteratorNext = next_value.unchecked_into();
                if next.done() {
                    break;
                }

                // `entries()` yields `[name, handle]` pairs.
                let pair: Array = next.value().unchecked_into();
                let name = pair.get(0).as_string().unwrap_or_default();
                let handle_value = pair.get(1);

                if let Some(file_handle) = handle_value.dyn_ref::<FileSystemFileHandle>() {
                    let file = get_file(file_handle).await?;
                    out.push(file_entry(name, &file));
                } else {
                    out.push(dir_entry(name));
                }
            }
            Ok(out)
        })
    }

    fn open_file(
        &self,
        path: &str,
        write: bool,
        create: bool,
        truncate: bool,
    ) -> LocalBoxFuture<'_, Result<u32, FsError>> {
        let path = path.to_owned();
        Box::pin(async move {
            let components = normalize_path(&path)?;
            let (name, parent_components) = components.split_last().ok_or(FsError::AccessDenied)?;
            let parent = self.resolve_dir(parent_components).await?;

            let get_options = FileSystemGetFileOptions::new();
            get_options.set_create(create);
            let file_handle: FileSystemFileHandle =
                JsFuture::from(parent.get_file_handle_with_options(name, &get_options))
                    .await
                    .map_err(fs_error_from_js)?
                    .unchecked_into();

            let writable = if write {
                // `keep_existing_data: true` per the plan this task implements: RDPDR writes land
                // at arbitrary offsets, so the writable must start from the file's current
                // content rather than an empty one. `truncate` (when requested) is then applied
                // explicitly below instead of by the writable's own creation-time truncation.
                let writable_options = FileSystemCreateWritableOptions::new();
                writable_options.set_keep_existing_data(true);
                let writable: FileSystemWritableFileStream =
                    JsFuture::from(file_handle.create_writable_with_options(&writable_options))
                        .await
                        .map_err(fs_error_from_js)?
                        .unchecked_into();

                if truncate {
                    let truncated = match writable.truncate_with_f64(0.0) {
                        Ok(promise) => JsFuture::from(promise).await.map_err(fs_error_from_js),
                        Err(err) => Err(fs_error_from_js(err)),
                    };
                    if let Err(err) = truncated {
                        // The writable was already created and never gets stored in `handles`
                        // now — abort it so the browser releases its file lock deterministically
                        // instead of waiting on GC to drop an unclosed writable.
                        abort_best_effort(&writable).await;
                        return Err(err);
                    }
                }
                Some(writable)
            } else {
                None
            };

            let handle = self.allocate_handle();
            self.handles.borrow_mut().insert(
                handle,
                OpenWebFile {
                    file_handle,
                    writable,
                    path,
                },
            );
            Ok(handle)
        })
    }

    fn read(&self, handle: u32, offset: u64, len: u32) -> LocalBoxFuture<'_, Result<Vec<u8>, FsError>> {
        Box::pin(async move {
            let file_handle = {
                let handles = self.handles.borrow();
                handles.get(&handle).ok_or(FsError::NotFound)?.file_handle.clone()
            };
            // See this module's doc comment: if `handle` is also open for write, this observes
            // the content as of the last `close()`, not any not-yet-closed writes.
            let file = get_file(&file_handle).await?;

            let start = offset_to_f64(offset);
            let end = start + f64::from(len);
            let blob = file.slice_with_f64_and_f64(start, end).map_err(fs_error_from_js)?;
            let array_buffer = JsFuture::from(blob.array_buffer()).await.map_err(fs_error_from_js)?;

            let mut bytes = Uint8Array::new(&array_buffer).to_vec();
            // `Blob::slice` already clamps to the file's actual remaining length, so this is
            // never a truncation in practice — belt-and-suspenders against the contract.
            let requested = usize::try_from(len).map_err(|_| FsError::Other("len overflow".to_owned()))?;
            bytes.truncate(requested);
            Ok(bytes)
        })
    }

    fn write(&self, handle: u32, offset: u64, data: Vec<u8>) -> LocalBoxFuture<'_, Result<u32, FsError>> {
        Box::pin(async move {
            let writable = {
                let handles = self.handles.borrow();
                let entry = handles.get(&handle).ok_or(FsError::NotFound)?;
                entry.writable.clone().ok_or(FsError::AccessDenied)?
            };

            let seek_promise = writable
                .seek_with_f64(offset_to_f64(offset))
                .map_err(fs_error_from_js)?;
            JsFuture::from(seek_promise).await.map_err(fs_error_from_js)?;

            let write_promise = writable.write_with_u8_array(&data).map_err(fs_error_from_js)?;
            JsFuture::from(write_promise).await.map_err(fs_error_from_js)?;

            u32::try_from(data.len()).map_err(|_| FsError::Other("write too large".to_owned()))
        })
    }

    fn close(&self, handle: u32) -> LocalBoxFuture<'_, Result<(), FsError>> {
        Box::pin(async move {
            let entry = self.handles.borrow_mut().remove(&handle).ok_or(FsError::NotFound)?;
            if let Some(writable) = entry.writable {
                // This is what commits a write-opened file's content — see this module's doc
                // comment on `FileSystemWritableFileStream`'s swap-file semantics.
                JsFuture::from(writable.close()).await.map_err(fs_error_from_js)?;
            }
            Ok(())
        })
    }

    fn rename(&self, from: &str, to: &str) -> LocalBoxFuture<'_, Result<(), FsError>> {
        let (from, to) = (from.to_owned(), to.to_owned());
        Box::pin(async move {
            let from_components = normalize_path(&from)?;
            let to_components = normalize_path(&to)?;
            let (from_name, from_parent_components) = from_components.split_last().ok_or(FsError::AccessDenied)?;
            let (to_name, to_parent_components) = to_components.split_last().ok_or(FsError::AccessDenied)?;

            let from_parent = self.resolve_dir(from_parent_components).await?;

            match JsFuture::from(from_parent.get_file_handle(from_name)).await {
                Ok(value) => {
                    let file_handle: FileSystemFileHandle = value.unchecked_into();
                    let file = get_file(&file_handle).await?;
                    let array_buffer = JsFuture::from(file.array_buffer()).await.map_err(fs_error_from_js)?;
                    let bytes = Uint8Array::new(&array_buffer).to_vec();

                    // Copy to the destination first, delete the source only once that succeeds —
                    // a failure partway through then leaves the original intact rather than
                    // losing data.
                    let to_parent = self.resolve_dir(to_parent_components).await?;
                    let get_options = FileSystemGetFileOptions::new();
                    get_options.set_create(true);
                    let new_file_handle: FileSystemFileHandle =
                        JsFuture::from(to_parent.get_file_handle_with_options(to_name, &get_options))
                            .await
                            .map_err(fs_error_from_js)?
                            .unchecked_into();

                    let writable_options = FileSystemCreateWritableOptions::new();
                    writable_options.set_keep_existing_data(false);
                    let writable: FileSystemWritableFileStream =
                        JsFuture::from(new_file_handle.create_writable_with_options(&writable_options))
                            .await
                            .map_err(fs_error_from_js)?
                            .unchecked_into();

                    let written = match writable.write_with_u8_array(&bytes) {
                        Ok(promise) => JsFuture::from(promise).await.map_err(fs_error_from_js),
                        Err(err) => Err(fs_error_from_js(err)),
                    };
                    if let Err(err) = written {
                        // Same reasoning as `open_file`'s truncate-failure path: this writable
                        // is a local the caller of `rename` never gets a handle to, so it must be
                        // aborted here or it stays locked until GC.
                        abort_best_effort(&writable).await;
                        return Err(err);
                    }
                    JsFuture::from(writable.close()).await.map_err(fs_error_from_js)?;

                    JsFuture::from(from_parent.remove_entry(from_name))
                        .await
                        .map_err(fs_error_from_js)?;
                    Ok(())
                }
                // `from` exists but is a directory: FSAA has no directory-rename primitive (see
                // this module's doc comment) — Explorer surfaces this as an error, which is
                // accepted as a documented limitation rather than worked around.
                Err(err) if is_type_mismatch(&err) => Err(FsError::AccessDenied),
                Err(err) => Err(fs_error_from_js(err)),
            }
        })
    }

    fn delete(&self, path: &str) -> LocalBoxFuture<'_, Result<(), FsError>> {
        let path = path.to_owned();
        Box::pin(async move {
            let components = normalize_path(&path)?;
            let (name, parent_components) = components.split_last().ok_or(FsError::AccessDenied)?;
            let parent = self.resolve_dir(parent_components).await?;

            let options = FileSystemRemoveOptions::new();
            options.set_recursive(false);
            JsFuture::from(parent.remove_entry_with_options(name, &options))
                .await
                .map_err(fs_error_from_js)?;
            Ok(())
        })
    }

    fn mkdir(&self, path: &str) -> LocalBoxFuture<'_, Result<(), FsError>> {
        let path = path.to_owned();
        Box::pin(async move {
            let components = normalize_path(&path)?;
            let (name, parent_components) = components.split_last().ok_or(FsError::AccessDenied)?;
            let parent = self.resolve_dir(parent_components).await?;

            let options = FileSystemGetDirectoryOptions::new();
            options.set_create(true);
            JsFuture::from(parent.get_directory_handle_with_options(name, &options))
                .await
                .map_err(fs_error_from_js)?;
            Ok(())
        })
    }
}

fn dir_entry(name: String) -> FsEntry {
    FsEntry {
        name,
        is_dir: true,
        size: 0,
        last_modified_ms: 0.0,
    }
}

fn file_entry(name: String, file: &File) -> FsEntry {
    FsEntry {
        name,
        is_dir: false,
        size: file_size_to_u64(file.size()),
        last_modified_ms: file.last_modified(),
    }
}

async fn get_file(handle: &FileSystemFileHandle) -> Result<File, FsError> {
    let value = JsFuture::from(handle.get_file()).await.map_err(fs_error_from_js)?;
    Ok(value.unchecked_into())
}

/// Best-effort cleanup for a [`FileSystemWritableFileStream`] this module created but is about to
/// drop without ever reaching `close()` (a `truncate`/`write` step failing right after
/// `createWritable` succeeded). Aborting releases the browser's file lock deterministically
/// instead of leaving an unclosed writable for garbage collection to eventually drop — best
/// effort because there is nothing more useful to do if the abort itself fails; the error this is
/// cleaning up after is always what the caller propagates, not this one.
async fn abort_best_effort(writable: &FileSystemWritableFileStream) {
    if let Err(err) = JsFuture::from(writable.abort()).await {
        warn!(
            ?err,
            "Failed to abort a FileSystemWritableFileStream after an earlier error"
        );
    }
}

/// Converts a [`web_sys::Blob::size`] / [`File::last_modified`]-adjacent `f64` (the File API
/// represents sizes as IEEE-754 doubles, never integers) into the `u64` [`FsEntry::size`]
/// expects. `f64 as u64` is a saturating cast (stable since Rust 1.45): NaN and negative inputs —
/// never produced by a real browser, but not ruled out by the type — become `0`, and inputs at or
/// beyond `u64::MAX` saturate to it. `f64` only represents integers *exactly* up to 2^53
/// (~9 PiB) — vastly larger than any file the File System Access API could realistically hold, so
/// this is a theoretical caveat rather than a practical one.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "f64 -> u64 is a saturating cast (stable since Rust 1.45); see doc comment for the precision caveat"
)]
fn file_size_to_u64(size: f64) -> u64 {
    size as u64
}

/// Converts an RDPDR `u64` byte offset into the `f64` [`web_sys::Blob::slice_with_f64_and_f64`] /
/// [`web_sys::FileSystemWritableFileStream::seek_with_f64`] expect. Same precision caveat as
/// [`file_size_to_u64`], from the other direction: exact up to 2^53 (~9 PiB), which no RDPDR
/// drive-redirection offset approaches in practice.
#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "u64 -> f64 is the FSAA wire type for offsets; see doc comment for the precision caveat"
)]
fn offset_to_f64(offset: u64) -> f64 {
    offset as f64
}

/// Reads the `name` property off a thrown JS value (a `DOMException` for every FSAA failure this
/// module maps), if it has one.
fn js_error_name(err: &JsValue) -> Option<String> {
    Reflect::get(err, &JsValue::from_str("name")).ok()?.as_string()
}

/// `true` for the one JS exception name this module's callers need to distinguish from a bare
/// [`FsError::Other`]: `getFileHandle` throws `TypeMismatchError` (not `NotFoundError`) when the
/// name it was given exists but as a directory — see this module's doc comment.
fn is_type_mismatch(err: &JsValue) -> bool {
    js_error_name(err).as_deref() == Some("TypeMismatchError")
}

/// Maps a thrown JS value from an FSAA call into [`FsError`], per the plan this task implements:
/// `NotFoundError` -> [`FsError::NotFound`]; `NotAllowedError` / `NoModificationAllowedError` ->
/// [`FsError::AccessDenied`]; anything else -> [`FsError::Other`] (including `TypeMismatchError`
/// at every call site that doesn't special-case it — [`is_type_mismatch`] is checked first where
/// that distinction matters).
fn fs_error_from_js(err: JsValue) -> FsError {
    match js_error_name(&err).as_deref() {
        Some("NotFoundError") => FsError::NotFound,
        Some("NotAllowedError") | Some("NoModificationAllowedError") => FsError::AccessDenied,
        _ => FsError::Other(js_error_message(&err)),
    }
}

/// Best-effort human-readable description of a thrown JS value, for [`FsError::Other`].
fn js_error_message(err: &JsValue) -> String {
    Reflect::get(err, &JsValue::from_str("message"))
        .ok()
        .and_then(|value| value.as_string())
        .unwrap_or_else(|| format!("{err:?}"))
}
