//! Bounded in-memory storage for sampled records.

use std::fmt;
use std::num::NonZeroUsize;

/// Why a record was refused by [`Store::push`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushError {
    /// The row didn't have as many values as the store's records do.
    WrongWidth {
        /// How many values a record holds.
        expected: usize,
        /// How many the row had.
        got: usize,
    },
    /// The timestamp wasn't later than the newest record's, so the records
    /// would no longer be in time order.
    OutOfOrder {
        /// The newest record's timestamp.
        newest: u64,
        /// The refused timestamp.
        got: u64,
    },
}

impl fmt::Display for PushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongWidth { expected, got } => {
                write!(f, "a record has {expected} values, but the row has {got}")
            }
            Self::OutOfOrder { newest, got } => {
                write!(f, "timestamp {got} is not after the newest one, {newest}")
            }
        }
    }
}

impl std::error::Error for PushError {}

/// The store's contents copied out as columns: one list of timestamps and one
/// list of values per metric, all the same length, oldest first.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// Milliseconds since the Unix epoch.
    pub timestamps: Vec<u64>,
    /// `columns[m][i]` is metric `m`'s value at `timestamps[i]`.
    pub columns: Vec<Vec<f64>>,
}

/// A fixed-capacity store of records, each a timestamp and the same number of
/// values. When it is full, adding a record discards the oldest.
///
/// Memory use is bounded and known up front: room for `capacity` records is
/// allocated once and never grows. See [`Store::bytes_for`].
///
/// ```
/// use std::num::NonZeroUsize;
/// use simple_metrics::store::Store;
///
/// let mut store = Store::new(NonZeroUsize::new(2).unwrap(), 1).unwrap();
/// store.push(1000, &[1.0]).unwrap();
/// store.push(2000, &[2.0]).unwrap();
/// store.push(3000, &[3.0]).unwrap(); // the record at 1000 is discarded
/// assert_eq!(store.snapshot().timestamps, [2000, 3000]);
/// ```
#[derive(Debug, Clone)]
pub struct Store {
    width: usize,
    capacity: NonZeroUsize,
    /// Timestamps in a ring; `head` is the index of the oldest.
    timestamps: Vec<u64>,
    /// `capacity` rows of `width` values, in the same ring order.
    values: Vec<f64>,
    head: usize,
    len: usize,
}

impl Store {
    /// The memory a store of this size takes, in bytes: a timestamp and
    /// `width` values for each of `capacity` records. `None` if that doesn't
    /// fit in a `usize`.
    #[must_use]
    pub fn bytes_for(capacity: usize, width: usize) -> Option<usize> {
        width
            .checked_add(1)?
            .checked_mul(size_of::<u64>())?
            .checked_mul(capacity)
    }

    /// Creates an empty store that holds at most `capacity` records of `width`
    /// values each. Returns `None` if that is too big to allocate.
    #[must_use]
    pub fn new(capacity: NonZeroUsize, width: usize) -> Option<Self> {
        Self::bytes_for(capacity.get(), width)?;
        let cells = capacity.get().checked_mul(width)?;
        Some(Self {
            width,
            capacity,
            // Zeroed memory comes from the OS lazily, so the real footprint
            // grows as the store fills rather than all at once at startup.
            timestamps: vec![0; capacity.get()],
            values: vec![0.0; cells],
            head: 0,
            len: 0,
        })
    }

    /// Adds a record as the newest, discarding the oldest if the store is full.
    ///
    /// # Errors
    /// If `row` isn't [`Store::width`] values long, or `timestamp` isn't later
    /// than the newest record's. The store is unchanged in either case.
    pub fn push(&mut self, timestamp: u64, row: &[f64]) -> Result<(), PushError> {
        if row.len() != self.width {
            return Err(PushError::WrongWidth {
                expected: self.width,
                got: row.len(),
            });
        }
        if let Some((newest, _)) = self.latest() {
            if timestamp <= newest {
                return Err(PushError::OutOfOrder {
                    newest,
                    got: timestamp,
                });
            }
        }

        let slot = if self.len < self.capacity.get() {
            let slot = self.slot(self.len);
            self.len += 1;
            slot
        } else {
            // Full: the oldest record's slot becomes the newest.
            let slot = self.head;
            self.head = (self.head + 1) % self.capacity.get();
            slot
        };
        self.timestamps[slot] = timestamp;
        self.values[slot * self.width..(slot + 1) * self.width].copy_from_slice(row);
        Ok(())
    }

    /// The newest record's timestamp and values.
    #[must_use]
    pub fn latest(&self) -> Option<(u64, &[f64])> {
        let last = self.len.checked_sub(1)?;
        let slot = self.slot(last);
        Some((
            self.timestamps[slot],
            &self.values[slot * self.width..(slot + 1) * self.width],
        ))
    }

    /// Copies out everything held, oldest first.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let mut timestamps = Vec::with_capacity(self.len);
        let mut columns = vec![Vec::with_capacity(self.len); self.width];
        for i in 0..self.len {
            let slot = self.slot(i);
            timestamps.push(self.timestamps[slot]);
            let row = &self.values[slot * self.width..(slot + 1) * self.width];
            for (column, value) in columns.iter_mut().zip(row) {
                column.push(*value);
            }
        }
        Snapshot {
            timestamps,
            columns,
        }
    }

    /// How many records are held now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no records are held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The most records the store will hold.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity.get()
    }

    /// How many values each record has.
    #[must_use]
    pub fn width(&self) -> usize {
        self.width
    }

    /// The slot holding the `i`th oldest record (0 = oldest).
    fn slot(&self, i: usize) -> usize {
        (self.head + i) % self.capacity.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(capacity: usize, width: usize) -> Store {
        Store::new(NonZeroUsize::new(capacity).unwrap(), width).unwrap()
    }

    fn timestamps(store: &Store) -> Vec<u64> {
        store.snapshot().timestamps
    }

    #[test]
    fn starts_empty() {
        let store = store(3, 2);
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert_eq!(store.capacity(), 3);
        assert_eq!(store.width(), 2);
        assert_eq!(store.latest(), None);
        let snapshot = store.snapshot();
        assert!(snapshot.timestamps.is_empty());
        assert_eq!(snapshot.columns, [Vec::<f64>::new(), Vec::new()]);
    }

    #[test]
    fn keeps_everything_until_full() {
        let mut store = store(3, 1);
        for t in 1..=3_u32 {
            store.push(u64::from(t) * 1000, &[f64::from(t)]).unwrap();
        }
        assert_eq!(store.len(), 3);
        assert_eq!(timestamps(&store), [1000, 2000, 3000]);
    }

    #[test]
    fn discards_the_oldest_when_full() {
        let mut store = store(3, 1);
        for t in 1..=5_u32 {
            store.push(u64::from(t) * 1000, &[f64::from(t)]).unwrap();
        }
        assert_eq!(store.len(), 3);
        let snapshot = store.snapshot();
        assert_eq!(snapshot.timestamps, [3000, 4000, 5000]);
        assert_eq!(snapshot.columns, [vec![3.0, 4.0, 5.0]]);
    }

    #[test]
    fn stays_correct_over_many_wraps() {
        let mut store = store(5, 2);
        for t in 1..=1_003_u32 {
            let value = f64::from(t);
            store.push(u64::from(t), &[value, -value]).unwrap();
            assert!(store.len() <= 5);
        }
        let snapshot = store.snapshot();
        assert_eq!(snapshot.timestamps, [999, 1000, 1001, 1002, 1003]);
        assert_eq!(snapshot.columns[0], [999.0, 1000.0, 1001.0, 1002.0, 1003.0]);
        assert_eq!(
            snapshot.columns[1],
            [-999.0, -1000.0, -1001.0, -1002.0, -1003.0]
        );
    }

    #[test]
    fn latest_is_the_newest_record_before_and_after_wrapping() {
        let mut store = store(2, 2);
        store.push(10, &[1.0, 2.0]).unwrap();
        assert_eq!(store.latest(), Some((10, &[1.0, 2.0][..])));
        store.push(20, &[3.0, 4.0]).unwrap();
        store.push(30, &[5.0, 6.0]).unwrap();
        assert_eq!(store.latest(), Some((30, &[5.0, 6.0][..])));
    }

    #[test]
    fn a_capacity_of_one_holds_only_the_latest() {
        let mut store = store(1, 1);
        store.push(1, &[1.0]).unwrap();
        store.push(2, &[2.0]).unwrap();
        assert_eq!(timestamps(&store), [2]);
        assert_eq!(store.latest(), Some((2, &[2.0][..])));
    }

    #[test]
    fn a_row_of_the_wrong_width_is_refused_and_changes_nothing() {
        let mut store = store(3, 2);
        store.push(1, &[1.0, 1.0]).unwrap();
        assert_eq!(
            store.push(2, &[1.0]),
            Err(PushError::WrongWidth {
                expected: 2,
                got: 1
            })
        );
        assert!(store.push(2, &[1.0, 2.0, 3.0]).is_err());
        assert_eq!(timestamps(&store), [1]);
    }

    #[test]
    fn a_timestamp_that_is_not_later_is_refused_and_changes_nothing() {
        let mut store = store(3, 1);
        store.push(100, &[1.0]).unwrap();
        for bad in [100, 99, 0] {
            assert_eq!(
                store.push(bad, &[9.0]),
                Err(PushError::OutOfOrder {
                    newest: 100,
                    got: bad
                })
            );
        }
        assert_eq!(store.snapshot().columns, [vec![1.0]]);
        store.push(101, &[2.0]).unwrap();
    }

    #[test]
    fn values_that_are_not_numbers_are_kept_as_they_are() {
        let mut store = store(2, 1);
        store.push(1, &[f64::NAN]).unwrap();
        assert!(store.snapshot().columns[0][0].is_nan());
    }

    #[test]
    fn memory_use_is_known_up_front() {
        // 7 days at 5 s is 120,960 records; each is a timestamp + 15 values.
        assert_eq!(Store::bytes_for(120_960, 15), Some(120_960 * 16 * 8));
        assert_eq!(Store::bytes_for(usize::MAX, 15), None);
        assert_eq!(Store::bytes_for(1, usize::MAX), None);
    }

    #[test]
    fn a_store_too_big_to_allocate_is_refused() {
        assert!(Store::new(NonZeroUsize::MAX, 15).is_none());
    }

    #[test]
    fn the_doc_example_holds() {
        let mut store = store(2, 1);
        store.push(1000, &[1.0]).unwrap();
        store.push(2000, &[2.0]).unwrap();
        store.push(3000, &[3.0]).unwrap();
        assert_eq!(timestamps(&store), [2000, 3000]);
    }
}
