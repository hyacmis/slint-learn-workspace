use std::{thread, time::Duration};

use crate::AppWindow;
use crate::collector::{cpu, memory};
use crate::models::MonitorData;
use slint::{Weak, invoke_from_event_loop};
use sysinfo::System;

pub fn start_worker(ui: Weak<AppWindow>) {
    thread::spawn(move || {
        let mut system = System::new_all();

        loop {
            system.refresh_cpu_all();
            system.refresh_memory();

            let data = MonitorData {
                cpu: cpu::cpu_usage(&system),
                memory: memory::memory_usage(&system),
            };

            let ui_weak = ui.clone();

            invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_cpu_text(data.format_cpu().into());
                    ui.set_memory_text(data.format_memory().into());
                }
            })
            .ok();

            thread::sleep(Duration::from_secs(1));
        }
    });
}
