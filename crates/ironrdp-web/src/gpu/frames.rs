//! `VideoFrame`s decoded on the GPU, waiting for the backend to draw them, by handle.
//!
//! A [`DecodedFrame::external`](ironrdp_egfx::decode::DecodedFrame::external) frame carries
//! only the handle: the frame itself is a JS object that cannot cross the `Send` boundary.

use core::cell::RefCell;
use std::collections::BTreeMap;

use crate::h264::VideoFrame;

/// Items by handle, each handed out once.
struct Store<T> {
    next: u64,
    items: BTreeMap<u64, T>,
}

impl<T> Default for Store<T> {
    fn default() -> Self {
        Self {
            next: 0,
            items: BTreeMap::new(),
        }
    }
}

impl<T> Store<T> {
    fn insert(&mut self, item: T) -> u64 {
        let handle = self.next;
        self.next = self.next.wrapping_add(1);
        self.items.insert(handle, item);
        handle
    }

    fn take(&mut self, handle: u64) -> Option<T> {
        self.items.remove(&handle)
    }
}

thread_local! {
    static FRAMES: RefCell<Store<VideoFrame>> = RefCell::new(Store::default());
}

/// Holds `frame` until the backend draws it, returning its handle.
///
/// The pipeline has one H.264 frame in flight, so any frame still held is one the pipeline
/// dropped (a failed or closed decode). It is closed here: an unclosed `VideoFrame` holds a
/// decoder buffer, and enough of them stall the decoder.
pub(crate) fn insert(frame: VideoFrame) -> u64 {
    FRAMES.with(|frames| {
        let mut frames = frames.borrow_mut();
        for (_, stale) in core::mem::take(&mut frames.items) {
            stale.close();
        }
        frames.insert(frame)
    })
}

/// The frame for `handle`, once.
pub(crate) fn take(handle: u64) -> Option<VideoFrame> {
    FRAMES.with(|frames| frames.borrow_mut().take(handle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_hands_out_each_item_once() {
        let mut store = Store::default();
        let first = store.insert("a");
        let second = store.insert("b");

        assert_ne!(first, second);
        assert_eq!(store.take(second), Some("b"));
        assert_eq!(store.take(second), None);
        assert_eq!(store.take(first), Some("a"));
    }
}
