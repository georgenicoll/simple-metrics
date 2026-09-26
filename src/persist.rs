//! Keeping the history on disk, so it survives a restart or a reboot.
//!
//! Optional (`--state-dir`): without it nothing is ever written. The file is
//! an append-only log of records, all the same size, so that:
//!
//! - writing is cheap and gentle on an SD card: new records are collected and
//!   appended in one small write every [`crate::config::DEFAULT_FLUSH_INTERVAL`],
//!   not the whole history rewritten;
//! - nothing needs to happen at shutdown, so a power cut or a crash loses at
//!   most the last interval's records;
//! - a torn or damaged tail is found by its size and checksum, and only the
//!   records from there on are lost.
//!
//! When the file has grown to twice the store's capacity it is rewritten
//! from the store (to a temporary file, then renamed over it, so there is
//! always a complete file). That is also how it is repaired after any
//! problem, and how it starts on every run.
//!
//! Layout, all little-endian: a header of the magic bytes, the number of
//! values per record and a hash of the metrics' names (so history from a
//! different set of metrics is never misread); then records of a timestamp,
//! the values as `f64` bits, and a checksum of those.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::metrics::Schema;
use crate::store::Store;

const MAGIC: &[u8; 8] = b"SMHIST\0\x01";
const HEADER_LEN: usize = 8 + 4 + 8;
/// The file's name inside the state directory.
pub const FILE_NAME: &str = "history.bin";

/// A hash of the metrics' names, in order: what makes stored history
/// compatible with the metrics being recorded now.
#[must_use]
pub fn schema_hash(schema: &Schema) -> u64 {
    let mut hash = FNV64_OFFSET;
    for metric in schema.metrics() {
        for byte in metric.name.bytes().chain([0]) {
            hash = (hash ^ u64::from(byte)).wrapping_mul(FNV64_PRIME);
        }
    }
    hash
}

const FNV64_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV64_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A 32-bit FNV-1a checksum: enough to notice a torn or garbled record.
fn checksum(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5_u32, |hash, &byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    })
}

fn header(width: usize, hash: u64) -> [u8; HEADER_LEN] {
    let mut bytes = [0; HEADER_LEN];
    bytes[..8].copy_from_slice(MAGIC);
    bytes[8..12].copy_from_slice(&u32::try_from(width).unwrap_or(u32::MAX).to_le_bytes());
    bytes[12..].copy_from_slice(&hash.to_le_bytes());
    bytes
}

const fn record_len(width: usize) -> usize {
    8 + 8 * width + 4
}

fn encode(out: &mut Vec<u8>, timestamp: u64, row: &[f64]) {
    let start = out.len();
    out.extend_from_slice(&timestamp.to_le_bytes());
    for value in row {
        out.extend_from_slice(&value.to_bits().to_le_bytes());
    }
    let sum = checksum(&out[start..]);
    out.extend_from_slice(&sum.to_le_bytes());
}

/// What loading the history file found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loaded {
    /// There was no file: a first run.
    Nothing,
    /// The file was read.
    Records {
        /// How many records went into the store.
        loaded: usize,
        /// How many were left out as older than wanted or out of order.
        skipped: usize,
        /// Whether the end of the file was torn or damaged (and was ignored).
        damaged_tail: bool,
    },
    /// The file isn't history for these metrics (or isn't history at all), so
    /// it was ignored and will be replaced.
    Incompatible(&'static str),
}

/// Reads the history file at `path` into `store`, leaving out records older
/// than `oldest` (milliseconds since the epoch) and any that aren't in time
/// order. The newest records that fit are kept.
///
/// # Errors
/// If the file exists but can't be read at all. Damage inside it isn't an
/// error: see [`Loaded`].
pub fn load(path: &Path, hash: u64, store: &mut Store, oldest: u64) -> io::Result<Loaded> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Loaded::Nothing),
        Err(error) => return Err(error),
    };
    let mut reader = BufReader::with_capacity(1 << 16, file);
    let width = store.width();

    let mut head = [0; HEADER_LEN];
    if read_full(&mut reader, &mut head)? != HEADER_LEN {
        return Ok(Loaded::Incompatible("the file is shorter than its header"));
    }
    if &head[..8] != MAGIC {
        return Ok(Loaded::Incompatible(
            "it is not a simple-metrics history file",
        ));
    }
    if head != header(width, hash) {
        return Ok(Loaded::Incompatible(
            "it was recorded with a different set of metrics",
        ));
    }

    let mut buffer = vec![0; record_len(width)];
    let mut row = vec![0.0; width];
    let (mut loaded, mut skipped) = (0, 0);
    loop {
        let got = read_full(&mut reader, &mut buffer)?;
        if got == 0 {
            return Ok(Loaded::Records {
                loaded,
                skipped,
                damaged_tail: false,
            });
        }
        let body = buffer.len() - 4;
        let intact =
            got == buffer.len() && buffer[body..] == checksum(&buffer[..body]).to_le_bytes();
        if !intact {
            return Ok(Loaded::Records {
                loaded,
                skipped,
                damaged_tail: true,
            });
        }
        let timestamp = u64::from_le_bytes(buffer[..8].try_into().unwrap_or([0; 8]));
        for (value, bytes) in row.iter_mut().zip(buffer[8..body].chunks_exact(8)) {
            *value = f64::from_bits(u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8])));
        }
        if timestamp >= oldest && store.push(timestamp, &row).is_ok() {
            loaded += 1;
        } else {
            skipped += 1;
        }
    }
}

/// Like `read_exact`, but returns how many bytes it got when the file ends
/// first, rather than an error.
fn read_full<R: Read>(reader: &mut R, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// Writes the history: new records are collected with [`Journal::record`] and
/// written out by [`Journal::flush_if_due`], which also rewrites the whole
/// file when it has grown large or gone wrong.
#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
    hash: u64,
    width: usize,
    capacity: usize,
    flush_every: Duration,
    file: Option<File>,
    /// Encoded records waiting to be appended.
    pending: Vec<u8>,
    /// How many records the file holds.
    in_file: usize,
    /// Set by a failed write: the file can't be trusted to be appended to, so
    /// the next flush rewrites it whole (and nothing is kept in `pending`).
    needs_rewrite: bool,
    failing: bool,
    last_flush: Instant,
}

impl Journal {
    /// Starts a journal at `path` by writing the file afresh from `store`.
    ///
    /// # Errors
    /// If the file can't be written: the state directory is wrong or full.
    pub fn start(
        path: PathBuf,
        hash: u64,
        store: &Store,
        flush_every: Duration,
    ) -> io::Result<Self> {
        let mut journal = Self {
            path,
            hash,
            width: store.width(),
            capacity: store.capacity(),
            flush_every,
            file: None,
            pending: Vec::new(),
            in_file: 0,
            needs_rewrite: false,
            failing: false,
            last_flush: Instant::now(),
        };
        journal.rewrite(store)?;
        Ok(journal)
    }

    /// Notes a record that has just been added to the store, to be written at
    /// the next flush.
    pub fn record(&mut self, timestamp: u64, row: &[f64]) {
        // While a rewrite is due the store is what gets written, so there is
        // nothing to keep (and nothing that can pile up while writes fail).
        if !self.needs_rewrite {
            encode(&mut self.pending, timestamp, row);
        }
    }

    /// Writes what is waiting if the flush interval has passed. A failure is
    /// logged (once, until it clears) and retried next time: the history in
    /// memory carries on regardless.
    pub fn flush_if_due(&mut self, store: &Store) {
        if self.last_flush.elapsed() < self.flush_every {
            return;
        }
        self.last_flush = Instant::now();
        let result = if self.needs_rewrite || self.in_file > 2 * self.capacity {
            self.rewrite(store)
        } else {
            self.append()
        };
        match result {
            Ok(()) => {
                if self.failing {
                    self.failing = false;
                    crate::log(format_args!("history file: writing works again"));
                }
            }
            Err(error) => {
                if !self.failing {
                    self.failing = true;
                    crate::log(format_args!(
                        "history file {}: {error} (will keep trying; the history in memory is unaffected)",
                        self.path.display()
                    ));
                }
                self.needs_rewrite = true;
                self.pending.clear();
            }
        }
    }

    fn append(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let Some(file) = self.file.as_mut() else {
            return Err(io::Error::other("the file is not open"));
        };
        file.write_all(&self.pending)?;
        self.in_file += self.pending.len() / record_len(self.width);
        self.pending.clear();
        Ok(())
    }

    /// Replaces the file with one holding exactly what the store holds now.
    fn rewrite(&mut self, store: &Store) -> io::Result<()> {
        self.file = None;
        let mut temporary = self.path.clone().into_os_string();
        temporary.push(".tmp");
        let temporary = PathBuf::from(temporary);
        let result = self.write_whole(&temporary, store);
        if result.is_err() {
            fs::remove_file(&temporary).ok();
        }
        result?;
        self.file = Some(OpenOptions::new().append(true).open(&self.path)?);
        self.in_file = store.len();
        self.pending.clear();
        self.needs_rewrite = false;
        Ok(())
    }

    fn write_whole(&self, temporary: &Path, store: &Store) -> io::Result<()> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(temporary)?;
        let mut writer = BufWriter::with_capacity(1 << 16, file);
        writer.write_all(&header(self.width, self.hash))?;
        let mut encoded = Vec::with_capacity(record_len(self.width));
        for (timestamp, row) in store.records() {
            encoded.clear();
            encode(&mut encoded, timestamp, row);
            writer.write_all(&encoded)?;
        }
        let file = writer
            .into_inner()
            .map_err(io::IntoInnerError::into_error)?;
        file.sync_all()?;
        fs::rename(temporary, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use super::*;

    const WIDTH: usize = 3;

    fn store(capacity: usize) -> Store {
        Store::new(NonZeroUsize::new(capacity).unwrap(), WIDTH).unwrap()
    }

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sm-persist-{name}-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn row(n: u64) -> [f64; WIDTH] {
        [
            f64::from(u32::try_from(n).unwrap()),
            f64::from(u32::try_from(n).unwrap()) / 2.0,
            f64::NAN,
        ]
    }

    fn filled(capacity: usize, count: u64) -> Store {
        let mut store = store(capacity);
        for n in 1..=count {
            store.push(n * 1000, &row(n)).unwrap();
        }
        store
    }

    fn contents(store: &Store) -> Vec<(u64, Vec<u64>)> {
        store
            .records()
            .map(|(t, r)| (t, r.iter().map(|v| v.to_bits()).collect()))
            .collect()
    }

    fn write_file(path: &Path, source: &Store) -> Journal {
        Journal::start(path.to_path_buf(), 7, source, Duration::ZERO).unwrap()
    }

    #[test]
    fn a_missing_file_is_a_first_run() {
        let path = dir("missing").join(FILE_NAME);
        let mut into = store(4);
        assert_eq!(load(&path, 7, &mut into, 0).unwrap(), Loaded::Nothing);
        assert!(into.is_empty());
    }

    #[test]
    fn what_is_written_is_read_back_exactly_including_gaps() {
        let path = dir("roundtrip").join(FILE_NAME);
        let source = filled(8, 5);
        write_file(&path, &source);
        let mut into = store(8);
        let loaded = load(&path, 7, &mut into, 0).unwrap();
        assert_eq!(
            loaded,
            Loaded::Records {
                loaded: 5,
                skipped: 0,
                damaged_tail: false
            }
        );
        assert_eq!(contents(&into), contents(&source));
        assert!(into.latest().unwrap().1[2].is_nan(), "a gap stays a gap");
    }

    #[test]
    fn appended_records_follow_and_are_read_back() {
        let path = dir("append").join(FILE_NAME);
        let mut source = filled(8, 2);
        let mut journal = write_file(&path, &source);
        for n in 3..=5 {
            source.push(n * 1000, &row(n)).unwrap();
            journal.record(n * 1000, &row(n));
        }
        journal.flush_if_due(&source);
        let mut into = store(8);
        load(&path, 7, &mut into, 0).unwrap();
        assert_eq!(contents(&into), contents(&source));
    }

    #[test]
    fn nothing_is_written_before_the_flush_interval_has_passed() {
        let path = dir("interval").join(FILE_NAME);
        let mut source = filled(8, 1);
        let mut journal =
            Journal::start(path.clone(), 7, &source, Duration::from_secs(3600)).unwrap();
        source.push(2000, &row(2)).unwrap();
        journal.record(2000, &row(2));
        journal.flush_if_due(&source);
        let mut into = store(8);
        load(&path, 7, &mut into, 0).unwrap();
        assert_eq!(into.len(), 1, "the new record is still waiting");
    }

    #[test]
    fn a_small_store_keeps_only_the_newest_records_that_fit() {
        let path = dir("newest").join(FILE_NAME);
        write_file(&path, &filled(8, 8));
        let mut into = store(3);
        load(&path, 7, &mut into, 0).unwrap();
        let times: Vec<u64> = into.records().map(|(t, _)| t).collect();
        assert_eq!(times, [6000, 7000, 8000]);
    }

    #[test]
    fn records_older_than_wanted_are_left_out() {
        let path = dir("old").join(FILE_NAME);
        write_file(&path, &filled(8, 6));
        let mut into = store(8);
        let loaded = load(&path, 7, &mut into, 4000).unwrap();
        assert_eq!(
            loaded,
            Loaded::Records {
                loaded: 3,
                skipped: 3,
                damaged_tail: false
            }
        );
        assert_eq!(into.records().next().unwrap().0, 4000);
    }

    #[test]
    fn a_torn_last_record_costs_only_that_record() {
        let path = dir("torn").join(FILE_NAME);
        write_file(&path, &filled(8, 4));
        let length = fs::metadata(&path).unwrap().len();
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(length - 5).unwrap();
        let mut into = store(8);
        let loaded = load(&path, 7, &mut into, 0).unwrap();
        assert_eq!(
            loaded,
            Loaded::Records {
                loaded: 3,
                skipped: 0,
                damaged_tail: true
            }
        );
    }

    #[test]
    fn a_damaged_record_ends_the_history_there() {
        let path = dir("flipped").join(FILE_NAME);
        write_file(&path, &filled(8, 5));
        let mut bytes = fs::read(&path).unwrap();
        let second = HEADER_LEN + record_len(WIDTH) + 10; // inside record 2
        bytes[second] ^= 0xff;
        fs::write(&path, bytes).unwrap();
        let mut into = store(8);
        let loaded = load(&path, 7, &mut into, 0).unwrap();
        assert_eq!(
            loaded,
            Loaded::Records {
                loaded: 1,
                skipped: 0,
                damaged_tail: true
            }
        );
    }

    #[test]
    fn history_from_other_metrics_is_ignored_not_misread() {
        let path = dir("hash").join(FILE_NAME);
        write_file(&path, &filled(4, 3));
        let mut into = store(4);
        assert!(matches!(
            load(&path, 8, &mut into, 0).unwrap(),
            Loaded::Incompatible(_)
        ));
        assert!(into.is_empty());
        let mut wider = Store::new(NonZeroUsize::new(4).unwrap(), WIDTH + 1).unwrap();
        assert!(matches!(
            load(&path, 7, &mut wider, 0).unwrap(),
            Loaded::Incompatible(_)
        ));
    }

    #[test]
    fn a_file_that_is_not_history_is_ignored() {
        let path = dir("junk").join(FILE_NAME);
        fs::write(&path, "hello, this is not a history file at all").unwrap();
        let mut into = store(4);
        assert!(matches!(
            load(&path, 7, &mut into, 0).unwrap(),
            Loaded::Incompatible(_)
        ));
        fs::write(&path, "tiny").unwrap();
        assert!(matches!(
            load(&path, 7, &mut into, 0).unwrap(),
            Loaded::Incompatible(_)
        ));
    }

    #[test]
    fn records_out_of_time_order_are_skipped() {
        let path = dir("order").join(FILE_NAME);
        let mut bytes = header(WIDTH, 7).to_vec();
        for t in [5000, 6000, 5500, 7000] {
            encode(&mut bytes, t, &row(1));
        }
        fs::write(&path, bytes).unwrap();
        let mut into = store(8);
        let loaded = load(&path, 7, &mut into, 0).unwrap();
        assert_eq!(
            loaded,
            Loaded::Records {
                loaded: 3,
                skipped: 1,
                damaged_tail: false
            }
        );
    }

    #[test]
    fn the_file_is_rewritten_when_it_grows_past_twice_the_capacity() {
        let path = dir("compact").join(FILE_NAME);
        let mut source = store(4);
        let mut journal = write_file(&path, &source);
        for n in 1..=20 {
            source.push(n * 1000, &row(n)).unwrap();
            journal.record(n * 1000, &row(n));
            journal.flush_if_due(&source);
        }
        let size = usize::try_from(fs::metadata(&path).unwrap().len()).unwrap();
        assert!(
            size <= HEADER_LEN + 9 * record_len(WIDTH),
            "the file should stay near twice the capacity, not hold all 20 ({size})"
        );
        let mut into = store(4);
        load(&path, 7, &mut into, 0).unwrap();
        assert_eq!(contents(&into), contents(&source));
    }

    #[test]
    fn starting_repairs_a_damaged_file_and_leaves_no_temporary_one() {
        let path = dir("repair").join(FILE_NAME);
        write_file(&path, &filled(8, 4));
        let length = fs::metadata(&path).unwrap().len();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(length - 3)
            .unwrap();
        let mut recovered = store(8);
        load(&path, 7, &mut recovered, 0).unwrap();
        write_file(&path, &recovered);
        let mut again = store(8);
        assert_eq!(
            load(&path, 7, &mut again, 0).unwrap(),
            Loaded::Records {
                loaded: 3,
                skipped: 0,
                damaged_tail: false
            }
        );
        assert!(!path.with_extension("bin.tmp").exists());
    }

    #[test]
    fn the_file_is_private_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let path = dir("mode").join(FILE_NAME);
        write_file(&path, &filled(2, 1));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn a_failing_write_is_survived_and_the_file_is_rewritten_when_it_clears() {
        let directory = dir("failing");
        let path = directory.join(FILE_NAME);
        let mut source = filled(8, 2);
        let mut journal = write_file(&path, &source);
        // Take the directory away: appends still work on the open file, so
        // break the journal itself the way a failed write would.
        journal.file = None;
        source.push(3000, &row(3)).unwrap();
        journal.record(3000, &row(3));
        journal.flush_if_due(&source); // fails: no file open
        assert!(journal.needs_rewrite && journal.pending.is_empty());
        source.push(4000, &row(4)).unwrap();
        journal.record(4000, &row(4));
        assert!(
            journal.pending.is_empty(),
            "nothing piles up while it is failing"
        );
        journal.flush_if_due(&source); // rewrites whole
        let mut into = store(8);
        load(&path, 7, &mut into, 0).unwrap();
        assert_eq!(contents(&into), contents(&source));
    }

    #[test]
    fn the_schema_hash_depends_on_the_names_and_their_order() {
        let a = schema_hash(&Schema::new(&["eth0".to_owned(), "wg0".to_owned()]));
        let b = schema_hash(&Schema::new(&["wg0".to_owned(), "eth0".to_owned()]));
        let c = schema_hash(&Schema::new(&["eth0".to_owned()]));
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(
            a,
            schema_hash(&Schema::new(&["eth0".to_owned(), "wg0".to_owned()]))
        );
    }
}
