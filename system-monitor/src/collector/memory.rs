use sysinfo::System;

pub fn memory_usage(system: &System) -> f32 {
    let total = system.total_memory();
    let used = system.used_memory() as f32;
    if total == 0 {
        return 0.0;
    }
    (used * 100.0) / total as f32
}
