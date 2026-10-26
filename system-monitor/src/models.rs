#[derive(Debug, Copy, Clone)]
pub struct MonitorData {
    pub cpu: f32,
    pub memory: f32,
}

impl MonitorData {
    pub fn format_cpu(&self) -> String {
        format!("{:.1} %", self.cpu)
    }
    pub fn format_memory(&self) -> String {
        format!("{:.1} %", self.memory)
    }
}
