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

/// The most buckets one downsampled query may produce: enough for a whole
/// store at its finest resolution, and a bound on the memory a query can use.
pub const MAX_BUCKETS: u64 = 200_000;

/// A query's records grouped into equal time buckets, one value per metric
/// per bucket. Buckets are aligned to multiples of `step_ms` since the Unix
/// epoch, so a later query for a later window uses the same bucket edges.
/// A value is `NaN` if nothing usable fell in its bucket.
#[derive(Debug, Clone, PartialEq)]
pub struct Downsampled {
    /// The width of each bucket, in milliseconds.
    pub step_ms: u64,
    /// Each bucket's start time, in milliseconds since the Unix epoch.
    pub timestamps: Vec<u64>,
    /// `avg[m][b]` is the mean of metric `m` over bucket `b`.
    pub avg: Vec<Vec<f64>>,
    /// The smallest value in each bucket, if asked for.
    pub min: Option<Vec<Vec<f64>>>,
    /// The largest value in each bucket, if asked for.
    pub max: Option<Vec<Vec<f64>>>,
}

/// A downsampled query would have produced more than [`MAX_BUCKETS`] buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooManyBuckets {
    /// How many it would have taken.
    pub buckets: u64,
}

impl fmt::Display for TooManyBuckets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "that would be {} buckets (at most {MAX_BUCKETS}): use a larger step_ms or a max_points",
            self.buckets
        )
    }
}

impl std::error::Error for TooManyBuckets {}

/// Picks a bucket width for at most `max_points` buckets over the data from
/// `first` to `last` (milliseconds, `first <= last`), given samples
/// `interval_ms` apart. The width is a whole number of samples, so a bucket
/// never ends up systematically half-empty, and it is chosen so the buckets
/// (aligned to the epoch, as [`Downsampled`] describes) really are no more
/// than `max_points`. `max_points` must be at least 2: one point can't span
/// data that straddles a bucket edge, however wide the bucket.
#[must_use]
pub fn step_for_max_points(first: u64, last: u64, max_points: u64, interval_ms: u64) -> u64 {
    let interval = interval_ms.max(1);
    let max_points = max_points.max(2);
    let span = last.saturating_sub(first).saturating_add(1);
    let whole_samples = |step: u64| {
        step.div_ceil(interval)
            .saturating_mul(interval)
            .max(interval)
    };

    let mut step = whole_samples(span.div_ceil(max_points));
    // Epoch alignment can put the data in one bucket more than span/step
    // suggests, so widen (by a bit at a time, ending for certain once one
    // bucket is at least as wide as the data) until it fits.
    while bucket_count(first, last, step) > max_points {
        step = whole_samples(step.saturating_add(step.div_ceil(8)));
    }
    step
}

/// How many `step`-wide epoch-aligned buckets are needed to cover `first..=last`.
fn bucket_count(first: u64, last: u64, step: u64) -> u64 {
    (last - last % step - (first - first % step)) / step + 1
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
        let all: Vec<usize> = (0..self.width).collect();
        self.snapshot_range(None, None, &all)
    }

    /// Copies out the records from `from` to `to` (both inclusive, in
    /// milliseconds; `None` for no limit), oldest first, keeping only the
    /// values at the `columns` indices, in that order. An index past the last
    /// value gives `NaN`s.
    #[must_use]
    pub fn snapshot_range(
        &self,
        from: Option<u64>,
        to: Option<u64>,
        columns: &[usize],
    ) -> Snapshot {
        let (start, end) = self.index_range(from, to);
        let mut timestamps = Vec::with_capacity(end - start);
        let mut out = vec![Vec::with_capacity(end - start); columns.len()];
        for i in start..end {
            let slot = self.slot(i);
            timestamps.push(self.timestamps[slot]);
            let row = self.row(slot);
            for (column, index) in out.iter_mut().zip(columns) {
                column.push(row.get(*index).copied().unwrap_or(f64::NAN));
            }
        }
        Snapshot {
            timestamps,
            columns: out,
        }
    }

    /// The timestamps of the first and last records from `from` to `to`
    /// (both inclusive; `None` for no limit), or `None` if there are none.
    #[must_use]
    pub fn bounds(&self, from: Option<u64>, to: Option<u64>) -> Option<(u64, u64)> {
        let (start, end) = self.index_range(from, to);
        if start >= end {
            return None;
        }
        Some((
            self.timestamps[self.slot(start)],
            self.timestamps[self.slot(end - 1)],
        ))
    }

    /// Groups the records from `from` to `to` (both inclusive; `None` for no
    /// limit) into `step_ms`-wide buckets and summarises the values at the
    /// `columns` indices in each: the mean, and the smallest and largest too if
    /// `extremes`. Values that aren't finite are left out; a bucket with none
    /// left has `NaN`. Every bucket from the first record's to the last's is
    /// present, so a stretch with no records shows as a run of `NaN`s.
    ///
    /// # Errors
    /// If that would take more than [`MAX_BUCKETS`] buckets.
    pub fn downsample(
        &self,
        from: Option<u64>,
        to: Option<u64>,
        columns: &[usize],
        step_ms: u64,
        extremes: bool,
    ) -> Result<Downsampled, TooManyBuckets> {
        let step = step_ms.max(1);
        let (start, end) = self.index_range(from, to);
        let blank = |n: usize| vec![vec![f64::NAN; n]; columns.len()];
        if start >= end {
            return Ok(Downsampled {
                step_ms: step,
                timestamps: Vec::new(),
                avg: blank(0),
                min: extremes.then(|| blank(0)),
                max: extremes.then(|| blank(0)),
            });
        }

        let first = self.timestamps[self.slot(start)];
        let last = self.timestamps[self.slot(end - 1)];
        let buckets = bucket_count(first, last, step);
        if buckets > MAX_BUCKETS {
            return Err(TooManyBuckets { buckets });
        }
        let count = usize::try_from(buckets).map_err(|_| TooManyBuckets { buckets })?;
        let first_bucket = first - first % step;

        let mut result = Downsampled {
            step_ms: step,
            timestamps: (0..buckets).map(|b| first_bucket + b * step).collect(),
            avg: blank(count),
            min: extremes.then(|| blank(count)),
            max: extremes.then(|| blank(count)),
        };

        // Records are in time order, so each bucket is a run of them: sum up
        // one run at a time and store its summary when the next begins.
        let mut running = vec![Running::default(); columns.len()];
        let mut current = 0_usize;
        for i in start..end {
            let slot = self.slot(i);
            let bucket =
                usize::try_from((self.timestamps[slot] - first_bucket) / step).unwrap_or(count - 1);
            if bucket != current {
                store_summary(&mut result, current, &running);
                running.fill(Running::default());
                current = bucket;
            }
            let row = self.row(slot);
            for (run, index) in running.iter_mut().zip(columns) {
                if let Some(value) = row.get(*index).copied().filter(|v| v.is_finite()) {
                    run.add(value);
                }
            }
        }
        store_summary(&mut result, current, &running);
        Ok(result)
    }

    /// The logical index (0 = oldest) of the first record for which `before`
    /// is false, given that it is true for some run of records at the start.
    fn partition(&self, mut before: impl FnMut(u64) -> bool) -> usize {
        let (mut low, mut high) = (0, self.len);
        while low < high {
            let middle = low + (high - low) / 2;
            if before(self.timestamps[self.slot(middle)]) {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    /// The range of logical indices of the records from `from` to `to`
    /// (both inclusive; `None` for no limit), as `start..end`.
    fn index_range(&self, from: Option<u64>, to: Option<u64>) -> (usize, usize) {
        let start = from.map_or(0, |from| self.partition(|t| t < from));
        let end = to.map_or(self.len, |to| self.partition(|t| t <= to));
        (start, end.max(start))
    }

    /// The values of the record in `slot`.
    fn row(&self, slot: usize) -> &[f64] {
        &self.values[slot * self.width..(slot + 1) * self.width]
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

/// A bucket's running totals for one metric.
#[derive(Debug, Clone, Copy)]
struct Running {
    sum: f64,
    count: u32,
    min: f64,
    max: f64,
}

impl Default for Running {
    fn default() -> Self {
        Self {
            sum: 0.0,
            count: 0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }
}

impl Running {
    fn add(&mut self, value: f64) {
        self.sum += value;
        self.count += 1;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
    }
}

/// Writes a finished bucket's summaries into `result`. A metric with no usable
/// values in it is left as the `NaN` it was created with.
fn store_summary(result: &mut Downsampled, bucket: usize, running: &[Running]) {
    for (metric, run) in running.iter().enumerate() {
        if run.count == 0 {
            continue;
        }
        result.avg[metric][bucket] = run.sum / f64::from(run.count);
        if let Some(min) = &mut result.min {
            min[metric][bucket] = run.min;
        }
        if let Some(max) = &mut result.max {
            max[metric][bucket] = run.max;
        }
    }
}

#[cfg(test)]
// The expected values in these tests (means of small whole numbers) are exactly
// representable, so comparing them exactly is right.
#[allow(clippy::float_cmp)]
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
    // ---- ranges and subsets -------------------------------------------------

    /// A store of capacity 10, width 2, holding records at 1000, 2000, ... 5000
    /// whose values are `[t/1000, t/1000 * 10]`.
    fn five_records() -> Store {
        let mut store = store(10, 2);
        for i in 1..=5_u32 {
            store
                .push(u64::from(i) * 1000, &[f64::from(i), f64::from(i) * 10.0])
                .unwrap();
        }
        store
    }

    fn range(store: &Store, from: Option<u64>, to: Option<u64>) -> Vec<u64> {
        store.snapshot_range(from, to, &[0]).timestamps
    }

    #[test]
    fn a_range_includes_both_of_its_ends() {
        let store = five_records();
        assert_eq!(range(&store, Some(2000), Some(4000)), [2000, 3000, 4000]);
        assert_eq!(range(&store, Some(2000), Some(2000)), [2000]);
    }

    #[test]
    fn an_end_between_records_is_rounded_inwards() {
        let store = five_records();
        assert_eq!(range(&store, Some(1500), Some(4500)), [2000, 3000, 4000]);
    }

    #[test]
    fn a_missing_end_means_no_limit() {
        let store = five_records();
        assert_eq!(range(&store, None, None), [1000, 2000, 3000, 4000, 5000]);
        assert_eq!(range(&store, Some(4000), None), [4000, 5000]);
        assert_eq!(range(&store, None, Some(2000)), [1000, 2000]);
    }

    #[test]
    fn a_range_outside_the_data_is_empty_not_an_error() {
        let store = five_records();
        assert!(range(&store, Some(6000), None).is_empty());
        assert!(range(&store, None, Some(999)).is_empty());
        assert!(range(&store, Some(2100), Some(2900)).is_empty());
        assert!(
            range(&store, Some(4000), Some(2000)).is_empty(),
            "backwards"
        );
        assert!(range(&store, Some(u64::MAX), Some(u64::MAX)).is_empty());
        assert_eq!(range(&store, Some(0), Some(u64::MAX)).len(), 5);
    }

    #[test]
    fn ranges_work_after_the_store_has_wrapped() {
        let mut store = store(4, 1);
        for t in 1..=11_u32 {
            store.push(u64::from(t) * 100, &[f64::from(t)]).unwrap();
        }
        // Holds 800, 900, 1000, 1100, now stored out of slot order.
        assert_eq!(range(&store, None, None), [800, 900, 1000, 1100]);
        assert_eq!(range(&store, Some(850), Some(1000)), [900, 1000]);
        assert_eq!(range(&store, Some(1100), None), [1100]);
        assert!(range(&store, None, Some(799)).is_empty());
    }

    #[test]
    fn a_subset_of_columns_comes_back_in_the_order_asked_for() {
        let store = five_records();
        let both = store.snapshot_range(Some(2000), Some(3000), &[1, 0]);
        assert_eq!(both.columns, [vec![20.0, 30.0], vec![2.0, 3.0]]);
        let one = store.snapshot_range(None, None, &[1]);
        assert_eq!(one.columns, [vec![10.0, 20.0, 30.0, 40.0, 50.0]]);
    }

    #[test]
    fn a_column_that_does_not_exist_gives_gaps() {
        let store = five_records();
        let snapshot = store.snapshot_range(None, None, &[0, 7]);
        assert_eq!(snapshot.columns[0].len(), 5);
        assert!(snapshot.columns[1].iter().all(|v| v.is_nan()));
    }

    #[test]
    fn bounds_are_the_first_and_last_records_in_the_range() {
        let store = five_records();
        assert_eq!(store.bounds(None, None), Some((1000, 5000)));
        assert_eq!(store.bounds(Some(1500), Some(4500)), Some((2000, 4000)));
        assert_eq!(store.bounds(Some(6000), None), None);
        assert_eq!(super::tests::store(3, 1).bounds(None, None), None);
    }

    // ---- downsampling -------------------------------------------------------

    /// One column, records every 1000 ms from 0 to 9000 with the value 0..=9.
    fn ten_records() -> Store {
        let mut store = store(20, 1);
        for i in 0..10_u32 {
            store.push(u64::from(i) * 1000, &[f64::from(i)]).unwrap();
        }
        store
    }

    #[test]
    fn buckets_hold_the_mean_of_their_records() {
        let result = ten_records()
            .downsample(None, None, &[0], 5000, false)
            .unwrap();
        assert_eq!(result.step_ms, 5000);
        assert_eq!(result.timestamps, [0, 5000]);
        assert_eq!(result.avg, [vec![2.0, 7.0]]); // mean of 0..=4 and 5..=9
        assert_eq!(result.min, None);
        assert_eq!(result.max, None);
    }

    #[test]
    fn the_smallest_and_largest_are_kept_when_asked_for() {
        let result = ten_records()
            .downsample(None, None, &[0], 5000, true)
            .unwrap();
        assert_eq!(result.min, Some(vec![vec![0.0, 5.0]]));
        assert_eq!(result.max, Some(vec![vec![4.0, 9.0]]));
    }

    #[test]
    fn a_step_of_one_record_reproduces_the_records() {
        let result = ten_records()
            .downsample(None, None, &[0], 1000, true)
            .unwrap();
        let expected: Vec<f64> = (0..10).map(f64::from).collect();
        assert_eq!(result.avg[0], expected);
        assert_eq!(result.min.unwrap()[0], expected);
        assert_eq!(result.max.unwrap()[0], expected);
    }

    #[test]
    fn buckets_are_aligned_to_the_epoch_not_to_the_query() {
        let store = ten_records();
        // Asking from 2500 must not shift the edges: they stay at multiples of 4000.
        let result = store
            .downsample(Some(2500), None, &[0], 4000, false)
            .unwrap();
        assert_eq!(result.timestamps, [0, 4000, 8000]);
        assert_eq!(result.avg[0][0], 3.0); // only the record at 3000 is in 0..4000
        assert_eq!(result.avg[0][1], 5.5); // 4000..=7000
        assert_eq!(result.avg[0][2], 8.5); // 8000 and 9000
    }

    #[test]
    fn a_stretch_with_no_records_is_a_run_of_gaps() {
        let mut store = store(10, 1);
        store.push(0, &[1.0]).unwrap();
        store.push(1000, &[3.0]).unwrap();
        store.push(9000, &[5.0]).unwrap(); // nothing for 8 seconds
        let result = store.downsample(None, None, &[0], 2000, true).unwrap();
        assert_eq!(result.timestamps, [0, 2000, 4000, 6000, 8000]);
        assert_eq!(result.avg[0][0], 2.0);
        assert!(result.avg[0][1..4].iter().all(|v| v.is_nan()));
        assert_eq!(result.avg[0][4], 5.0);
        assert!(result.min.unwrap()[0][2].is_nan());
    }

    #[test]
    fn values_that_are_not_numbers_are_left_out_of_the_summary() {
        let mut store = store(10, 2);
        store.push(0, &[1.0, f64::NAN]).unwrap();
        store.push(1000, &[f64::NAN, f64::NAN]).unwrap();
        store.push(2000, &[3.0, f64::INFINITY]).unwrap();
        let result = store.downsample(None, None, &[0, 1], 4000, true).unwrap();
        assert_eq!(result.avg[0], [2.0]); // mean of 1 and 3, the NaN ignored
        assert_eq!(result.min.as_ref().unwrap()[0], [1.0]);
        assert_eq!(result.max.as_ref().unwrap()[0], [3.0]);
        // The second metric never had a usable value.
        assert!(result.avg[1][0].is_nan());
        assert!(result.min.unwrap()[1][0].is_nan());
    }

    #[test]
    fn several_metrics_are_summarised_together() {
        let store = five_records();
        let result = store
            .downsample(None, None, &[1, 0], 10_000, false)
            .unwrap();
        assert_eq!(result.avg, [vec![30.0], vec![3.0]]);
    }

    #[test]
    fn downsampling_respects_the_range() {
        let result = ten_records()
            .downsample(Some(2000), Some(5000), &[0], 10_000, true)
            .unwrap();
        assert_eq!(result.timestamps, [0]);
        assert_eq!(result.avg[0], [3.5]); // records 2..=5
        assert_eq!(result.min.unwrap()[0], [2.0]);
        assert_eq!(result.max.unwrap()[0], [5.0]);
    }

    #[test]
    fn downsampling_nothing_gives_nothing() {
        let empty = store(4, 1);
        let result = empty.downsample(None, None, &[0], 1000, true).unwrap();
        assert!(result.timestamps.is_empty());
        assert_eq!(result.avg, [Vec::<f64>::new()]);
        assert_eq!(result.min, Some(vec![Vec::new()]));
        let outside = ten_records()
            .downsample(Some(50_000), None, &[0], 1000, false)
            .unwrap();
        assert!(outside.timestamps.is_empty());
    }

    #[test]
    fn a_single_record_is_a_single_bucket() {
        let mut store = store(4, 1);
        store.push(12_345, &[7.0]).unwrap();
        let result = store.downsample(None, None, &[0], 10_000, false).unwrap();
        assert_eq!(result.timestamps, [10_000]);
        assert_eq!(result.avg, [vec![7.0]]);
    }

    #[test]
    fn a_step_of_zero_is_treated_as_one_millisecond() {
        let result = ten_records()
            .downsample(None, None, &[0], 0, false)
            .unwrap();
        assert_eq!(result.step_ms, 1);
    }

    #[test]
    fn downsampling_works_after_the_store_has_wrapped() {
        let mut store = store(4, 1);
        for t in 0..10_u32 {
            store.push(u64::from(t) * 1000, &[f64::from(t)]).unwrap();
        }
        // Holds 6000..=9000.
        let result = store.downsample(None, None, &[0], 2000, false).unwrap();
        assert_eq!(result.timestamps, [6000, 8000]);
        assert_eq!(result.avg[0], [6.5, 8.5]);
    }

    #[test]
    fn too_many_buckets_is_refused_rather_than_allocated() {
        let mut store = store(4, 1);
        store.push(0, &[1.0]).unwrap();
        store.push(1_000_000_000_000, &[2.0]).unwrap(); // ~31 years later
        let error = store.downsample(None, None, &[0], 1, false).unwrap_err();
        assert!(error.buckets > MAX_BUCKETS);
        assert!(error.to_string().contains("max_points"));
        // A step that fits is fine.
        assert!(
            store
                .downsample(None, None, &[0], 10_000_000_000, false)
                .is_ok()
        );
    }

    // ---- choosing a step ----------------------------------------------------

    #[test]
    fn the_step_is_a_whole_number_of_samples() {
        // 1 hour of 5 s samples, into at most 100 points: 36 s is the least
        // that fits, rounded up to whole samples.
        let step = step_for_max_points(0, 3_599_999, 100, 5000);
        assert_eq!(step, 40_000);
        assert_eq!(step % 5000, 0);
    }

    #[test]
    fn enough_points_for_every_record_keeps_the_native_resolution() {
        assert_eq!(step_for_max_points(0, 59_999, 1000, 5000), 5000);
    }

    #[test]
    fn a_single_instant_needs_one_sample_width() {
        assert_eq!(step_for_max_points(12_345, 12_345, 50, 5000), 5000);
    }

    #[test]
    fn two_points_always_fits_even_when_the_data_straddles_an_edge() {
        let step = step_for_max_points(9_990_000, 10_010_000, 2, 5000);
        assert!(bucket_count(9_990_000, 10_010_000, step) <= 2);
    }

    #[test]
    fn the_buckets_never_exceed_max_points() {
        // Many awkward combinations, from a simple deterministic generator.
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move |limit: u64| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) % limit
        };
        for _ in 0..20_000 {
            let interval = [1, 100, 1000, 5000, 60_000][usize::try_from(next(5)).unwrap()];
            let first = 1_700_000_000_000 + next(1_000_000_000);
            let last = first + next(700_000_000);
            let max_points = 2 + next(2000);
            let step = step_for_max_points(first, last, max_points, interval);
            assert_eq!(
                step % interval,
                0,
                "whole samples: {first} {last} {max_points} {interval}"
            );
            assert!(
                bucket_count(first, last, step) <= max_points,
                "{first} {last} {max_points} {interval} -> {step}"
            );
        }
    }

    #[test]
    fn choosing_a_step_ends_even_for_huge_spans() {
        let step = step_for_max_points(0, u64::MAX - 1, 2, 5000);
        assert!(bucket_count(0, u64::MAX - 1, step) <= 2);
    }
    /// Not run by default. `cargo test --release -- --ignored --nocapture`
    /// prints how long queries over a full 7 days (120,960 records of 15
    /// values) take, since the store is locked while they run.
    #[test]
    #[ignore = "a timing check, not a test"]
    #[allow(clippy::cast_precision_loss, clippy::cast_lossless)]
    fn timing_of_queries_over_a_full_week() {
        use std::time::Instant;
        let mut store = store(120_960, 15);
        for i in 0..120_960_u64 {
            let row: Vec<f64> = (0..15).map(|c| (i % 97) as f64 + c as f64).collect();
            store.push(1_700_000_000_000 + i * 5000, &row).unwrap();
        }
        let all: Vec<usize> = (0..15).collect();
        let time = |what: &str, run: &dyn Fn()| {
            let start = Instant::now();
            run();
            println!("{what}: {:?}", start.elapsed());
        };
        time("one metric, whole week, 600 points + extremes", &|| {
            let step = step_for_max_points(
                1_700_000_000_000,
                1_700_000_000_000 + 120_959 * 5000,
                600,
                5000,
            );
            store.downsample(None, None, &[3], step, true).unwrap();
        });
        time("all 15 metrics, whole week, 600 points + extremes", &|| {
            let step = step_for_max_points(
                1_700_000_000_000,
                1_700_000_000_000 + 120_959 * 5000,
                600,
                5000,
            );
            store.downsample(None, None, &all, step, true).unwrap();
        });
        time("one metric, last hour raw (720 records)", &|| {
            let _ = store.snapshot_range(Some(1_700_000_000_000 + 120_240 * 5000), None, &[3]);
        });
        time("one metric, whole week raw", &|| {
            let _ = store.snapshot_range(None, None, &[3]);
        });
    }
}
