//! Bounded in-memory storage for sampled records.

use std::collections::VecDeque;
use std::collections::vec_deque::Iter;
use std::num::NonZeroUsize;

/// A fixed-capacity buffer that discards its oldest item when a new one
/// arrives and it is full.
///
/// Memory use is bounded: the space for `capacity` items is allocated once, up
/// front, and never grows.
///
/// ```
/// use std::num::NonZeroUsize;
/// use simple_metrics::store::RingBuffer;
///
/// let mut buffer = RingBuffer::new(NonZeroUsize::new(2).unwrap());
/// assert_eq!(buffer.push("a"), None);
/// assert_eq!(buffer.push("b"), None);
/// assert_eq!(buffer.push("c"), Some("a")); // "a" was the oldest, so it goes
/// assert_eq!(buffer.iter().copied().collect::<Vec<_>>(), ["b", "c"]);
/// ```
#[derive(Debug, Clone)]
pub struct RingBuffer<T> {
    items: VecDeque<T>,
    capacity: NonZeroUsize,
}

impl<T> RingBuffer<T> {
    /// Creates an empty buffer that holds at most `capacity` items.
    #[must_use]
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            items: VecDeque::with_capacity(capacity.get()),
            capacity,
        }
    }

    /// Adds `item` as the newest entry. If the buffer was already full, the
    /// oldest entry is removed and returned.
    pub fn push(&mut self, item: T) -> Option<T> {
        let evicted = if self.items.len() == self.capacity.get() {
            self.items.pop_front()
        } else {
            None
        };
        self.items.push_back(item);
        evicted
    }

    /// The number of items currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the buffer holds no items.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The most items the buffer will hold.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity.get()
    }

    /// Iterates from the oldest item to the newest.
    #[must_use]
    pub fn iter(&self) -> Iter<'_, T> {
        self.items.iter()
    }
}

impl<'a, T> IntoIterator for &'a RingBuffer<T> {
    type Item = &'a T;
    type IntoIter = Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(capacity: usize) -> RingBuffer<u32> {
        RingBuffer::new(NonZeroUsize::new(capacity).unwrap())
    }

    fn contents(buffer: &RingBuffer<u32>) -> Vec<u32> {
        buffer.iter().copied().collect()
    }

    #[test]
    fn starts_empty() {
        let buffer = buffer(3);
        assert!(buffer.is_empty());
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.capacity(), 3);
    }

    #[test]
    fn keeps_everything_until_full() {
        let mut buffer = buffer(3);
        assert_eq!(buffer.push(1), None);
        assert_eq!(buffer.push(2), None);
        assert_eq!(buffer.push(3), None);
        assert_eq!(contents(&buffer), [1, 2, 3]);
        assert_eq!(buffer.len(), 3);
    }

    #[test]
    fn discards_the_oldest_when_full() {
        let mut buffer = buffer(3);
        for n in 1..=3 {
            buffer.push(n);
        }
        assert_eq!(buffer.push(4), Some(1));
        assert_eq!(buffer.push(5), Some(2));
        assert_eq!(contents(&buffer), [3, 4, 5]);
    }

    #[test]
    fn never_grows_past_its_capacity() {
        let mut buffer = buffer(5);
        for n in 0..1_000 {
            buffer.push(n);
            assert!(buffer.len() <= 5);
        }
        assert_eq!(contents(&buffer), [995, 996, 997, 998, 999]);
    }

    #[test]
    fn a_capacity_of_one_holds_only_the_latest() {
        let mut buffer = buffer(1);
        assert_eq!(buffer.push(1), None);
        assert_eq!(buffer.push(2), Some(1));
        assert_eq!(contents(&buffer), [2]);
    }

    #[test]
    fn can_be_iterated_by_reference() {
        let mut buffer = buffer(3);
        buffer.push(7);
        buffer.push(8);
        let mut seen = Vec::new();
        for item in &buffer {
            seen.push(*item);
        }
        assert_eq!(seen, [7, 8]);
    }
}
