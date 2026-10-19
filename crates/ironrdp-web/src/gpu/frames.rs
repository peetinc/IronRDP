//! `VideoFrame`s decoded on the GPU, waiting for the backend to draw them, by handle.
//!
//! A [`DecodedFrame::external`](ironrdp_egfx::decode::DecodedFrame::external) frame carries
//! only the handle: the frame itself is a JS object that cannot cross the `Send` boundary.

use core::cell::RefCell;
use std::collections::BTreeMap;

use crate::h264::VideoFrame;

/// Frames one decoder may hold at once. It has a few decoding at a time and the backend
/// draws each as it applies, so more than this are frames its pipeline dropped.
const MAX_HELD_PER_OWNER: usize = 8;

/// Items by handle, each handed out once, and by the owner that inserted them.
struct Store<T> {
    next: u64,
    items: BTreeMap<u64, (u32, T)>,
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
    /// Holds `item` for `owner`, handing back the owner's oldest items beyond
    /// [`MAX_HELD_PER_OWNER`].
    fn insert(&mut self, owner: u32, item: T) -> (u64, Vec<T>) {
        let handle = self.next;
        self.next = self.next.wrapping_add(1);
        self.items.insert(handle, (owner, item));

        let held: Vec<u64> = self
            .items
            .iter()
            .filter(|(_, (item_owner, _))| *item_owner == owner)
            .map(|(handle, _)| *handle)
            .collect();
        let excess = held.len().saturating_sub(MAX_HELD_PER_OWNER);
        let stale = held[..excess].iter().filter_map(|handle| self.take(*handle)).collect();
        (handle, stale)
    }

    fn take(&mut self, handle: u64) -> Option<T> {
        self.items.remove(&handle).map(|(_, item)| item)
    }

    /// Every item `owner` still holds.
    fn release(&mut self, owner: u32) -> Vec<T> {
        let handles: Vec<u64> = self
            .items
            .iter()
            .filter(|(_, (item_owner, _))| *item_owner == owner)
            .map(|(handle, _)| *handle)
            .collect();
        handles.into_iter().filter_map(|handle| self.take(handle)).collect()
    }
}

thread_local! {
    static FRAMES: RefCell<Store<VideoFrame>> = RefCell::new(Store::default());
}

/// Holds `frame`, from the decoder `owner`, until the backend draws it, returning its handle.
///
/// A decoder has a few H.264 frames in flight, so beyond [`MAX_HELD_PER_OWNER`] its oldest
/// frames are ones its pipeline dropped (a failed or closed decode). They are closed here: an
/// unclosed `VideoFrame` holds a decoder buffer, and enough of them stall the decoder. Other
/// decoders' frames, from other sessions on the page, are left alone.
pub(crate) fn insert(owner: u32, frame: VideoFrame) -> u64 {
    let (handle, stale) = FRAMES.with(|frames| frames.borrow_mut().insert(owner, frame));
    for frame in stale {
        frame.close();
    }
    handle
}

/// The frame for `handle`, once.
pub(crate) fn take(handle: u64) -> Option<VideoFrame> {
    FRAMES.with(|frames| frames.borrow_mut().take(handle))
}

/// Closes every frame the decoder `owner` still holds; called as the decoder goes away.
pub(crate) fn release(owner: u32) {
    let stale = FRAMES.with(|frames| frames.borrow_mut().release(owner));
    for frame in stale {
        frame.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_hands_out_each_item_once() {
        let mut store = Store::default();
        let (first, _) = store.insert(1, "a");
        let (second, _) = store.insert(2, "b");

        assert_ne!(first, second);
        assert_eq!(store.take(second), Some("b"));
        assert_eq!(store.take(second), None);
        assert_eq!(store.take(first), Some("a"));
    }

    #[test]
    fn an_owner_can_hold_several_frames() {
        let mut store = Store::default();
        let (first, stale) = store.insert(1, "a1");
        assert!(stale.is_empty());
        let (second, stale) = store.insert(1, "a2");
        assert!(stale.is_empty(), "frames in flight are not stale");

        assert_eq!(store.take(first), Some("a1"));
        assert_eq!(store.take(second), Some("a2"));
    }

    #[test]
    fn insert_closes_only_beyond_the_cap() {
        let mut store = Store::default();
        let (other, _) = store.insert(2, 1000);
        let mut handles = Vec::new();
        for item in 0..MAX_HELD_PER_OWNER {
            let (handle, stale) = store.insert(1, item);
            assert!(stale.is_empty());
            handles.push(handle);
        }

        let (_, stale) = store.insert(1, MAX_HELD_PER_OWNER);

        assert_eq!(stale, vec![0], "the owner's oldest frame goes");
        assert_eq!(store.take(handles[0]), None);
        assert_eq!(store.take(handles[1]), Some(1));
        assert_eq!(store.take(other), Some(1000), "other owners' frames stay");
    }

    #[test]
    fn release_hands_back_every_item_of_an_owner() {
        let mut store = Store::default();
        let (a, _) = store.insert(1, "a");
        let (b, _) = store.insert(2, "b");

        assert_eq!(store.release(1), vec!["a"]);
        assert_eq!(store.take(a), None);
        assert_eq!(store.take(b), Some("b"));
    }
}
