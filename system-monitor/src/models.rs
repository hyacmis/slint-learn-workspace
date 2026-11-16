#[derive(Debug, Copy, Clone)]
pub struct MonitorData {
    pub cpu: f32,
    pub memory: f32,
    pub network_received_bytes_per_second: f64,
    pub network_transmitted_bytes_per_second: f64,
}

impl MonitorData {
    pub fn format_cpu(&self) -> String {
        format_percentage(self.cpu)
    }

    pub fn format_memory(&self) -> String {
        format_percentage(self.memory)
    }

    pub fn format_network(&self) -> (String, String) {
        (
            format_byte_rate(self.network_received_bytes_per_second),
            format_byte_rate(self.network_transmitted_bytes_per_second),
        )
    }
}

fn format_percentage(value: f32) -> String {
    format!("{value:.1} %")
}

fn format_byte_rate(bytes_per_second: f64) -> String {
    const UNITS: [&str; 4] = ["B/s", "KB/s", "MB/s", "GB/s"];

    let mut rate = bytes_per_second.max(0.0);
    let mut unit = 0;
    while rate >= 1024.0 && unit < UNITS.len() - 1 {
        rate /= 1024.0;
        unit += 1;
    }

    format!("{rate:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_network_rates_with_readable_units() {
        let data = MonitorData {
            cpu: 0.0,
            memory: 0.0,
            network_received_bytes_per_second: 1536.0,
            network_transmitted_bytes_per_second: 512.0,
        };

        assert_eq!(
            data.format_network(),
            ("1.5 KB/s".to_string(), "512.0 B/s".to_string())
        );
    }

    #[test]
    fn formats_cpu_and_memory_percentages_consistently() {
        assert_eq!(format_percentage(42.56), "42.6 %");
        assert_eq!(format_percentage(0.0), "0.0 %");
    }

    #[test]
    fn formats_network_rate_unit_boundaries() {
        assert_eq!(format_byte_rate(0.0), "0.0 B/s");
        assert_eq!(format_byte_rate(1024.0), "1.0 KB/s");
        assert_eq!(format_byte_rate(1024.0 * 1024.0), "1.0 MB/s");
        assert_eq!(format_byte_rate(1024.0_f64.powi(3)), "1.0 GB/s");
    }

    #[test]
    fn clamps_negative_network_rates_to_zero() {
        assert_eq!(format_byte_rate(-1.0), "0.0 B/s");
    }
}
