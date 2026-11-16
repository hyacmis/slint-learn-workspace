use std::{
    thread,
    time::{Duration, Instant},
};

use crate::AppWindow;
use crate::collector::{cpu, memory, network};
use crate::models::MonitorData;
use slint::{Weak, invoke_from_event_loop};
use sysinfo::{Networks, System};

pub fn start_worker(ui: Weak<AppWindow>) {
    thread::spawn(move || {
        let mut system = System::new_all();
        let mut networks = Networks::new_with_refreshed_list();
        let mut previous_refresh = Instant::now();

        loop {
            if ui.upgrade().is_none() {
                break;
            }

            thread::sleep(Duration::from_secs(1));
            let now = Instant::now();
            let elapsed = now.duration_since(previous_refresh);
            previous_refresh = now;

            system.refresh_cpu_all();
            system.refresh_memory();
            networks.refresh(true);
            let network_usage = network::network_usage(&networks, elapsed);

            let data = MonitorData {
                cpu: cpu::cpu_usage(&system),
                memory: memory::memory_usage(&system),
                network_received_bytes_per_second: network_usage.received_bytes_per_second,
                network_transmitted_bytes_per_second: network_usage.transmitted_bytes_per_second,
            };

            let ui_weak = ui.clone();

            if invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_cpu_text(data.format_cpu().into());
                    ui.set_memory_text(data.format_memory().into());
                    let (received, transmitted) = data.format_network();
                    ui.set_network_received_text(received.into());
                    ui.set_network_transmitted_text(transmitted.into());
                }
            })
            .is_err()
            {
                break;
            }
        }
    });
}
