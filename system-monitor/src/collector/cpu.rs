use sysinfo::System;

pub fn cpu_usage(system: &System) -> f32 {
    system.global_cpu_usage()
}
