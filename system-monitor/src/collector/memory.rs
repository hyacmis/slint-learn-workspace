use sysinfo::System;

pub fn memory_usage(system: &System) -> f32 {
    memory_usage_from_bytes(system.total_memory(), system.used_memory())
}

fn memory_usage_from_bytes(total: u64, used: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    (used as f64 * 100.0 / total as f64) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calculates_memory_percentage() {
        assert_eq!(memory_usage_from_bytes(8_000, 2_000), 25.0);
    }

    #[test]
    fn returns_zero_when_total_memory_is_unavailable() {
        assert_eq!(memory_usage_from_bytes(0, 0), 0.0);
    }
}
