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
pub const DEFAULT_RETENTION: Duration = Duration::from_hours(168);
/// The network interfaces to report on unless told otherwise.
pub const DEFAULT_INTERFACES: [&str; 5] = ["eth0", "wlan0", "wlan1", "br-ap", "wg0"];
/// What `--interface` is given to mean "every interface the machine has".
pub const ALL_INTERFACES: &str = "all";
/// The most network interfaces that will be charted: each costs about 2 MB
/// (at the default interval and retention), so a machine with hundreds of
/// virtual ones must be asked to skip them (`--ignore-interface`).
pub const MAX_INTERFACES: usize = 64;
/// How often new records are written to the history file, if there is one.
pub const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_secs(60);
/// The most memory the store may be sized to use. A guard against a typo such
/// as `--retention 7000d`, not a target.
pub const MAX_STORE_BYTES: usize = 1 << 30;

/// The help text.
pub const USAGE: &str = "\
Usage: simple-metrics [OPTIONS]

Samples this machine's CPU, memory, temperature and network use at a regular
interval, keeps the most recent records in memory, and serves them over a
Unix socket. Nothing is written to disk unless --state-dir is given.

Options:
      --socket <PATH>       Unix socket to listen on
                            [default: /run/simple-metrics/simple-metrics.sock]
      --socket-mode <MODE>  Permissions for the socket, in octal [default: 660]
      --interval <TIME>     How often to sample [default: 5s]
      --retention <TIME>    How much history to keep [default: 7d]
      --interface <NAME>    A network interface to report on. Repeat for
                            several; giving any replaces the defaults. `all`
                            means every interface the machine has when the
                            daemon starts, except lo (see --ignore-interface)
                            [default: eth0 wlan0 wlan1 br-ap wg0]
      --ignore-interface <PREFIX>
                            With `--interface all`: skip the interfaces whose
                            names start with PREFIX, e.g. veth for the
                            virtual ones Docker makes. Repeat for several.
                            An interface named with --interface is kept
      --state-dir <DIR>     Keep the history in DIR too, so it survives a
                            restart or reboot (created if need be; the file
                            is history.bin, readable only by this user)
                            [default: none: memory only]
      --flush-interval <TIME>
                            How often new records are written there
                            [default: 1m]
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
    /// How much history is kept.
    pub retention: Duration,
    /// How many records the store holds: the retention divided by the interval.
    pub capacity: NonZeroUsize,
    /// Where to keep the history on disk as well, if anywhere.
    pub state_dir: Option<PathBuf>,
    /// How often new records are written to the history file.
    pub flush_interval: Duration,
    /// The network interfaces named on the command line, in order.
    pub interfaces: Vec<String>,
    /// Whether to report on every interface the machine has as well (see
    /// [`Config::resolve_interfaces`], which does it).
    pub all_interfaces: bool,
    /// With `all_interfaces`: the name prefixes of interfaces to leave out.
    pub ignore_interfaces: Vec<String>,
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
    /// `--interface all` found more interfaces than are charted.
    TooManyInterfaces {
        /// How many there were.
        count: usize,
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
            Self::TooManyInterfaces { count } => write!(
                f,
                "--interface all found {count} network interfaces (at most {MAX_INTERFACES} are \
                 charted); skip some with --ignore-interface <PREFIX>"
            ),
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
    let mut chosen = InterfaceArgs::default();
    let mut root = PathBuf::from("/");
    let mut state_dir = None;
    let mut flush_interval = DEFAULT_FLUSH_INTERVAL;

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
                chosen
                    .add(name.clone())
                    .map_err(|reason| invalid(&name, reason))?;
            }
            "--ignore-interface" => {
                let prefix = value(flag)?;
                chosen
                    .ignore(prefix.clone())
                    .map_err(|reason| invalid(&prefix, reason))?;
            }
            "--state-dir" => state_dir = Some(PathBuf::from(value(flag)?)),
            "--flush-interval" => {
                let text = value(flag)?;
                flush_interval = parse_duration(&text).map_err(|reason| invalid(&text, reason))?;
            }
            "--root" => root = PathBuf::from(value(flag)?),
            _ => return Err(ConfigError::UnknownArgument(arg.clone())),
        }
    }

    let InterfaceArgs {
        mut interfaces,
        all: all_interfaces,
        ignore: ignore_interfaces,
    } = chosen;
    if let Some(prefix) = ignore_interfaces.first().filter(|_| !all_interfaces) {
        return Err(ConfigError::InvalidValue {
            flag: "--ignore-interface".to_owned(),
            value: prefix.clone(),
            reason: "only applies together with --interface all",
        });
    }
    if interfaces.is_empty() && !all_interfaces {
        interfaces = DEFAULT_INTERFACES.iter().map(ToString::to_string).collect();
    }
    let capacity = capacity_for(retention, interval, interfaces.len())?;
    Ok(Parsed::Run(Config {
        socket,
        socket_mode,
        interval,
        retention,
        capacity,
        state_dir,
        flush_interval,
        interfaces,
        all_interfaces,
        ignore_interfaces,
        root,
    }))
}

/// The `--interface` and `--ignore-interface` options as they are given.
#[derive(Default)]
struct InterfaceArgs {
    interfaces: Vec<String>,
    all: bool,
    ignore: Vec<String>,
}

impl InterfaceArgs {
    /// An `--interface`: a name, or `all`.
    fn add(&mut self, name: String) -> Result<(), &'static str> {
        if name == ALL_INTERFACES {
            return if std::mem::replace(&mut self.all, true) {
                Err("given more than once")
            } else {
                Ok(())
            };
        }
        if !is_valid_interface(&name) {
            return Err("not a usable interface name");
        }
        if self.interfaces.contains(&name) {
            return Err("given more than once");
        }
        self.interfaces.push(name);
        Ok(())
    }

    /// An `--ignore-interface`: the start of a name.
    fn ignore(&mut self, prefix: String) -> Result<(), &'static str> {
        if !is_valid_interface(&prefix) {
            return Err("not a usable interface name prefix");
        }
        if self.ignore.contains(&prefix) {
            return Err("given more than once");
        }
        self.ignore.push(prefix);
        Ok(())
    }
}

impl Config {
    /// Works out the final list of interfaces. Unless `--interface all` was
    /// given this changes nothing. With it, the list is every one of
    /// `available` (the machine's interfaces) except the loopback `lo` and
    /// any whose name starts with an ignored prefix, in name order, followed
    /// by any named on the command line that aren't there yet (so naming
    /// one keeps it even if its prefix is ignored, or it isn't up yet).
    ///
    /// The set is fixed from here on: an interface that appears later isn't
    /// charted until the daemon restarts.
    ///
    /// # Errors
    /// If there are too many interfaces, or the store for them would be too
    /// big.
    pub fn resolve_interfaces(mut self, available: &[String]) -> Result<Self, ConfigError> {
        if !self.all_interfaces {
            return Ok(self);
        }
        let mut found: Vec<String> = available
            .iter()
            .filter(|name| {
                *name != "lo"
                    && is_valid_interface(name)
                    && !self
                        .ignore_interfaces
                        .iter()
                        .any(|prefix| name.starts_with(prefix.as_str()))
            })
            .cloned()
            .collect();
        found.sort();
        found.dedup();
        for named in &self.interfaces {
            if !found.contains(named) {
                found.push(named.clone());
            }
        }
        if found.len() > MAX_INTERFACES {
            return Err(ConfigError::TooManyInterfaces { count: found.len() });
        }
        self.capacity = capacity_for(self.retention, self.interval, found.len())?;
        self.interfaces = found;
        self.all_interfaces = false;
        Ok(self)
    }
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
pub(crate) fn parse_duration(text: &str) -> Result<Duration, &'static str> {
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
        assert_eq!(config.retention, DEFAULT_RETENTION);
        assert_eq!(config.state_dir, None, "memory only unless asked");
        assert_eq!(config.flush_interval, Duration::from_secs(60));
    }

    #[test]
    fn a_state_directory_and_flush_interval_can_be_given() {
        let config = config(&["--state-dir", "/var/lib/x", "--flush-interval", "30s"]);
        assert_eq!(config.state_dir, Some(PathBuf::from("/var/lib/x")));
        assert_eq!(config.flush_interval, Duration::from_secs(30));
    }

    #[test]
    fn a_bad_flush_interval_names_the_option() {
        assert_eq!(
            invalid_flag(&["--flush-interval", "soon"]),
            "--flush-interval"
        );
        assert_eq!(invalid_flag(&["--flush-interval", "0"]), "--flush-interval");
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

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn all_interfaces_is_asked_for_by_name_and_is_not_a_name() {
        let config = config(&["--interface", "all"]);
        assert!(config.all_interfaces);
        assert!(
            config.interfaces.is_empty(),
            "no defaults when asking for all"
        );
        assert_eq!(
            invalid_flag(&["--interface", "all", "--interface", "all"]),
            "--interface"
        );
    }

    #[test]
    fn resolving_all_takes_every_interface_but_lo_in_name_order() {
        let resolved = config(&["--interface", "all"])
            .resolve_interfaces(&names(&["wg0", "lo", "eth0", "br-ap"]))
            .unwrap();
        assert_eq!(resolved.interfaces, ["br-ap", "eth0", "wg0"]);
        assert!(
            !resolved.all_interfaces,
            "resolving twice changes nothing more"
        );
    }

    #[test]
    fn ignored_prefixes_are_left_out_but_named_interfaces_are_kept() {
        let resolved = config(&[
            "--interface",
            "all",
            "--ignore-interface",
            "veth",
            "--ignore-interface",
            "docker",
            "--interface",
            "veth-keep",
            "--interface",
            "not-up-yet",
        ])
        .resolve_interfaces(&names(&["eth0", "veth1a2b", "docker0", "veth-keep"]))
        .unwrap();
        assert_eq!(resolved.interfaces, ["eth0", "veth-keep", "not-up-yet"]);
    }

    #[test]
    fn an_ignore_without_all_is_refused_and_a_bad_prefix_too() {
        assert_eq!(
            invalid_flag(&["--ignore-interface", "veth"]),
            "--ignore-interface"
        );
        assert_eq!(
            invalid_flag(&["--interface", "all", "--ignore-interface", "a b"]),
            "--ignore-interface"
        );
        assert_eq!(
            invalid_flag(&[
                "--interface",
                "all",
                "--ignore-interface",
                "x",
                "--ignore-interface",
                "x"
            ]),
            "--ignore-interface"
        );
    }

    #[test]
    fn without_all_resolving_changes_nothing() {
        let before = config(&["--interface", "eth0"]);
        let after = before
            .clone()
            .resolve_interfaces(&names(&["eth0", "wg0"]))
            .unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn too_many_interfaces_is_an_error_that_says_what_to_do() {
        let many: Vec<String> = (0..=MAX_INTERFACES).map(|n| format!("veth{n}")).collect();
        let error = config(&["--interface", "all"])
            .resolve_interfaces(&many)
            .unwrap_err();
        assert_eq!(
            error,
            ConfigError::TooManyInterfaces {
                count: MAX_INTERFACES + 1
            }
        );
        assert!(error.to_string().contains("--ignore-interface"));
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
