// SPDX-License-Identifier: MIT
// bsr-core — DropOldestBuffer
//
// Seed-BSR-G2-03-11: Bounded ring buffer that drops the OLDEST item when at
// capacity, enforcing the DropOldest backpressure policy declared in the
// saddle config.

use std::collections::VecDeque;

/// A bounded ring buffer. When `push` is called on a full buffer the oldest
/// item (front) is evicted and the new item is appended, keeping the buffer
/// at exactly `capacity` items. Eviction counts are tracked for telemetry.
pub struct DropOldestBuffer<T> {
    buf: VecDeque<T>,
    capacity: usize,
    dropped_count: u64,
}

impl<T> DropOldestBuffer<T> {
    /// Create a new buffer with the given maximum capacity.
    ///
    /// # Panics
    /// Panics if `capacity` is 0.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "DropOldestBuffer capacity must be > 0");
        Self {
            buf: VecDeque::with_capacity(capacity),
            capacity,
            dropped_count: 0,
        }
    }

    /// Push `item` into the buffer.
    ///
    /// If the buffer is already at capacity, the oldest item (front) is
    /// silently dropped and the internal `dropped_count` is incremented.
    ///
    /// Returns `true` if an existing item was dropped to make room.
    pub fn push(&mut self, item: T) -> bool {
        if self.buf.len() >= self.capacity {
            self.buf.pop_front();
            self.dropped_count += 1;
            self.buf.push_back(item);
            true
        } else {
            self.buf.push_back(item);
            false
        }
    }

    /// Remove and return the oldest item (FIFO order).
    pub fn pop(&mut self) -> Option<T> {
        self.buf.pop_front()
    }

    /// Number of items currently in the buffer.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// `true` when the buffer holds no items.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Total number of items dropped since this buffer was created.
    pub fn dropped_count(&self) -> u64 {
        self.dropped_count
    }

    /// Maximum number of items the buffer can hold before dropping.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_below_capacity_no_drop() {
        let mut buf: DropOldestBuffer<i32> = DropOldestBuffer::new(4);
        assert!(!buf.push(1));
        assert!(!buf.push(2));
        assert_eq!(buf.dropped_count(), 0);
        assert_eq!(buf.len(), 2);
    }

    #[test]
    fn push_at_capacity_drops_oldest() {
        let mut buf: DropOldestBuffer<i32> = DropOldestBuffer::new(3);
        buf.push(1);
        buf.push(2);
        buf.push(3);
        // Buffer is full — next push drops item 1.
        let dropped = buf.push(4);
        assert!(dropped, "expected a drop to occur");
        assert_eq!(buf.dropped_count(), 1);
        assert_eq!(buf.len(), 3);
        // Oldest surviving item should be 2.
        assert_eq!(buf.pop(), Some(2));
        assert_eq!(buf.pop(), Some(3));
        assert_eq!(buf.pop(), Some(4));
        assert!(buf.is_empty());
    }

    #[test]
    fn pop_empty_returns_none() {
        let mut buf: DropOldestBuffer<u8> = DropOldestBuffer::new(8);
        assert_eq!(buf.pop(), None);
    }

    #[test]
    #[should_panic(expected = "capacity must be > 0")]
    fn zero_capacity_panics() {
        let _: DropOldestBuffer<u8> = DropOldestBuffer::new(0);
    }
}
