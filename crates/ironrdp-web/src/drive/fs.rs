//! [`DriveFs`]: the async filesystem abstraction a drive-redirection RDPDR
//! backend drives. Every method takes an already-`normalize_path`d-style
//! path (backslash-separated, rooted at the share — see
//! [`super::state::normalize_path`]); an implementation just needs to
//! interpret it relative to its own root.
//!
//! [`MockFs`] is an in-memory, test-only implementation used to exercise
//! [`super::state::DriveState`] — and, in a later task, `WasmDriveBackend` —
//! without touching any browser API.

use futures_util::future::LocalBoxFuture;

/// A single filesystem entry, as returned by [`DriveFs::stat`] and
/// [`DriveFs::list`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FsEntry {
    pub(crate) name: String,
    pub(crate) is_dir: bool,
    pub(crate) size: u64,
    pub(crate) last_modified_ms: f64,
}

/// Failure modes surfaced by a [`DriveFs`] implementation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FsError {
    NotFound,
    AccessDenied,
    Other(String),
}

/// Async filesystem operations a drive-redirection RDPDR backend drives.
///
/// Object-safe (`LocalBoxFuture`, no `Send` bound required) so it can back a
/// `Box<dyn DriveFs>` on the wasm event loop, which is single-threaded —
/// unlike [`ironrdp::rdpdr::backend::RdpdrBackend`] itself, which the SVC
/// processor requires to be `Send`. `WasmDriveBackend` (a later task) will
/// bridge the two: `Send` on the outside, `Box<dyn DriveFs>` driven from a
/// single-threaded task on the inside.
pub(crate) trait DriveFs {
    fn stat(&self, path: &str) -> LocalBoxFuture<'_, Result<FsEntry, FsError>>;
    fn list(&self, path: &str) -> LocalBoxFuture<'_, Result<Vec<FsEntry>, FsError>>;
    fn open_file(
        &self,
        path: &str,
        write: bool,
        create: bool,
        truncate: bool,
    ) -> LocalBoxFuture<'_, Result<u32 /* fs handle */, FsError>>;
    fn read(&self, handle: u32, offset: u64, len: u32) -> LocalBoxFuture<'_, Result<Vec<u8>, FsError>>;
    fn write(&self, handle: u32, offset: u64, data: Vec<u8>) -> LocalBoxFuture<'_, Result<u32, FsError>>;
    fn close(&self, handle: u32) -> LocalBoxFuture<'_, Result<(), FsError>>;
    /// Truncates or zero-extends the file behind a write-opened `handle` to exactly `size`
    /// bytes. Backs `FileEndOfFileInformation`/`FileAllocationInformation` SetInformation IRPs —
    /// Windows' copy engine sets the destination's size up front before writing any data, and
    /// treats a failure here as fatal to the whole copy.
    fn set_len(&self, handle: u32, size: u64) -> LocalBoxFuture<'_, Result<(), FsError>>;
    fn rename(&self, from: &str, to: &str) -> LocalBoxFuture<'_, Result<(), FsError>>;
    fn delete(&self, path: &str) -> LocalBoxFuture<'_, Result<(), FsError>>;
    fn mkdir(&self, path: &str) -> LocalBoxFuture<'_, Result<(), FsError>>;
}

// Surfaced for this module's own tests today; a later task's `WasmDriveBackend`
// tests are the intended non-test-local consumer.
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use mock::MockFs;

#[cfg(test)]
mod mock {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, HashMap};

    use futures_util::future::LocalBoxFuture;

    use super::{DriveFs, FsEntry, FsError};
    use crate::drive::state::normalize_path;

    /// A node in the in-memory tree backing [`MockFs`].
    #[derive(Debug, Clone)]
    enum Node {
        File(Vec<u8>),
        Dir(BTreeMap<String, Node>),
    }

    impl Node {
        fn new_dir() -> Self {
            Node::Dir(BTreeMap::new())
        }
    }

    /// One outstanding `MockFs::open_file` handle.
    struct OpenHandle {
        components: Vec<String>,
        write: bool,
    }

    struct Inner {
        root: Node,
        handles: HashMap<u32, OpenHandle>,
        next_handle: u32,
    }

    /// In-memory [`DriveFs`] test double: a tree of [`Node`]s rooted at the
    /// share root, with its own open-file-handle table. That table is
    /// distinct from [`super::super::state::DriveState`]'s RDPDR `file_id`
    /// table — a real backend has both layers: the RDPDR `file_id` from
    /// `DriveState`, and (for files) the `DriveFs` handle it maps to.
    pub(crate) struct MockFs {
        inner: RefCell<Inner>,
    }

    impl MockFs {
        pub(crate) fn new() -> Self {
            Self {
                inner: RefCell::new(Inner {
                    root: Node::new_dir(),
                    handles: HashMap::new(),
                    next_handle: 1,
                }),
            }
        }

        /// Seeds a file at `path` (backslash-separated) with `contents`,
        /// creating parent directories as needed. Test fixture helper only.
        pub(crate) fn seed_file(&self, path: &str, contents: &[u8]) {
            let components = normalize_path(path).expect("test fixture path must normalize");
            let mut inner = self.inner.borrow_mut();
            let node = Self::make_path_mut(&mut inner.root, &components);
            *node = Node::File(contents.to_vec());
        }

        /// Seeds an empty directory at `path`. Test fixture helper only.
        pub(crate) fn seed_dir(&self, path: &str) {
            let components = normalize_path(path).expect("test fixture path must normalize");
            let mut inner = self.inner.borrow_mut();
            let _ = Self::make_path_mut(&mut inner.root, &components);
        }

        /// Walks from `root`, creating any missing directory components,
        /// and returns the (possibly freshly-created, always a dir until
        /// the caller overwrites it) node at `components`.
        fn make_path_mut<'a>(root: &'a mut Node, components: &[String]) -> &'a mut Node {
            let mut current = root;
            for component in components {
                let Node::Dir(children) = current else {
                    panic!("path component traverses through a file");
                };
                current = children.entry(component.clone()).or_insert_with(Node::new_dir);
            }
            current
        }

        fn find<'a>(root: &'a Node, components: &[String]) -> Option<&'a Node> {
            let mut current = root;
            for component in components {
                match current {
                    Node::Dir(children) => current = children.get(component)?,
                    Node::File(_) => return None,
                }
            }
            Some(current)
        }

        /// Splits `components` into its parent directory's children map and
        /// the final path segment's name. Errors if `components` is the
        /// share root itself (nothing to split) or the parent doesn't exist
        /// / isn't a directory.
        fn parent_children_mut<'a>(
            root: &'a mut Node,
            components: &'a [String],
        ) -> Result<(&'a mut BTreeMap<String, Node>, &'a str), FsError> {
            let (name, parent_components) = components.split_last().ok_or(FsError::AccessDenied)?;
            let mut current = root;
            for component in parent_components {
                match current {
                    Node::Dir(children) => current = children.get_mut(component).ok_or(FsError::NotFound)?,
                    Node::File(_) => return Err(FsError::NotFound),
                }
            }
            match current {
                Node::Dir(children) => Ok((children, name.as_str())),
                Node::File(_) => Err(FsError::NotFound),
            }
        }

        fn to_entry(name: String, node: &Node) -> FsEntry {
            match node {
                Node::Dir(_) => FsEntry {
                    name,
                    is_dir: true,
                    size: 0,
                    last_modified_ms: 0.0,
                },
                Node::File(data) => FsEntry {
                    name,
                    is_dir: false,
                    size: data.len() as u64,
                    last_modified_ms: 0.0,
                },
            }
        }

        fn allocate_handle(inner: &mut Inner) -> u32 {
            let handle = inner.next_handle;
            inner.next_handle = inner.next_handle.wrapping_add(1);
            if inner.next_handle == 0 {
                inner.next_handle = 1;
            }
            handle
        }
    }

    impl DriveFs for MockFs {
        fn stat(&self, path: &str) -> LocalBoxFuture<'_, Result<FsEntry, FsError>> {
            let path = path.to_string();
            Box::pin(async move {
                let components = normalize_path(&path)?;
                let inner = self.inner.borrow();
                let node = Self::find(&inner.root, &components).ok_or(FsError::NotFound)?;
                let name = components.last().cloned().unwrap_or_default();
                Ok(Self::to_entry(name, node))
            })
        }

        fn list(&self, path: &str) -> LocalBoxFuture<'_, Result<Vec<FsEntry>, FsError>> {
            let path = path.to_string();
            Box::pin(async move {
                let components = normalize_path(&path)?;
                let inner = self.inner.borrow();
                let node = Self::find(&inner.root, &components).ok_or(FsError::NotFound)?;
                match node {
                    Node::Dir(children) => Ok(children
                        .iter()
                        .map(|(name, child)| Self::to_entry(name.clone(), child))
                        .collect()),
                    Node::File(_) => Err(FsError::Other("not a directory".to_string())),
                }
            })
        }

        fn open_file(
            &self,
            path: &str,
            write: bool,
            create: bool,
            truncate: bool,
        ) -> LocalBoxFuture<'_, Result<u32, FsError>> {
            let path = path.to_string();
            Box::pin(async move {
                let components = normalize_path(&path)?;
                let mut inner = self.inner.borrow_mut();
                let exists = Self::find(&inner.root, &components).is_some();

                if !exists {
                    if !create {
                        return Err(FsError::NotFound);
                    }
                    let node = Self::make_path_mut(&mut inner.root, &components);
                    *node = Node::File(Vec::new());
                } else if truncate {
                    let node = Self::make_path_mut(&mut inner.root, &components);
                    if matches!(node, Node::Dir(_)) {
                        return Err(FsError::Other("cannot truncate a directory".to_string()));
                    }
                    *node = Node::File(Vec::new());
                }

                let handle = Self::allocate_handle(&mut inner);
                inner.handles.insert(handle, OpenHandle { components, write });
                Ok(handle)
            })
        }

        fn read(&self, handle: u32, offset: u64, len: u32) -> LocalBoxFuture<'_, Result<Vec<u8>, FsError>> {
            Box::pin(async move {
                let inner = self.inner.borrow();
                let open = inner.handles.get(&handle).ok_or(FsError::NotFound)?;
                let node = Self::find(&inner.root, &open.components).ok_or(FsError::NotFound)?;
                let Node::File(data) = node else {
                    return Err(FsError::Other("handle refers to a directory".to_string()));
                };
                let offset = usize::try_from(offset).map_err(|_| FsError::Other("offset overflow".to_string()))?;
                if offset >= data.len() {
                    return Ok(Vec::new());
                }
                let len = usize::try_from(len).map_err(|_| FsError::Other("len overflow".to_string()))?;
                let end = offset.saturating_add(len).min(data.len());
                Ok(data[offset..end].to_vec())
            })
        }

        fn write(&self, handle: u32, offset: u64, data: Vec<u8>) -> LocalBoxFuture<'_, Result<u32, FsError>> {
            Box::pin(async move {
                let mut inner = self.inner.borrow_mut();
                let write_allowed = inner.handles.get(&handle).ok_or(FsError::NotFound)?.write;
                if !write_allowed {
                    return Err(FsError::AccessDenied);
                }
                let components = inner.handles.get(&handle).expect("checked above").components.clone();

                let node = Self::make_path_mut(&mut inner.root, &components);
                let Node::File(existing) = node else {
                    return Err(FsError::Other("handle refers to a directory".to_string()));
                };
                let offset = usize::try_from(offset).map_err(|_| FsError::Other("offset overflow".to_string()))?;
                if existing.len() < offset {
                    existing.resize(offset, 0);
                }
                let end = offset + data.len();
                if existing.len() < end {
                    existing.resize(end, 0);
                }
                existing[offset..end].copy_from_slice(&data);
                let written = u32::try_from(data.len()).map_err(|_| FsError::Other("write too large".to_string()))?;
                Ok(written)
            })
        }

        fn close(&self, handle: u32) -> LocalBoxFuture<'_, Result<(), FsError>> {
            Box::pin(async move {
                let mut inner = self.inner.borrow_mut();
                inner.handles.remove(&handle).map(|_| ()).ok_or(FsError::NotFound)
            })
        }

        fn set_len(&self, handle: u32, size: u64) -> LocalBoxFuture<'_, Result<(), FsError>> {
            Box::pin(async move {
                let mut inner = self.inner.borrow_mut();
                let open = inner.handles.get(&handle).ok_or(FsError::NotFound)?;
                if !open.write {
                    return Err(FsError::AccessDenied);
                }
                let components = open.components.clone();

                let node = Self::make_path_mut(&mut inner.root, &components);
                let Node::File(existing) = node else {
                    return Err(FsError::Other("handle refers to a directory".to_string()));
                };
                let size = usize::try_from(size).map_err(|_| FsError::Other("size overflow".to_string()))?;
                existing.resize(size, 0);
                Ok(())
            })
        }

        fn rename(&self, from: &str, to: &str) -> LocalBoxFuture<'_, Result<(), FsError>> {
            let (from, to) = (from.to_string(), to.to_string());
            Box::pin(async move {
                let from_components = normalize_path(&from)?;
                let to_components = normalize_path(&to)?;
                let mut inner = self.inner.borrow_mut();

                let node = {
                    let (parent, name) = Self::parent_children_mut(&mut inner.root, &from_components)?;
                    parent.remove(name).ok_or(FsError::NotFound)?
                };
                let (parent, name) = Self::parent_children_mut(&mut inner.root, &to_components)?;
                parent.insert(name.to_string(), node);
                Ok(())
            })
        }

        fn delete(&self, path: &str) -> LocalBoxFuture<'_, Result<(), FsError>> {
            let path = path.to_string();
            Box::pin(async move {
                let components = normalize_path(&path)?;
                let mut inner = self.inner.borrow_mut();
                let (parent, name) = Self::parent_children_mut(&mut inner.root, &components)?;
                parent.remove(name).map(|_| ()).ok_or(FsError::NotFound)
            })
        }

        fn mkdir(&self, path: &str) -> LocalBoxFuture<'_, Result<(), FsError>> {
            let path = path.to_string();
            Box::pin(async move {
                let components = normalize_path(&path)?;
                let mut inner = self.inner.borrow_mut();
                let (parent, name) = Self::parent_children_mut(&mut inner.root, &components)?;
                if parent.contains_key(name) {
                    return Err(FsError::Other("already exists".to_string()));
                }
                parent.insert(name.to_string(), Node::new_dir());
                Ok(())
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use std::task::{Context, Poll};

        use super::*;

        /// `MockFs`'s futures never actually suspend (no wasm I/O involved),
        /// so a single poll always resolves them — no executor dependency
        /// needed to test it.
        fn block_on<F: Future>(future: F) -> F::Output {
            let waker = futures_util::task::noop_waker();
            let mut cx = Context::from_waker(&waker);
            let mut future = std::pin::pin!(future);
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(output) => output,
                Poll::Pending => panic!("MockFs futures resolve synchronously; got Pending"),
            }
        }

        #[test]
        fn stat_reports_file_size_and_dir_flag() {
            let fs = MockFs::new();
            fs.seed_file("\\dir\\f.txt", b"hello");
            fs.seed_dir("\\dir\\sub");

            let file = block_on(fs.stat("\\dir\\f.txt")).unwrap();
            assert_eq!(file.name, "f.txt");
            assert!(!file.is_dir);
            assert_eq!(file.size, 5);

            let dir = block_on(fs.stat("\\dir\\sub")).unwrap();
            assert!(dir.is_dir);
            assert_eq!(dir.size, 0);
        }

        #[test]
        fn stat_unknown_path_is_not_found() {
            let fs = MockFs::new();
            assert_eq!(block_on(fs.stat("\\missing")), Err(FsError::NotFound));
        }

        #[test]
        fn list_returns_direct_children_only() {
            let fs = MockFs::new();
            fs.seed_file("\\dir\\a.txt", b"a");
            fs.seed_file("\\dir\\b.txt", b"bb");
            fs.seed_file("\\dir\\sub\\deep.txt", b"nested");

            let mut names: Vec<String> = block_on(fs.list("\\dir"))
                .unwrap()
                .into_iter()
                .map(|e| e.name)
                .collect();
            names.sort();
            assert_eq!(names, vec!["a.txt".to_string(), "b.txt".to_string(), "sub".to_string()]);
        }

        #[test]
        fn list_on_a_file_is_rejected() {
            let fs = MockFs::new();
            fs.seed_file("\\f.txt", b"x");
            assert!(matches!(block_on(fs.list("\\f.txt")), Err(FsError::Other(_))));
        }

        #[test]
        fn open_without_create_on_missing_path_is_not_found() {
            let fs = MockFs::new();
            assert_eq!(
                block_on(fs.open_file("\\missing.txt", false, false, false)),
                Err(FsError::NotFound)
            );
        }

        #[test]
        fn open_with_create_makes_a_new_empty_file() {
            let fs = MockFs::new();
            let handle = block_on(fs.open_file("\\new.txt", true, true, false)).unwrap();
            let entry = block_on(fs.stat("\\new.txt")).unwrap();
            assert_eq!(entry.size, 0);
            block_on(fs.close(handle)).unwrap();
        }

        #[test]
        fn write_then_read_round_trips_bytes() {
            let fs = MockFs::new();
            let handle = block_on(fs.open_file("\\rw.txt", true, true, false)).unwrap();

            let written = block_on(fs.write(handle, 0, b"hello world".to_vec())).unwrap();
            assert_eq!(written, 11);

            let read_back = block_on(fs.read(handle, 0, 11)).unwrap();
            assert_eq!(read_back, b"hello world");

            let partial = block_on(fs.read(handle, 6, 5)).unwrap();
            assert_eq!(partial, b"world");
        }

        #[test]
        fn write_without_write_flag_is_access_denied() {
            let fs = MockFs::new();
            fs.seed_file("\\ro.txt", b"immutable");
            let handle = block_on(fs.open_file("\\ro.txt", false, false, false)).unwrap();
            assert_eq!(block_on(fs.write(handle, 0, b"x".to_vec())), Err(FsError::AccessDenied));
        }

        #[test]
        fn read_after_close_is_not_found() {
            let fs = MockFs::new();
            let handle = block_on(fs.open_file("\\a.txt", true, true, false)).unwrap();
            block_on(fs.close(handle)).unwrap();
            assert_eq!(block_on(fs.read(handle, 0, 1)), Err(FsError::NotFound));
        }

        #[test]
        fn mkdir_then_delete_round_trips() {
            let fs = MockFs::new();
            block_on(fs.mkdir("\\newdir")).unwrap();
            assert!(block_on(fs.stat("\\newdir")).unwrap().is_dir);

            block_on(fs.delete("\\newdir")).unwrap();
            assert_eq!(block_on(fs.stat("\\newdir")), Err(FsError::NotFound));
        }

        #[test]
        fn rename_moves_a_file() {
            let fs = MockFs::new();
            fs.seed_file("\\old.txt", b"payload");

            block_on(fs.rename("\\old.txt", "\\new.txt")).unwrap();

            assert_eq!(block_on(fs.stat("\\old.txt")), Err(FsError::NotFound));
            let entry = block_on(fs.stat("\\new.txt")).unwrap();
            assert_eq!(entry.size, 7);
        }
    }
}
