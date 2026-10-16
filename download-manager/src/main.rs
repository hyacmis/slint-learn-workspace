#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use slint::{Timer, TimerMode};
use std::cell::RefCell;
use std::error::Error;
use std::rc::Rc;
use std::time::Duration;

slint::include_modules!();

fn main() -> Result<(), Box<dyn Error>> {
    let ui = AppWindow::new()?;

    let manager = Rc::new(RefCell::new(DownloadManager {
        tasks: vec![DownloadTask {
            name: "test.zip".into(),
            total_size: 1000,
            down_size: 0,
            status: DownloadStatus::Waiting,
        }],
    }));

    let timer = Timer::default();

    timer.start(TimerMode::Repeated, Duration::from_secs(1), {
        let manager = manager.clone();
        let ui_weak = ui.as_weak();

        move || {
            manager.borrow_mut().tick();

            if let Some(ui) = ui_weak.upgrade() {
                sync_ui(&ui, &manager.borrow());
            }
        }
    });

    ui.on_start_clicked({
        let manager = manager.clone();
        let ui_weak = ui.as_weak();

        move || {
            {
                let mut manager = manager.borrow_mut();
                if let Some(task) = manager.tasks.first_mut() {
                    task.start();
                }
            }

            if let Some(ui) = ui_weak.upgrade() {
                sync_ui(&ui, &manager.borrow());
            }
        }
    });

    ui.on_pause_clicked({
        let manager = manager.clone();
        let ui_weak = ui.as_weak();

        move || {
            {
                let mut manager = manager.borrow_mut();
                if let Some(task) = manager.tasks.first_mut() {
                    task.pause();
                }
            }

            if let Some(ui) = ui_weak.upgrade() {
                sync_ui(&ui, &manager.borrow());
            }
        }
    });

    ui.on_resume_clicked({
        let manager = manager.clone();
        let ui_weak = ui.as_weak();

        move || {
            {
                let mut manager = manager.borrow_mut();
                if let Some(task) = manager.tasks.first_mut() {
                    task.resume();
                }
            }

            if let Some(ui) = ui_weak.upgrade() {
                sync_ui(&ui, &manager.borrow());
            }
        }
    });

    sync_ui(&ui, &manager.borrow());
    ui.run()?;

    Ok(())
}

struct DownloadManager {
    tasks: Vec<DownloadTask>,
}

impl DownloadManager {
    fn tick(&mut self) {
        for task in &mut self.tasks {
            if task.status == DownloadStatus::Downloading {
                task.down_size += 100;
                if task.down_size >= task.total_size {
                    task.down_size = task.total_size;
                    task.status = DownloadStatus::Completed;
                }
            }
        }
    }
}

struct DownloadTask {
    name: String,
    total_size: u64,
    down_size: u64,
    status: DownloadStatus,
}

impl DownloadTask {
    /// 已完成的下载是终态，不再接受状态变更。
    fn start(&mut self) {
        if self.status != DownloadStatus::Completed {
            self.status = DownloadStatus::Downloading;
        }
    }

    fn pause(&mut self) {
        if self.status == DownloadStatus::Downloading {
            self.status = DownloadStatus::Paused;
        }
    }

    fn resume(&mut self) {
        if self.status == DownloadStatus::Paused {
            self.status = DownloadStatus::Downloading;
        }
    }

    /// 进度百分比（0-100）。用整数运算，避免 f32 出现 "30.000002%" 这类显示。
    fn percent(&self) -> u64 {
        self.down_size
            .saturating_mul(100)
            .checked_div(self.total_size)
            .unwrap_or(0)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum DownloadStatus {
    Waiting,
    Downloading,
    Paused,
    Completed,
}

impl DownloadStatus {
    fn to_str(self) -> &'static str {
        match self {
            DownloadStatus::Waiting => "Waiting",
            DownloadStatus::Downloading => "Downloading",
            DownloadStatus::Paused => "Paused",
            DownloadStatus::Completed => "Completed",
        }
    }
}

fn sync_ui(ui: &AppWindow, manager: &DownloadManager) {
    let Some(task) = manager.tasks.first() else {
        return;
    };

    ui.set_file_name(task.name.clone().into());
    ui.set_progress_text(format!("{}%", task.percent()).into());
    ui.set_status_text(task.status.to_str().into());
}
