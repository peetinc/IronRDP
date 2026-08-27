//! IRP-scoped state for the drive-redirection RDPDR backend: the open
//! RDPDR `file_id` table (mirroring [`crate::printer::WasmPrinterBackend`]'s
//! per-file-handle map) plus directory-listing cursors for
//! `IRP_MJ_DIRECTORY_CONTROL` (`QueryDirectory`), and the path
//! normalization every incoming drive IRP path goes through before it
//! reaches a [`super::fs::DriveFs`] implementation.

use std::collections::HashMap;

use super::fs::{FsEntry, FsError};

/// Book-keeping for one open RDPDR drive `file_id`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OpenEntry {
    /// Normalized, backslash-joined path this handle was opened against.
    pub(crate) path: String,
    /// The underlying [`super::fs::DriveFs`] handle, if this entry has one.
    /// Directories are listed via `DriveFs::list` and cached in
    /// `dir_listing` instead, so a directory entry is never given an
    /// `fs_handle`.
    pub(crate) fs_handle: Option<u32>,
    pub(crate) is_dir: bool,
    /// Cached `DriveFs::list` result backing `IRP_MJ_DIRECTORY_CONTROL` /
    /// `QueryDirectory`, populated on the first query for this handle.
    pub(crate) dir_listing: Option<Vec<FsEntry>>,
    /// Index into `dir_listing` of the next entry `next_dir_entry` returns.
    pub(crate) dir_cursor: usize,
}

/// Maps RDPDR `file_id` -> [`OpenEntry`] for the lifetime of a drive
/// redirection session, and allocates new `file_id`s.
#[derive(Debug, Default)]
pub(crate) struct DriveState {
    entries: HashMap<u32, OpenEntry>,
    /// Monotonic file id counter, same scheme as
    /// `WasmPrinterBackend::allocate_file_id` (`printer.rs:178`): starts at
    /// 1, wraps past `u32::MAX` back to 1 (0 is never handed out). Ids are
    /// never reallocated after `close` even though the slot is freed — the
    /// server only requires uniqueness among *currently open* handles, and
    /// reusing a just-closed number needlessly risks a stale in-flight IRP
    /// racing a fresh open of the same id.
    next_file_id: u32,
}

impl DriveState {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            next_file_id: 1,
        }
    }

    /// Opens a new entry, allocating and returning its `file_id`.
    pub(crate) fn open(&mut self, path: impl Into<String>, fs_handle: Option<u32>, is_dir: bool) -> u32 {
        let file_id = self.allocate_file_id();
        self.entries.insert(
            file_id,
            OpenEntry {
                path: path.into(),
                fs_handle,
                is_dir,
                dir_listing: None,
                dir_cursor: 0,
            },
        );
        file_id
    }

    pub(crate) fn get(&self, file_id: u32) -> Option<&OpenEntry> {
        self.entries.get(&file_id)
    }

    pub(crate) fn get_mut(&mut self, file_id: u32) -> Option<&mut OpenEntry> {
        self.entries.get_mut(&file_id)
    }

    /// Removes and returns the entry for `file_id`, if it was open. The
    /// `file_id` itself is never handed out again — see the `next_file_id`
    /// doc comment.
    pub(crate) fn close(&mut self, file_id: u32) -> Option<OpenEntry> {
        self.entries.remove(&file_id)
    }

    /// Caches a directory listing against an open entry and resets its
    /// cursor, so the next `next_dir_entry` call starts from the beginning.
    /// Returns `false` if `file_id` isn't open.
    pub(crate) fn set_dir_listing(&mut self, file_id: u32, listing: Vec<FsEntry>) -> bool {
        let Some(entry) = self.entries.get_mut(&file_id) else {
            return false;
        };
        entry.dir_listing = Some(listing);
        entry.dir_cursor = 0;
        true
    }

    /// Pops the next cached directory entry for `file_id`, advancing the
    /// cursor. Returns `None` once the listing is exhausted, or if there is
    /// no cached listing / open entry for `file_id`.
    pub(crate) fn next_dir_entry(&mut self, file_id: u32) -> Option<FsEntry> {
        let entry = self.entries.get_mut(&file_id)?;
        let listing = entry.dir_listing.as_ref()?;
        let item = listing.get(entry.dir_cursor)?.clone();
        entry.dir_cursor += 1;
        Some(item)
    }

    fn allocate_file_id(&mut self) -> u32 {
        let id = self.next_file_id;
        self.next_file_id = self.next_file_id.wrapping_add(1);
        if self.next_file_id == 0 {
            self.next_file_id = 1;
        }
        id
    }
}

/// Normalizes a drive-IRP path (backslash-separated, rooted at the share,
/// e.g. `"\\dir\\f.txt"`) into path components, rejecting attempts to escape
/// the share root via `..`.
///
/// Forward slashes are accepted too (some clients send them), empty
/// components from leading/trailing/doubled separators and `.` segments are
/// dropped. An input that normalizes to nothing denotes the share root
/// itself — an empty component vector, not an error.
pub(crate) fn normalize_path(path: &str) -> Result<Vec<String>, FsError> {
    let mut components = Vec::new();
    for component in path.split(['\\', '/']) {
        match component {
            "" | "." => continue,
            ".." => return Err(FsError::AccessDenied),
            other => components.push(other.to_owned()),
        }
    }
    Ok(components)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_path_accepts_backslash_separated_path() {
        assert_eq!(
            normalize_path("\\dir\\f.txt").unwrap(),
            vec!["dir".to_string(), "f.txt".to_string()]
        );
    }

    #[test]
    fn normalize_path_rejects_parent_traversal() {
        assert_eq!(normalize_path("..\\x"), Err(FsError::AccessDenied));
    }

    #[test]
    fn normalize_path_rejects_parent_traversal_mid_path() {
        assert_eq!(normalize_path("\\dir\\..\\x"), Err(FsError::AccessDenied));
    }

    #[test]
    fn normalize_path_root_normalizes_to_empty_components() {
        assert_eq!(normalize_path("\\").unwrap(), Vec::<String>::new());
        assert_eq!(normalize_path("").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn file_id_allocates_from_one_and_is_not_reused_after_close() {
        let mut state = DriveState::new();

        let first = state.open("\\a.txt", Some(1), false);
        assert_eq!(first, 1, "first allocated file_id must be 1");
        assert!(state.get(first).is_some());

        assert!(state.close(first).is_some());
        assert!(state.get(first).is_none(), "closed entry must no longer be looked up");

        let second = state.open("\\b.txt", Some(2), false);
        assert_eq!(
            second, 2,
            "closing file_id 1 must not make it available for reallocation"
        );
    }

    #[test]
    fn close_of_unknown_file_id_returns_none() {
        let mut state = DriveState::new();
        assert!(state.close(42).is_none());
    }

    #[test]
    fn dir_cursor_pops_three_entries_then_is_exhausted() {
        let mut state = DriveState::new();
        let file_id = state.open("\\dir", None, true);

        let listing = vec![
            FsEntry {
                name: "a".to_string(),
                is_dir: false,
                size: 1,
                last_modified_ms: 0.0,
            },
            FsEntry {
                name: "b".to_string(),
                is_dir: false,
                size: 2,
                last_modified_ms: 0.0,
            },
            FsEntry {
                name: "c".to_string(),
                is_dir: true,
                size: 0,
                last_modified_ms: 0.0,
            },
        ];
        assert!(state.set_dir_listing(file_id, listing.clone()));

        assert_eq!(state.next_dir_entry(file_id), Some(listing[0].clone()));
        assert_eq!(state.next_dir_entry(file_id), Some(listing[1].clone()));
        assert_eq!(state.next_dir_entry(file_id), Some(listing[2].clone()));
        assert_eq!(
            state.next_dir_entry(file_id),
            None,
            "listing must be exhausted after 3 pops"
        );
        assert_eq!(state.next_dir_entry(file_id), None, "exhausted cursor stays exhausted");
    }

    #[test]
    fn dir_cursor_on_entry_without_listing_yields_none() {
        let mut state = DriveState::new();
        let file_id = state.open("\\dir", None, true);
        assert_eq!(state.next_dir_entry(file_id), None);
    }
}
