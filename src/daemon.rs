//! Running the collector: sampling on one thread, serving on the other.

use std::convert::Infallible;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::metrics::Schema;
use crate::proc::Readings;
use crate::sampler::Sampler;
use crate::server;
use crate::state::Shared;
use crate::store::Store;

/// Why the daemon couldn't start.
#[derive(Debug)]
pub enum DaemonError {
    /// The store would be too big to allocate.
    TooLarge,
    /// Something is already listening on the socket's path.
    AlreadyRunning(PathBuf),
    /// The socket's path exists and isn't a socket, so it isn't ours to replace.
    NotASocket(PathBuf),
    /// Setting up the socket failed.
    Socket {
        /// The socket's path.
        path: PathBuf,
        /// What went wrong.
        source: io::Error,
    },
    /// Starting the sampler thread failed.
    Thread(io::Error),
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => write!(f, "the store is too large to allocate"),
            Self::AlreadyRunning(path) => {
                write!(
                    f,
                    "another process is already listening on {}",
                    path.display()
                )
            }
            Self::NotASocket(path) => write!(
                f,
                "{} exists and is not a socket, so it has not been replaced",
                path.display()
            ),
            Self::Socket { path, source } => {
                write!(f, "cannot set up the socket {}: {source}", path.display())
            }
            Self::Thread(source) => write!(f, "cannot start the sampler thread: {source}"),
        }
    }
}

impl std::error::Error for DaemonError {}

/// Starts sampling and serving. Once started this never returns; it only
/// returns an error, when it can't start.
///
/// # Errors
/// If the store can't be allocated, the socket can't be set up, or the
/// sampler thread can't be started.
pub fn run(config: &Config) -> Result<Infallible, DaemonError> {
    let schema = Schema::new(&config.interfaces);
    let store = Store::new(config.capacity, schema.len()).ok_or(DaemonError::TooLarge)?;
    let shared = Arc::new(Shared::new(schema, config.interval, store));

    let socket_error = |source| DaemonError::Socket {
        path: config.socket.clone(),
        source,
    };
    prepare_socket_path(&config.socket)?;
    let listener = UnixListener::bind(&config.socket).map_err(socket_error)?;
    // The permissions are set after binding, so there's a moment when the
    // socket has whatever the process's umask allowed. The directory it's in
    // (created by systemd with its own mode) is what keeps others out then.
    fs::set_permissions(
        &config.socket,
        fs::Permissions::from_mode(config.socket_mode),
    )
    .map_err(socket_error)?;

    let sampling = SamplingPlan {
        interfaces: config.interfaces.clone(),
        interval: config.interval,
        root: config.root.clone(),
    };
    let sampler_shared = Arc::clone(&shared);
    thread::Builder::new()
        .name("sampler".to_owned())
        .spawn(move || sample_forever(&sampler_shared, &sampling))
        .map_err(DaemonError::Thread)?;

    crate::log(format_args!(
        "simple-metrics {} listening on {}: sampling every {:?}, keeping {} records (up to {} MiB)",
        crate::VERSION,
        config.socket.display(),
        config.interval,
        config.capacity,
        Store::bytes_for(config.capacity.get(), shared.schema.len()).unwrap_or(0) >> 20,
    ));
    server::serve(&listener, &shared)
}

/// Makes way for a fresh socket at `path`: removes a stale one left by an
/// earlier run, but never a live one, and never something that isn't a socket.
fn prepare_socket_path(path: &Path) -> Result<(), DaemonError> {
    let socket_error = |source| DaemonError::Socket {
        path: path.to_path_buf(),
        source,
    };
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            if UnixStream::connect(path).is_ok() {
                return Err(DaemonError::AlreadyRunning(path.to_path_buf()));
            }
            fs::remove_file(path).map_err(socket_error)
        }
        Ok(_) => Err(DaemonError::NotASocket(path.to_path_buf())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(socket_error(error)),
    }
}

/// What the sampler thread needs to know.
struct SamplingPlan {
    interfaces: Vec<String>,
    interval: Duration,
    root: PathBuf,
}

/// Takes a sample now and then every `interval`, for ever.
fn sample_forever(shared: &Shared, plan: &SamplingPlan) -> ! {
    let mut sampler = Sampler::new(plan.interfaces.clone());
    let mut last = Instant::now();
    let mut next = last;
    loop {
        let now = Instant::now();
        let elapsed = now.duration_since(last).as_secs_f64();
        last = now;

        let row = sampler.sample(&Readings::read(&plan.root), elapsed);
        {
            let mut store = shared.write();
            // The clock can step (a Pi with no battery clock jumps forward
            // when it first reaches the network), and records must stay in
            // time order, so a timestamp is never earlier than the last.
            let timestamp = store
                .latest()
                .map_or(unix_millis(), |(newest, _)| unix_millis().max(newest + 1));
            if let Err(error) = store.push(timestamp, &row) {
                crate::log(format_args!("could not record a sample: {error}"));
            }
        }

        // Aim for evenly spaced samples rather than one interval after the
        // last finished, so the reading time doesn't add up as drift. If we
        // fell behind (the machine was suspended), start again from now.
        next += plan.interval;
        let after = Instant::now();
        if next <= after {
            next = after + plan.interval;
        }
        thread::sleep(next - after);
    }
}

/// Milliseconds since the Unix epoch (0 if the clock is before it).
fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("simple-metrics-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_path_is_fine() {
        let dir = temp_dir("missing");
        assert!(prepare_socket_path(&dir.join("s.sock")).is_ok());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_stale_socket_is_replaced() {
        let dir = temp_dir("stale");
        let path = dir.join("s.sock");
        drop(UnixListener::bind(&path).unwrap()); // leaves the file behind
        assert!(path.exists());
        prepare_socket_path(&path).unwrap();
        assert!(!path.exists());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_live_socket_is_left_alone() {
        let dir = temp_dir("live");
        let path = dir.join("s.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        assert!(matches!(
            prepare_socket_path(&path),
            Err(DaemonError::AlreadyRunning(_))
        ));
        assert!(path.exists());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn something_that_is_not_a_socket_is_never_removed() {
        let dir = temp_dir("file");
        let path = dir.join("precious.txt");
        fs::write(&path, "keep me").unwrap();
        assert!(matches!(
            prepare_socket_path(&path),
            Err(DaemonError::NotASocket(_))
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), "keep me");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_symlink_is_not_followed_and_not_removed() {
        let dir = temp_dir("symlink");
        let target = dir.join("target.txt");
        fs::write(&target, "keep me").unwrap();
        let link = dir.join("s.sock");
        symlink(&target, &link).unwrap();
        assert!(matches!(
            prepare_socket_path(&link),
            Err(DaemonError::NotASocket(_))
        ));
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep me");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn the_clock_reads_a_plausible_time() {
        // After 2024-01-01, in milliseconds.
        assert!(unix_millis() > 1_704_067_200_000);
    }
}
