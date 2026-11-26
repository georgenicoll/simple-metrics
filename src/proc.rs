//! Reading and parsing the kernel's files under `/proc` and `/sys`.
//!
//! The parsers take the file's text and return `None` when it isn't in the
//! expected shape, so a missing or odd file becomes a gap in the data rather
//! than a crash. Reading the files themselves is in [`Readings::read`].

use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Cumulative CPU time from the first line of `/proc/stat`, in clock ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    /// Time spent doing work (everything but idle and waiting for I/O).
    pub busy: u64,
    /// All time, busy or not.
    pub total: u64,
}

/// Memory figures from `/proc/meminfo`, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Memory {
    /// Memory in use by programs, not counting reclaimable cache.
    pub used: u64,
    /// Total physical memory.
    pub total: u64,
    /// Swap space in use.
    pub swap_used: u64,
    /// Total swap space (zero when the machine has none).
    pub swap_total: u64,
}

/// One interface's cumulative byte counters from `/proc/net/dev`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetCounters {
    /// Bytes received since the interface came up.
    pub rx_bytes: u64,
    /// Bytes sent since the interface came up.
    pub tx_bytes: u64,
}

/// One block device's cumulative counters from `/proc/diskstats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskCounters {
    /// Bytes read since boot.
    pub read_bytes: u64,
    /// Bytes written since boot.
    pub write_bytes: u64,
    /// Milliseconds with at least one request in progress, since boot. `None`
    /// for a device that has done I/O without ever counting any (software
    /// RAID), where zero would be misleading.
    pub busy_millis: Option<u64>,
}

/// `/proc/diskstats` counts in sectors of this size, whatever the device's is.
const SECTOR_BYTES: u64 = 512;

/// Everything read from the machine in one pass. A field is `None` (or an
/// interface or disk absent) when it couldn't be read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Readings {
    /// CPU time counters.
    pub cpu: Option<CpuTimes>,
    /// The 1-minute load average.
    pub load1: Option<f64>,
    /// Memory and swap use.
    pub memory: Option<Memory>,
    /// CPU temperature in degrees Celsius.
    pub temperature: Option<f64>,
    /// Byte counters by interface name.
    pub net: HashMap<String, NetCounters>,
    /// I/O counters by block device name.
    pub disks: HashMap<String, DiskCounters>,
}

impl Readings {
    /// Reads everything from the files under `root` (`/` on a real machine).
    /// Anything that can't be read is left empty.
    #[must_use]
    pub fn read(root: &Path) -> Self {
        let read = |path: &str| fs::read_to_string(root.join(path)).ok();
        Self {
            cpu: read("proc/stat").as_deref().and_then(parse_cpu_times),
            load1: read("proc/loadavg").as_deref().and_then(parse_load1),
            memory: read("proc/meminfo").as_deref().and_then(parse_memory),
            temperature: read("sys/class/thermal/thermal_zone0/temp")
                .as_deref()
                .and_then(parse_temperature),
            net: read("proc/net/dev")
                .as_deref()
                .map_or_default(parse_net_dev),
            disks: read("proc/diskstats")
                .as_deref()
                .map_or_default(parse_diskstats),
        }
    }
}

/// The names of the network interfaces the machine has, from
/// `proc/net/dev` under `root`, in name order. Empty if that can't be read.
#[must_use]
pub fn interface_names(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_to_string(root.join("proc/net/dev"))
        .map_or_default(|text| parse_net_dev(&text).into_keys().collect());
    names.sort();
    names
}

/// The names of the machine's physical disks, from `sys/block` under `root`,
/// in name order. Empty if that can't be read.
///
/// Physical means backed by a device, which leaves out `loop`, `ram`, `zram`,
/// `nbd`, `md` and `dm` devices. Also left out: optical and floppy drives, an
/// eMMC's boot areas, and anything with no capacity (a card reader with no
/// card in it).
#[must_use]
pub fn physical_disk_names(root: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(root.join("sys/block")) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let path = entry.path();
            let unwanted = name.starts_with("sr")
                || name.starts_with("fd")
                || (name.starts_with("mmcblk") && name.contains("boot"));
            let has_capacity = fs::read_to_string(path.join("size"))
                .is_ok_and(|size| size.trim().parse::<u64>().is_ok_and(|sectors| sectors > 0));
            (!unwanted && has_capacity && path.join("device").exists()).then_some(name)
        })
        .collect();
    names.sort();
    names
}

/// Parses the aggregate `cpu` line of `/proc/stat`:
/// `cpu user nice system idle iowait irq softirq steal guest guest_nice`.
///
/// `guest` time is already counted inside `user`, so only the first eight
/// fields are summed. Idle and iowait both count as not busy.
#[must_use]
pub fn parse_cpu_times(stat: &str) -> Option<CpuTimes> {
    let line = stat
        .lines()
        .find(|l| l.split_whitespace().next() == Some("cpu"))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    // idle is field 4 and iowait field 5; older kernels lack the later ones.
    let [_, _, _, idle, ..] = fields[..] else {
        return None;
    };
    let iowait = fields.get(4).copied().unwrap_or(0);
    let total: u64 = fields.iter().sum();
    Some(CpuTimes {
        busy: total.saturating_sub(idle).saturating_sub(iowait),
        total,
    })
}

/// Parses `/proc/loadavg` (`0.07 0.02 0.00 1/307 3187`) for the 1-minute load.
#[must_use]
pub fn parse_load1(loadavg: &str) -> Option<f64> {
    let load: f64 = loadavg.split_whitespace().next()?.parse().ok()?;
    load.is_finite().then_some(load)
}

/// Parses `/proc/meminfo`. "Used" is `MemTotal - MemAvailable`, not
/// `MemTotal - MemFree`: Linux keeps spare memory as cache, so "free" is
/// always small, while "available" is what programs could actually get.
#[must_use]
pub fn parse_memory(meminfo: &str) -> Option<Memory> {
    let kib = |key: &str| -> Option<u64> {
        let line = meminfo
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))?;
        line.split_whitespace().next()?.parse().ok()
    };
    let total = kib("MemTotal")?;
    let available = kib("MemAvailable")?;
    let swap_total = kib("SwapTotal")?;
    let swap_free = kib("SwapFree")?;
    Some(Memory {
        used: total.saturating_sub(available).saturating_mul(1024),
        total: total.saturating_mul(1024),
        swap_used: swap_total.saturating_sub(swap_free).saturating_mul(1024),
        swap_total: swap_total.saturating_mul(1024),
    })
}

/// Parses a thermal zone's `temp` file, which holds millidegrees Celsius.
#[must_use]
#[allow(clippy::cast_precision_loss)] // a temperature is far below 2^53
pub fn parse_temperature(temp: &str) -> Option<f64> {
    let milli: i64 = temp.trim().parse().ok()?;
    Some(milli as f64 / 1000.0)
}

/// Parses `/proc/net/dev`. Each interface line is `name: ` followed by eight
/// receive fields (bytes first) and eight transmit fields (bytes first).
/// Lines that don't fit that shape, such as the two header lines, are skipped.
#[must_use]
pub fn parse_net_dev(net_dev: &str) -> HashMap<String, NetCounters> {
    net_dev
        .lines()
        .filter_map(|line| {
            let (name, counters) = line.split_once(':')?;
            let fields: Vec<u64> = counters
                .split_whitespace()
                .map(str::parse)
                .collect::<Result<_, _>>()
                .ok()?;
            let [rx_bytes, _, _, _, _, _, _, _, tx_bytes, ..] = fields[..] else {
                return None;
            };
            Some((name.trim().to_owned(), NetCounters { rx_bytes, tx_bytes }))
        })
        .collect()
}

/// Parses `/proc/diskstats`. Each line is the device's major and minor
/// numbers and name, then its counters, of which the third is sectors read,
/// the seventh sectors written and the tenth milliseconds busy. Lines that
/// don't fit that shape are skipped.
#[must_use]
pub fn parse_diskstats(diskstats: &str) -> HashMap<String, DiskCounters> {
    diskstats
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace().skip(2);
            let name = parts.next()?;
            let fields: Vec<u64> = parts.map(str::parse).collect::<Result<_, _>>().ok()?;
            let [
                reads,
                _,
                sectors_read,
                _,
                writes,
                _,
                sectors_written,
                _,
                _,
                busy,
                ..,
            ] = fields[..]
            else {
                return None;
            };
            let accounted = busy > 0 || (reads == 0 && writes == 0);
            Some((
                name.to_owned(),
                DiskCounters {
                    read_bytes: sectors_read.saturating_mul(SECTOR_BYTES),
                    write_bytes: sectors_written.saturating_mul(SECTOR_BYTES),
                    busy_millis: accounted.then_some(busy),
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real files from a Raspberry Pi 5 running Raspberry Pi OS.
    const STAT: &str = include_str!("../tests/fixtures/root/proc/stat");
    const LOADAVG: &str = include_str!("../tests/fixtures/root/proc/loadavg");
    const MEMINFO: &str = include_str!("../tests/fixtures/root/proc/meminfo");
    const NET_DEV: &str = include_str!("../tests/fixtures/root/proc/net/dev");
    const DISKSTATS: &str = include_str!("../tests/fixtures/root/proc/diskstats");

    #[test]
    fn the_interface_names_are_listed_in_order() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/root");
        assert_eq!(
            interface_names(&root),
            ["br-ap", "eth0", "lo", "wg0", "wlan0", "wlan1"]
        );
        assert_eq!(
            interface_names(Path::new("/nonexistent")),
            Vec::<String>::new()
        );
    }
    const TEMP: &str = include_str!("../tests/fixtures/root/sys/class/thermal/thermal_zone0/temp");

    #[test]
    fn cpu_times_from_a_real_stat_file() {
        // cpu  5092 0 3387 2312719 1076 0 104 0 0 0
        let times = parse_cpu_times(STAT).unwrap();
        let total = 5092 + 3387 + 2_312_719 + 1076 + 104;
        assert_eq!(times.total, total);
        assert_eq!(times.busy, 5092 + 3387 + 104);
    }

    #[test]
    fn cpu_times_ignore_the_per_core_lines() {
        // "cpu0 ..." must not be mistaken for the aggregate "cpu ..." line.
        let per_core_first = "cpu0 1 1 1 1 1 1 1 1\ncpu  10 0 10 80 0 0 0 0\n";
        let times = parse_cpu_times(per_core_first).unwrap();
        assert_eq!((times.busy, times.total), (20, 100));
    }

    #[test]
    fn cpu_times_do_not_count_guest_time_twice() {
        // guest (9th) and guest_nice (10th) are included in user and nice.
        let times = parse_cpu_times("cpu  100 0 0 100 0 0 0 0 50 25\n").unwrap();
        assert_eq!(times.total, 200);
    }

    #[test]
    fn cpu_times_tolerate_an_old_kernel_with_fewer_fields() {
        let times = parse_cpu_times("cpu  10 0 10 80\n").unwrap();
        assert_eq!((times.busy, times.total), (20, 100));
    }

    #[test]
    fn cpu_times_reject_garbage() {
        for text in [
            "",
            "cpu\n",
            "cpu  a b c d\n",
            "intr 1 2 3\n",
            "cpu  1 2 3\n",
        ] {
            assert_eq!(parse_cpu_times(text), None, "{text:?}");
        }
    }

    #[test]
    fn load_from_a_real_loadavg_file() {
        assert_eq!(parse_load1(LOADAVG), Some(0.07));
    }

    #[test]
    fn load_rejects_garbage() {
        for text in ["", "abc 1 2", "NaN 0 0", "inf 0 0"] {
            assert_eq!(parse_load1(text), None, "{text:?}");
        }
    }

    #[test]
    fn memory_from_a_real_meminfo_file() {
        let memory = parse_memory(MEMINFO).unwrap();
        // MemTotal 3886840 kB, MemAvailable 3482432 kB, swap entirely free.
        assert_eq!(memory.used, (3_886_840 - 3_482_432) * 1024);
        assert_eq!(memory.swap_used, 0);
    }

    #[test]
    fn memory_counts_swap_in_use() {
        let text = "MemTotal: 1000 kB\nMemAvailable: 400 kB\nSwapTotal: 500 kB\nSwapFree: 200 kB\n";
        let memory = parse_memory(text).unwrap();
        assert_eq!(memory.used, 600 * 1024);
        assert_eq!(memory.swap_used, 300 * 1024);
        assert_eq!(memory.total, 1000 * 1024);
        assert_eq!(memory.swap_total, 500 * 1024);
    }

    #[test]
    fn memory_key_match_is_exact() {
        // "SwapCached" must not satisfy a lookup for "Swap...", nor "MemFree"
        // one for "MemTotal".
        let text = "MemFree: 5 kB\nSwapCached: 1 kB\n";
        assert_eq!(parse_memory(text), None);
    }

    #[test]
    fn memory_needs_every_field() {
        let text = "MemTotal: 1000 kB\nMemAvailable: 400 kB\nSwapTotal: 500 kB\n";
        assert_eq!(parse_memory(text), None);
        assert_eq!(parse_memory(""), None);
    }

    #[test]
    fn temperature_from_a_real_file() {
        assert_eq!(parse_temperature(TEMP), Some(48.686));
    }

    #[test]
    fn temperature_can_be_below_zero_and_rejects_garbage() {
        assert_eq!(parse_temperature("-5500\n"), Some(-5.5));
        assert_eq!(parse_temperature("warm"), None);
        assert_eq!(parse_temperature(""), None);
    }

    #[test]
    fn net_dev_from_a_real_file() {
        let net = parse_net_dev(NET_DEV);
        assert_eq!(net.len(), 6); // lo, eth0, wlan0, wlan1, br-ap, wg0
        assert_eq!(
            net["eth0"],
            NetCounters {
                rx_bytes: 2_773_361,
                tx_bytes: 488_920
            }
        );
        assert_eq!(net["wg0"].tx_bytes, 162_504);
        assert_eq!(net["wlan0"].rx_bytes, 0);
    }

    #[test]
    fn net_dev_skips_headers_and_malformed_lines() {
        let text = "Inter-|   Receive\n face |bytes packets\n  eth0: 1 2 3\n  ok: 1 0 0 0 0 0 0 0 9 0 0 0 0 0 0 0\n";
        let net = parse_net_dev(text);
        assert_eq!(net.len(), 1);
        assert_eq!(net["ok"].tx_bytes, 9);
    }

    #[test]
    fn net_dev_handles_a_name_with_no_space_before_the_counters() {
        // Long interface names run into the first counter: "enp0s31f6:123 ...".
        let text = "enp0s31f6:123 0 0 0 0 0 0 0 456 0 0 0 0 0 0 0\n";
        let net = parse_net_dev(text);
        assert_eq!(
            (net["enp0s31f6"].rx_bytes, net["enp0s31f6"].tx_bytes),
            (123, 456)
        );
    }

    #[test]
    fn diskstats_counts_bytes_from_512_byte_sectors() {
        let disks = parse_diskstats(DISKSTATS);
        assert_eq!(
            disks["mmcblk0"],
            DiskCounters {
                read_bytes: 800_000 * 512,
                write_bytes: 240_000 * 512,
                busy_millis: Some(9000),
            }
        );
        assert!(disks.contains_key("mmcblk0p1"));
    }

    #[test]
    fn diskstats_gives_no_busy_time_for_a_device_that_never_counts_it() {
        // A software RAID device: lots of I/O, no time at all.
        let text = "9 0 md0 11626891 0 6908688760 0 47743199 0 2782007536 0 0 0 0\n\
                    8 0 sda 0 0 0 0 0 0 0 0 0 0 0\n";
        let disks = parse_diskstats(text);
        assert_eq!(disks["md0"].busy_millis, None);
        assert_eq!(disks["sda"].busy_millis, Some(0), "idle is not unknown");
    }

    #[test]
    fn diskstats_skips_malformed_lines() {
        let text = "\n8 0 short 1 2 3\n8 0 bad a 0 0 0 0 0 0 0 0 0\n\
                    8 0 ok 1 0 2 0 1 0 4 0 0 5 5\n";
        let disks = parse_diskstats(text);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks["ok"].write_bytes, 4 * 512);
    }

    #[test]
    fn only_physical_disks_with_something_in_them_are_listed() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/root");
        // The fixture also has loop0 (no device), mmcblk0boot0, sr0 and an
        // empty card reader, sda.
        assert_eq!(physical_disk_names(&root), ["mmcblk0"]);
        assert_eq!(
            physical_disk_names(Path::new("/nonexistent")),
            Vec::<String>::new()
        );
    }

    #[test]
    fn reading_a_missing_root_gives_empty_readings() {
        let readings = Readings::read(Path::new("/this/does/not/exist"));
        assert_eq!(readings, Readings::default());
    }

    #[test]
    fn reading_the_fixture_root_finds_everything() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/root");
        let readings = Readings::read(&root);
        assert!(readings.cpu.is_some());
        assert_eq!(readings.load1, Some(0.07));
        assert!(readings.memory.is_some());
        assert_eq!(readings.temperature, Some(48.686));
        assert!(readings.net.contains_key("eth0"));
        assert!(readings.disks.contains_key("mmcblk0"));
    }
}
