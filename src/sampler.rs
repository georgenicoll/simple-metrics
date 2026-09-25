//! Turns successive [`Readings`] of the machine into rows of values.
//!
//! Some readings are gauges (memory in use right now) and go straight into the
//! row. Others are cumulative counters (CPU ticks, bytes sent) and only mean
//! something as a change since the previous reading, so the sampler remembers
//! the last one. The first row therefore has no rates. A value that can't be
//! worked out is `NaN`, which the protocol reports as `null`: a gap in the
//! chart rather than a misleading zero.

use std::collections::HashMap;

use crate::metrics::Schema;
use crate::proc::{CpuTimes, NetCounters, Readings};

/// Remembers the previous counters so it can turn the next ones into rates.
#[derive(Debug)]
pub struct Sampler {
    interfaces: Vec<String>,
    previous: Option<Previous>,
}

#[derive(Debug)]
struct Previous {
    cpu: Option<CpuTimes>,
    net: HashMap<String, NetCounters>,
}

impl Sampler {
    /// A sampler that reports on these network `interfaces`, in this order.
    /// (The order must match [`crate::metrics::Schema::new`].)
    #[must_use]
    pub fn new(interfaces: Vec<String>) -> Self {
        Self {
            interfaces,
            previous: None,
        }
    }

    /// Produces one row of values, in the [`crate::metrics::Schema`] order, from
    /// the latest `readings`. `elapsed_secs` is the time since the previous
    /// call, and is what the counters' changes are divided by.
    pub fn sample(&mut self, readings: &Readings, elapsed_secs: f64) -> Vec<f64> {
        let previous = self.previous.take();
        let mut row = Vec::with_capacity(Schema::width_for(self.interfaces.len()));

        row.push(cpu_percent(
            previous.as_ref().and_then(|p| p.cpu),
            readings.cpu,
        ));
        row.push(readings.load1.unwrap_or(f64::NAN));
        row.push(readings.memory.map_or(f64::NAN, |m| to_f64(m.used)));
        row.push(readings.memory.map_or(f64::NAN, |m| to_f64(m.swap_used)));
        row.push(readings.temperature.unwrap_or(f64::NAN));

        for interface in &self.interfaces {
            let before = previous.as_ref().and_then(|p| p.net.get(interface));
            let now = readings.net.get(interface);
            row.push(rate(
                before.map(|c| c.rx_bytes),
                now.map(|c| c.rx_bytes),
                elapsed_secs,
            ));
            row.push(rate(
                before.map(|c| c.tx_bytes),
                now.map(|c| c.tx_bytes),
                elapsed_secs,
            ));
        }

        self.previous = Some(Previous {
            cpu: readings.cpu,
            net: readings.net.clone(),
        });
        row
    }
}

#[allow(clippy::cast_precision_loss)] // counters stay far below 2^53 in practice
fn to_f64(value: u64) -> f64 {
    value as f64
}

/// The percentage of CPU time spent busy between two readings.
fn cpu_percent(before: Option<CpuTimes>, now: Option<CpuTimes>) -> f64 {
    let (Some(before), Some(now)) = (before, now) else {
        return f64::NAN;
    };
    // A counter that went backwards (a reset) or didn't move (two readings in
    // the same tick) gives no meaningful percentage.
    let (Some(total), Some(busy)) = (
        now.total.checked_sub(before.total),
        now.busy.checked_sub(before.busy),
    ) else {
        return f64::NAN;
    };
    if total == 0 {
        return f64::NAN;
    }
    (100.0 * to_f64(busy) / to_f64(total)).clamp(0.0, 100.0)
}

/// Bytes per second between two readings of a cumulative counter.
fn rate(before: Option<u64>, now: Option<u64>, elapsed_secs: f64) -> f64 {
    let (Some(before), Some(now)) = (before, now) else {
        return f64::NAN;
    };
    // A counter that went backwards means the interface was reset or
    // recreated in between; the change can't be known.
    match now.checked_sub(before) {
        Some(delta) if elapsed_secs > 0.0 => to_f64(delta) / elapsed_secs,
        _ => f64::NAN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::Memory;

    // Row positions, from the schema's order.
    const CPU: usize = 0;
    const LOAD: usize = 1;
    const MEM: usize = 2;
    const SWAP: usize = 3;
    const TEMP: usize = 4;
    const ETH0_RX: usize = 5;
    const ETH0_TX: usize = 6;
    const WG0_RX: usize = 7;

    fn readings(busy: u64, total: u64, eth0: (u64, u64)) -> Readings {
        Readings {
            cpu: Some(CpuTimes { busy, total }),
            load1: Some(0.5),
            memory: Some(Memory {
                used: 1000,
                swap_used: 200,
            }),
            temperature: Some(50.0),
            net: HashMap::from([(
                "eth0".to_owned(),
                NetCounters {
                    rx_bytes: eth0.0,
                    tx_bytes: eth0.1,
                },
            )]),
        }
    }

    fn sampler() -> Sampler {
        Sampler::new(vec!["eth0".to_owned(), "wg0".to_owned()])
    }

    #[test]
    fn a_row_has_a_value_for_every_metric() {
        let row = sampler().sample(&readings(0, 0, (0, 0)), 5.0);
        assert_eq!(row.len(), 5 + 2 * 2);
    }

    #[test]
    fn gauges_are_reported_straight_away() {
        let row = sampler().sample(&readings(10, 100, (0, 0)), 5.0);
        assert!((row[LOAD] - 0.5).abs() < f64::EPSILON);
        assert!((row[MEM] - 1000.0).abs() < f64::EPSILON);
        assert!((row[SWAP] - 200.0).abs() < f64::EPSILON);
        assert!((row[TEMP] - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_first_row_has_no_rates() {
        let row = sampler().sample(&readings(10, 100, (500, 500)), 5.0);
        assert!(row[CPU].is_nan());
        assert!(row[ETH0_RX].is_nan());
        assert!(row[ETH0_TX].is_nan());
    }

    #[test]
    fn cpu_percent_is_the_busy_share_of_the_change() {
        let mut sampler = sampler();
        sampler.sample(&readings(100, 1000, (0, 0)), 5.0);
        // 50 of the 200 new ticks were busy.
        let row = sampler.sample(&readings(150, 1200, (0, 0)), 5.0);
        assert!((row[CPU] - 25.0).abs() < 1e-9);
    }

    #[test]
    fn network_rates_are_bytes_per_second() {
        let mut sampler = sampler();
        sampler.sample(&readings(0, 100, (1000, 4000)), 5.0);
        let row = sampler.sample(&readings(0, 200, (1500, 4100)), 5.0);
        assert!((row[ETH0_RX] - 100.0).abs() < 1e-9); // 500 bytes over 5 s
        assert!((row[ETH0_TX] - 20.0).abs() < 1e-9); // 100 bytes over 5 s
    }

    #[test]
    fn a_rate_uses_the_time_actually_elapsed() {
        let mut sampler = sampler();
        sampler.sample(&readings(0, 100, (0, 0)), 5.0);
        let row = sampler.sample(&readings(0, 200, (1000, 0)), 2.0);
        assert!((row[ETH0_RX] - 500.0).abs() < 1e-9);
    }

    #[test]
    fn a_counter_that_goes_backwards_is_a_gap_not_a_huge_number() {
        let mut sampler = sampler();
        sampler.sample(&readings(500, 1000, (9000, 9000)), 5.0);
        let row = sampler.sample(&readings(10, 20, (100, 100)), 5.0);
        assert!(row[CPU].is_nan());
        assert!(row[ETH0_RX].is_nan());
        assert!(row[ETH0_TX].is_nan());
    }

    #[test]
    fn no_cpu_time_passing_is_a_gap() {
        let mut sampler = sampler();
        sampler.sample(&readings(10, 100, (0, 0)), 5.0);
        let row = sampler.sample(&readings(10, 100, (0, 0)), 5.0);
        assert!(row[CPU].is_nan());
    }

    #[test]
    fn zero_elapsed_time_gives_no_rate() {
        let mut sampler = sampler();
        sampler.sample(&readings(0, 100, (0, 0)), 5.0);
        let row = sampler.sample(&readings(0, 200, (1000, 1000)), 0.0);
        assert!(row[ETH0_RX].is_nan());
    }

    #[test]
    fn cpu_percent_never_leaves_zero_to_one_hundred() {
        let mut sampler = sampler();
        sampler.sample(&readings(0, 100, (0, 0)), 5.0);
        // Busy grew more than total: inconsistent input, clamped.
        let row = sampler.sample(&readings(500, 200, (0, 0)), 5.0);
        assert!((row[CPU] - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn an_interface_that_is_absent_is_a_gap() {
        let mut sampler = sampler();
        sampler.sample(&readings(0, 100, (0, 0)), 5.0);
        let row = sampler.sample(&readings(0, 200, (10, 10)), 5.0);
        assert!(row[WG0_RX].is_nan()); // "wg0" isn't in the readings
        assert!(!row[ETH0_RX].is_nan());
    }

    #[test]
    fn an_interface_that_reappears_starts_from_a_gap() {
        let mut sampler = sampler();
        sampler.sample(&readings(0, 100, (100, 100)), 5.0);
        let mut without = readings(0, 200, (0, 0));
        without.net.clear();
        sampler.sample(&without, 5.0);
        // Back again: there's no previous counter to compare with.
        let row = sampler.sample(&readings(0, 300, (900, 900)), 5.0);
        assert!(row[ETH0_RX].is_nan());
    }

    #[test]
    fn unreadable_gauges_are_gaps() {
        let row = sampler().sample(&Readings::default(), 5.0);
        assert_eq!(row.len(), 9);
        assert!(row.iter().all(|v| v.is_nan()));
    }
}
