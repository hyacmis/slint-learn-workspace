use std::time::Duration;

use sysinfo::Networks;

#[derive(Debug, Copy, Clone)]
pub struct NetworkUsage {
    pub received_bytes_per_second: f64,
    pub transmitted_bytes_per_second: f64,
}

pub fn network_usage(networks: &Networks, elapsed: Duration) -> NetworkUsage {
    network_usage_from_deltas(
        networks
            .values()
            .map(|network| (network.received(), network.transmitted())),
        elapsed,
    )
}

fn network_usage_from_deltas(
    deltas: impl IntoIterator<Item = (u64, u64)>,
    elapsed: Duration,
) -> NetworkUsage {
    let seconds = elapsed.as_secs_f64();
    if seconds == 0.0 {
        return NetworkUsage {
            received_bytes_per_second: 0.0,
            transmitted_bytes_per_second: 0.0,
        };
    }

    let (received, transmitted) = deltas.into_iter().fold((0_u64, 0_u64), |totals, delta| {
        (
            totals.0.saturating_add(delta.0),
            totals.1.saturating_add(delta.1),
        )
    });

    NetworkUsage {
        received_bytes_per_second: received as f64 / seconds,
        transmitted_bytes_per_second: transmitted as f64 / seconds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_interface_deltas_and_calculates_rates() {
        let usage = network_usage_from_deltas([(600, 200), (400, 800)], Duration::from_secs(2));

        assert_eq!(usage.received_bytes_per_second, 500.0);
        assert_eq!(usage.transmitted_bytes_per_second, 500.0);
    }

    #[test]
    fn returns_zero_rates_for_zero_elapsed_time() {
        let usage = network_usage_from_deltas([(100, 200)], Duration::ZERO);

        assert_eq!(usage.received_bytes_per_second, 0.0);
        assert_eq!(usage.transmitted_bytes_per_second, 0.0);
    }

    #[test]
    fn saturates_interface_totals_instead_of_overflowing() {
        let usage = network_usage_from_deltas([(u64::MAX, 1), (1, 1)], Duration::from_secs(1));

        assert_eq!(usage.received_bytes_per_second, u64::MAX as f64);
        assert_eq!(usage.transmitted_bytes_per_second, 2.0);
    }
}
