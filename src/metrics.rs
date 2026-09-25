//! What is measured: the fixed set of variables every record contains.

/// One measured variable.
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

/// The ordered list of metrics in every record. A record's values are in this
/// order, so an index into the schema is an index into a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    metrics: Vec<Metric>,
}

/// The metrics that don't depend on the configured interfaces, in order.
const FIXED: [(&str, &str, &str); 5] = [
    ("cpu_percent", "CPU", "%"),
    ("load1", "Load (1 min)", ""),
    ("mem_used_bytes", "Memory used", "bytes"),
    ("swap_used_bytes", "Swap used", "bytes"),
    ("cpu_temp_celsius", "CPU temperature", "°C"),
];

impl Schema {
    /// How many values a record holds when there are `interface_count`
    /// interfaces, without building a schema to find out.
    #[must_use]
    pub const fn width_for(interface_count: usize) -> usize {
        FIXED.len() + 2 * interface_count
    }

    /// The schema for a machine with these network `interfaces`: the fixed
    /// metrics, then a receive and a transmit rate for each interface.
    #[must_use]
    pub fn new(interfaces: &[String]) -> Self {
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
        Self { metrics }
    }

    /// All the metrics, in record order.
    #[must_use]
    pub fn metrics(&self) -> &[Metric] {
        &self.metrics
    }

    /// How many values a record holds.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn interfaces(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn has_the_fixed_metrics_then_two_per_interface() {
        let schema = Schema::new(&interfaces(&["eth0", "wg0"]));
        let names: Vec<&str> = schema.metrics().iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "cpu_percent",
                "load1",
                "mem_used_bytes",
                "swap_used_bytes",
                "cpu_temp_celsius",
                "net_eth0_rx_bytes_per_sec",
                "net_eth0_tx_bytes_per_sec",
                "net_wg0_rx_bytes_per_sec",
                "net_wg0_tx_bytes_per_sec",
            ]
        );
        assert_eq!(schema.len(), 9);
        assert!(!schema.is_empty());
    }

    #[test]
    fn with_no_interfaces_only_the_fixed_metrics_remain() {
        assert_eq!(Schema::new(&[]).len(), FIXED.len());
    }

    #[test]
    fn metric_names_are_unique() {
        let schema = Schema::new(&interfaces(&["eth0", "wlan0", "wlan1", "br-ap", "wg0"]));
        let mut names: Vec<&str> = schema.metrics().iter().map(|m| m.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), schema.len());
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
