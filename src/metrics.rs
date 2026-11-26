//! What is measured: the fixed set of variables every record contains, and the
//! ones calculated from them.

use crate::store::Column;

/// One variable that can be asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metric {
    /// A stable identifier, used in the socket protocol (`cpu_percent`).
    pub name: String,
    /// A short human-readable name for a chart (`CPU`).
    pub label: String,
    /// What the values are measured in (`%`, `bytes`, `bytes/s`, `°C`, or an
    /// empty string for a plain number).
    pub unit: &'static str,
}

/// The metrics, in order, and where each one's value comes from. Most are
/// stored in every record, in this order; a calculated one is worked out from
/// those when asked for and takes no room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    metrics: Vec<Metric>,
    columns: Vec<Column>,
}

/// The metrics that don't depend on the configured interfaces or disks, in
/// order.
const FIXED: [(&str, &str, &str); 7] = [
    ("cpu_percent", "CPU", "%"),
    ("load1", "Load (1 min)", ""),
    ("mem_used_bytes", "Memory used", "bytes"),
    ("swap_used_bytes", "Swap used", "bytes"),
    ("mem_used_percent", "Memory used %", "%"),
    ("swap_used_percent", "Swap used %", "%"),
    ("cpu_temp_celsius", "CPU temperature", "°C"),
];

impl Schema {
    /// How many values a record holds (calculated metrics aren't among them)
    /// when there are `interface_count`
    /// interfaces and `disk_count` disks, without building a schema to find
    /// out.
    #[must_use]
    pub const fn width_for(interface_count: usize, disk_count: usize) -> usize {
        FIXED.len() + 2 * interface_count + 3 * disk_count
    }

    /// The schema for a machine with these network `interfaces` and `disks`:
    /// the fixed metrics, then a receive and a transmit rate for each
    /// interface, then a read rate, a write rate and a busy percentage for
    /// each disk, with the disk's total rate, which is calculated, listed
    /// after its write rate.
    #[must_use]
    pub fn new(interfaces: &[String], disks: &[String]) -> Self {
        let mut metrics: Vec<Metric> = FIXED
            .iter()
            .map(|&(name, label, unit)| Metric {
                name: name.to_owned(),
                label: label.to_owned(),
                unit,
            })
            .collect();
        for interface in interfaces {
            metrics.push(Metric {
                name: format!("net_{interface}_rx_bytes_per_sec"),
                label: format!("{interface} received"),
                unit: "bytes/s",
            });
            metrics.push(Metric {
                name: format!("net_{interface}_tx_bytes_per_sec"),
                label: format!("{interface} sent"),
                unit: "bytes/s",
            });
        }
        let mut columns: Vec<Column> = (0..metrics.len()).map(Column::Stored).collect();
        let mut stored = metrics.len();
        for disk in disks {
            let (read, write, busy) = (stored, stored + 1, stored + 2);
            stored += 3;
            for (suffix, label, unit, column) in [
                (
                    "read_bytes_per_sec",
                    "read",
                    "bytes/s",
                    Column::Stored(read),
                ),
                (
                    "write_bytes_per_sec",
                    "written",
                    "bytes/s",
                    Column::Stored(write),
                ),
                (
                    "total_bytes_per_sec",
                    "total",
                    "bytes/s",
                    Column::Sum(read, write),
                ),
                ("busy_percent", "busy", "%", Column::Stored(busy)),
            ] {
                metrics.push(Metric {
                    name: format!("disk_{disk}_{suffix}"),
                    label: format!("{disk} {label}"),
                    unit,
                });
                columns.push(column);
            }
        }
        Self { metrics, columns }
    }

    /// Where each metric's value comes from in a record, in the order of
    /// [`Schema::metrics`].
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// The names of the metrics a record holds, in the record's order.
    #[must_use]
    pub fn stored_names(&self) -> Vec<String> {
        self.metrics
            .iter()
            .zip(&self.columns)
            .filter(|(_, column)| matches!(column, Column::Stored(_)))
            .map(|(metric, _)| metric.name.clone())
            .collect()
    }

    /// How many values a record holds.
    #[must_use]
    pub fn width(&self) -> usize {
        self.columns
            .iter()
            .filter(|column| matches!(column, Column::Stored(_)))
            .count()
    }

    /// All the metrics, in order.
    #[must_use]
    pub fn metrics(&self) -> &[Metric] {
        &self.metrics
    }

    /// How many metrics there are, calculated ones included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.metrics.len()
    }

    /// Whether the schema has no metrics (never true for one from [`Schema::new`]).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.metrics.is_empty()
    }
}

/// Whether `name` is usable as a network interface name here: the characters
/// Linux interface names use in practice, and nothing that could confuse the
/// metric names or the protocol built from them.
#[must_use]
pub fn is_valid_interface(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15 // IFNAMSIZ - 1
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Whether `name` is usable as a disk name here: what the kernel calls block
/// devices (`sda`, `nvme0n1`, `dm-0`), and nothing that could confuse the
/// metric names or the protocol built from them.
#[must_use]
pub fn is_valid_disk(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 31 // DISK_NAME_LEN - 1
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interfaces(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn has_the_fixed_metrics_then_two_per_interface() {
        let schema = Schema::new(&interfaces(&["eth0", "wg0"]), &[]);
        let names: Vec<&str> = schema.metrics().iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "cpu_percent",
                "load1",
                "mem_used_bytes",
                "swap_used_bytes",
                "mem_used_percent",
                "swap_used_percent",
                "cpu_temp_celsius",
                "net_eth0_rx_bytes_per_sec",
                "net_eth0_tx_bytes_per_sec",
                "net_wg0_rx_bytes_per_sec",
                "net_wg0_tx_bytes_per_sec",
            ]
        );
        assert_eq!(schema.len(), 11);
        assert!(!schema.is_empty());
    }

    #[test]
    fn with_no_interfaces_only_the_fixed_metrics_remain() {
        assert_eq!(Schema::new(&[], &[]).len(), FIXED.len());
    }

    #[test]
    fn metric_names_are_unique() {
        let schema = Schema::new(
            &interfaces(&["eth0", "wlan0", "wlan1", "br-ap", "wg0"]),
            &interfaces(&["sda", "sdb"]),
        );
        let mut names: Vec<&str> = schema.metrics().iter().map(|m| m.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), schema.len());
    }

    #[test]
    fn disks_follow_the_interfaces_with_a_calculated_total_each() {
        let schema = Schema::new(&interfaces(&["eth0"]), &interfaces(&["sda"]));
        let names: Vec<&str> = schema.metrics().iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names[FIXED.len() + 2..],
            [
                "disk_sda_read_bytes_per_sec",
                "disk_sda_write_bytes_per_sec",
                "disk_sda_total_bytes_per_sec",
                "disk_sda_busy_percent",
            ]
        );
        assert_eq!(schema.len(), 7 + 2 + 4);
        assert_eq!(schema.width(), Schema::width_for(1, 1));
    }

    #[test]
    fn the_total_takes_no_room_and_the_rest_are_stored_in_order() {
        let schema = Schema::new(&[], &interfaces(&["sda", "sdb"]));
        let first = FIXED.len();
        assert_eq!(
            schema.columns()[first..],
            [
                Column::Stored(first),
                Column::Stored(first + 1),
                Column::Sum(first, first + 1),
                Column::Stored(first + 2),
                Column::Stored(first + 3),
                Column::Stored(first + 4),
                Column::Sum(first + 3, first + 4),
                Column::Stored(first + 5),
            ]
        );
        let stored = schema.stored_names();
        assert_eq!(stored.len(), schema.width());
        assert!(!stored.iter().any(|name| name.contains("total")));
    }

    #[test]
    fn disk_names_are_checked() {
        for name in ["sda", "nvme0n1", "mmcblk0", "dm-0", "md127"] {
            assert!(is_valid_disk(name), "{name}");
        }
        for name in ["", "a b", "a/b", "cciss!c0d0", &"x".repeat(32)] {
            assert!(!is_valid_disk(name), "{name:?}");
        }
    }

    #[test]
    fn accepts_ordinary_interface_names() {
        for name in ["eth0", "wlan1", "br-ap", "wg0", "enp0s3", "veth_a.1"] {
            assert!(is_valid_interface(name), "{name}");
        }
    }

    #[test]
    fn rejects_names_that_could_cause_trouble() {
        for name in [
            "",
            "a b",
            "a/b",
            "a:b",
            "a\"b",
            "a\nb",
            "way-too-long-name",
            "é",
        ] {
            assert!(!is_valid_interface(name), "{name:?}");
        }
    }
}
