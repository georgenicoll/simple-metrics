//! Command-line settings: what they are, their defaults, and parsing them.

use std::fmt;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

use crate::metrics::{Schema, is_valid_interface};
use crate::store::Store;

/// Where the socket goes unless told otherwise.
pub const DEFAULT_SOCKET: &str = "/run/simple-metrics/simple-metrics.sock";
/// Who may connect unless told otherwise: the owner and group, not everyone.
pub const DEFAULT_SOCKET_MODE: u32 = 0o660;
/// How often to sample unless told otherwise.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);
/// How much history to keep unless told otherwise (7 days).
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// The network interfaces to report on unless told otherwise.
pub const DEFAULT_INTERFACES: [&str; 5] = ["eth0", "wlan0", "wlan1", "br-ap", "wg0"];
/// The most memory the store may be sized to use. A guard against a typo such
/// as `--retention 7000d`, not a target.
pub const MAX_STORE_BYTES: usize = 1 << 30;

/// The help text.
pub const USAGE: &str = "\
Usage: simple-metrics [OPTIONS]

Samples this machine's CPU, memory, temperature and network use at a regular
interval, keeps the most recent records in memory, and serves them over a
Unix socket. Nothing is written to disk.

Options:
      --socket <PATH>       Unix socket to listen on
                            [default: /run/simple-metrics/simple-metrics.sock]
      --socket-mode <MODE>  Permissions for the socket, in octal [default: 660]
      --interval <TIME>     How often to sample [default: 5s]
      --retention <TIME>    How much history to keep [default: 7d]
      --interface <NAME>    A network interface to report on. Repeat for
                            several; giving any replaces the defaults
                            [default: eth0 wlan0 wlan1 br-ap wg0]
      --root <DIR>          Read /proc and /sys under DIR (for testing)
                            [default: /]
  -h, --help                Print this help and exit
  -V, --version             Print the version and exit

A TIME is a whole number with an optional unit: ms, s, m, h or d
(seconds if there is none), for example 500ms, 5s, 90m or 7d.
";

/// The settings the daemon runs with. Every value has been checked, so
/// building one through [`parse`] guarantees they are usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Where to create the Unix socket.
    pub socket: PathBuf,
    /// The socket's permission bits.
    pub socket_mode: u32,
    /// The time between samples.
    pub interval: Duration,
    /// How many records the store holds: the retention divided by the interval.
    pub capacity: NonZeroUsize,
    /// The network interfaces to report on, in order.
    pub interfaces: Vec<String>,
    /// The directory `proc/` and `sys/` are read from.
    pub root: PathBuf,
}

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// Print the help and exit.
    Help,
    /// Print the version and exit.
    Version,
    /// Run the daemon with these settings.
    Run(Config),
}

/// Why the command line couldn't be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// An option this program doesn't have.
    UnknownArgument(String),
    /// An option that needs a value was the last argument.
    MissingValue(String),
    /// An option's value wasn't acceptable.
    InvalidValue {
        /// The option.
        flag: String,
        /// What was given.
        value: String,
        /// What was wrong with it.
        reason: &'static str,
    },
    /// The retention and interval together need more memory than allowed.
    TooLarge {
        /// The memory the store would need, in bytes, if known.
        bytes: Option<usize>,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownArgument(arg) => write!(f, "unknown argument '{arg}'"),
            Self::MissingValue(flag) => write!(f, "{flag} needs a value"),
            Self::InvalidValue {
                flag,
                value,
                reason,
            } => write!(f, "invalid value '{value}' for {flag}: {reason}"),
            Self::TooLarge { bytes: Some(bytes) } => write!(
                f,
                "that retention and interval would need {} MiB for the store (at most {} MiB \
                 is allowed)",
                bytes / (1 << 20),
                MAX_STORE_BYTES / (1 << 20)
            ),
            Self::TooLarge { bytes: None } => {
                write!(f, "that retention and interval are too large")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Parses the command-line arguments (not including the program name).
///
/// # Errors
/// If an argument is unknown, lacks its value, or has an unusable value.
pub fn parse(args: &[String]) -> Result<Parsed, ConfigError> {
    let mut socket = PathBuf::from(DEFAULT_SOCKET);
    let mut socket_mode = DEFAULT_SOCKET_MODE;
    let mut interval = DEFAULT_INTERVAL;
    let mut retention = DEFAULT_RETENTION;
    let mut interfaces: Vec<String> = Vec::new();
    let mut root = PathBuf::from("/");

    let mut args = args.iter();
    while let Some(arg) = args.next() {
        // Both `--flag value` and `--flag=value` are accepted.
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_owned())),
            _ => (arg.as_str(), None),
        };
        let mut value = |flag: &str| {
            inline
                .clone()
                .or_else(|| args.next().cloned())
                .ok_or_else(|| ConfigError::MissingValue(flag.to_owned()))
        };
        let invalid = |value: &str, reason| ConfigError::InvalidValue {
            flag: flag.to_owned(),
            value: value.to_owned(),
            reason,
        };

        match flag {
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            "--socket" => socket = PathBuf::from(value(flag)?),
            "--socket-mode" => {
                let text = value(flag)?;
                socket_mode = u32::from_str_radix(&text, 8)
                    .ok()
                    .filter(|mode| *mode <= 0o777)
                    .ok_or_else(|| invalid(&text, "expected octal permission bits, e.g. 660"))?;
            }
            "--interval" => {
                let text = value(flag)?;
                interval = parse_duration(&text).map_err(|reason| invalid(&text, reason))?;
            }
            "--retention" => {
                let text = value(flag)?;
                retention = parse_duration(&text).map_err(|reason| invalid(&text, reason))?;
            }
            "--interface" => {
                let name = value(flag)?;
                if !is_valid_interface(&name) {
                    return Err(invalid(&name, "not a usable interface name"));
                }
                if interfaces.contains(&name) {
                    return Err(invalid(&name, "given more than once"));
                }
                interfaces.push(name);
            }
            "--root" => root = PathBuf::from(value(flag)?),
            _ => return Err(ConfigError::UnknownArgument(arg.clone())),
        }
    }

    if interfaces.is_empty() {
        interfaces = DEFAULT_INTERFACES.iter().map(ToString::to_string).collect();
    }
    let capacity = capacity_for(retention, interval, interfaces.len())?;
    Ok(Parsed::Run(Config {
        socket,
        socket_mode,
        interval,
        capacity,
        interfaces,
        root,
    }))
}

/// How many records cover `retention` at one per `interval`, provided the
/// store for them isn't too big.
fn capacity_for(
    retention: Duration,
    interval: Duration,
    interface_count: usize,
) -> Result<NonZeroUsize, ConfigError> {
    // Rounded up, and at least one, so a retention shorter than the interval
    // still keeps the latest record.
    let records = retention.as_nanos().div_ceil(interval.as_nanos()).max(1);
    let too_large = ConfigError::TooLarge { bytes: None };
    let capacity = usize::try_from(records).map_err(|_| too_large.clone())?;
    let bytes = Store::bytes_for(capacity, Schema::width_for(interface_count));
    match bytes {
        Some(bytes) if bytes <= MAX_STORE_BYTES => NonZeroUsize::new(capacity).ok_or(too_large),
        _ => Err(ConfigError::TooLarge { bytes }),
    }
}

/// Parses a duration such as `500ms`, `5s`, `90m`, `12h` or `7d`. A bare
/// number is seconds. Whole numbers only, and never zero.
fn parse_duration(text: &str) -> Result<Duration, &'static str> {
    let digits_end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(digits_end);
    let number: u64 = number
        .parse()
        .map_err(|_| "expected a whole number, e.g. 5s")?;
    let millis_per_unit: u64 = match unit {
        "ms" => 1,
        "" | "s" => 1000,
        "m" => 60 * 1000,
        "h" => 60 * 60 * 1000,
        "d" => 24 * 60 * 60 * 1000,
        _ => return Err("the unit must be ms, s, m, h or d"),
    };
    let millis = number
        .checked_mul(millis_per_unit)
        .ok_or("that is too long")?;
    if millis == 0 {
        return Err("must be more than zero");
    }
    Ok(Duration::from_millis(millis))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Parsed, ConfigError> {
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        parse(&args)
    }

    fn config(args: &[&str]) -> Config {
        match parse_args(args) {
            Ok(Parsed::Run(config)) => config,
            other => panic!("expected settings, got {other:?}"),
        }
    }

    fn invalid_flag(args: &[&str]) -> String {
        match parse_args(args) {
            Err(ConfigError::InvalidValue { flag, .. }) => flag,
            other => panic!("expected an invalid value, got {other:?}"),
        }
    }

    #[test]
    fn defaults_are_five_seconds_for_seven_days() {
        let config = config(&[]);
        assert_eq!(config.socket, PathBuf::from(DEFAULT_SOCKET));
        assert_eq!(config.socket_mode, 0o660);
        assert_eq!(config.interval, Duration::from_secs(5));
        assert_eq!(config.capacity.get(), 120_960);
        assert_eq!(config.interfaces, DEFAULT_INTERFACES);
        assert_eq!(config.root, PathBuf::from("/"));
    }

    #[test]
    fn help_and_version_win_wherever_they_appear() {
        assert_eq!(parse_args(&["-h"]), Ok(Parsed::Help));
        assert_eq!(
            parse_args(&["--interval", "1s", "--help"]),
            Ok(Parsed::Help)
        );
        assert_eq!(parse_args(&["--version"]), Ok(Parsed::Version));
        assert_eq!(parse_args(&["-V"]), Ok(Parsed::Version));
    }

    #[test]
    fn values_can_follow_a_space_or_an_equals_sign() {
        let a = config(&["--socket", "/tmp/a.sock", "--interval", "10s"]);
        let b = config(&["--socket=/tmp/a.sock", "--interval=10s"]);
        assert_eq!(a, b);
        assert_eq!(a.socket, PathBuf::from("/tmp/a.sock"));
        assert_eq!(a.interval, Duration::from_secs(10));
    }

    #[test]
    fn a_value_may_itself_contain_an_equals_sign() {
        let config = config(&["--socket=/tmp/a=b.sock"]);
        assert_eq!(config.socket, PathBuf::from("/tmp/a=b.sock"));
    }

    #[test]
    fn capacity_is_retention_over_interval() {
        assert_eq!(
            config(&["--interval", "1s", "--retention", "1h"])
                .capacity
                .get(),
            3600
        );
        assert_eq!(
            config(&["--interval", "500ms", "--retention", "10s"])
                .capacity
                .get(),
            20
        );
    }

    #[test]
    fn capacity_rounds_up_and_is_at_least_one() {
        assert_eq!(
            config(&["--interval", "3s", "--retention", "10s"])
                .capacity
                .get(),
            4
        );
        assert_eq!(
            config(&["--interval", "1h", "--retention", "1s"])
                .capacity
                .get(),
            1
        );
    }

    #[test]
    fn durations_understand_their_units() {
        let cases = [
            ("250ms", 250),
            ("7", 7000),
            ("7s", 7000),
            ("2m", 120_000),
            ("3h", 10_800_000),
            ("1d", 86_400_000),
        ];
        for (text, millis) in cases {
            assert_eq!(
                parse_duration(text),
                Ok(Duration::from_millis(millis)),
                "{text}"
            );
        }
    }

    #[test]
    fn bad_durations_are_refused() {
        for text in [
            "",
            "s",
            "0",
            "0s",
            "-5s",
            "1.5s",
            "5x",
            "5 s",
            "5S",
            "99999999999999999999",
            "18446744073709551615d",
        ] {
            assert!(parse_duration(text).is_err(), "{text:?}");
        }
        assert_eq!(invalid_flag(&["--interval", "soon"]), "--interval");
        assert_eq!(invalid_flag(&["--retention=0"]), "--retention");
    }

    #[test]
    fn giving_an_interface_replaces_the_defaults() {
        let config = config(&["--interface", "eth0", "--interface=wg0"]);
        assert_eq!(config.interfaces, ["eth0", "wg0"]);
    }

    #[test]
    fn bad_and_repeated_interfaces_are_refused() {
        assert_eq!(invalid_flag(&["--interface", "a b"]), "--interface");
        assert_eq!(invalid_flag(&["--interface", "../x"]), "--interface");
        assert_eq!(
            invalid_flag(&["--interface", "eth0", "--interface", "eth0"]),
            "--interface"
        );
    }

    #[test]
    fn the_socket_mode_is_octal_and_at_most_777() {
        assert_eq!(config(&["--socket-mode", "600"]).socket_mode, 0o600);
        assert_eq!(config(&["--socket-mode", "0660"]).socket_mode, 0o660);
        for bad in ["9", "rw", "1000", "-1", ""] {
            assert_eq!(
                invalid_flag(&["--socket-mode", bad]),
                "--socket-mode",
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_store_that_would_be_too_big_is_refused() {
        let result = parse_args(&["--interval", "1ms", "--retention", "365d"]);
        assert!(
            matches!(result, Err(ConfigError::TooLarge { .. })),
            "{result:?}"
        );
        let result = parse_args(&["--interval", "1ms", "--retention", "100000d"]);
        assert!(
            matches!(result, Err(ConfigError::TooLarge { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_missing_value_is_reported_by_flag() {
        for flag in [
            "--socket",
            "--interval",
            "--retention",
            "--interface",
            "--root",
            "--socket-mode",
        ] {
            assert_eq!(
                parse_args(&[flag]),
                Err(ConfigError::MissingValue(flag.to_owned()))
            );
        }
    }

    #[test]
    fn unknown_arguments_are_refused() {
        for arg in ["--bogus", "-x", "positional", "--socket-path=/x"] {
            assert_eq!(
                parse_args(&[arg]),
                Err(ConfigError::UnknownArgument(arg.to_owned()))
            );
        }
    }

    #[test]
    fn errors_read_sensibly() {
        let error = parse_args(&["--interval", "soon"]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid value 'soon' for --interval: expected a whole number, e.g. 5s"
        );
        let error = parse_args(&["--root"]).unwrap_err();
        assert_eq!(error.to_string(), "--root needs a value");
    }
}
