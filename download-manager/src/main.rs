#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use slint::{Model, ModelRc, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::error::Error;
use std::rc::Rc;
use std::time::Duration;

slint::include_modules!();

fn main() -> Result<(), Box<dyn Error>> {
    let ui = AppWindow::new()?;

    // ① 领域模型：Vec<DownloadTask>
    let tasks = Rc::new(RefCell::new(vec![
        DownloadTask::new("test.zip", 1000),
        // 特意用超长文件名：用来验证它只会自己变成省略号，不会挤动进度/状态
        DownloadTask::new("ubuntu-24.04.1-live-server-amd64-uefi-signed.iso", 4000),
        DownloadTask::new("movie.mp4", 2000),
    ]));

    // ② Slint 模型：Vec<DownloadTask> -> VecModel<DownloadItem> -> ModelRc
    //    VecModel 里存的是"给界面看的"数据，Rust 侧仍是 DownloadTask
    let items = Rc::new(VecModel::from(
        tasks
            .borrow()
            .iter()
            .map(DownloadTask::to_item)
            .collect::<Vec<_>>(),
    ));
    ui.set_downloads(ModelRc::from(items.clone()));

    // ③ 每秒推进一次；只有真正变化的行才 set_row_data，ListView 只重绘那一行
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_secs(1), {
        let tasks = tasks.clone();
        let items = items.clone();

        move || {
            for (index, task) in tasks.borrow_mut().iter_mut().enumerate() {
                if task.tick() {
                    items.set_row_data(index, task.to_item());
                }
            }
        }
    });

    // ④ 行内按钮：用行下标找到对应任务
    ui.on_task_clicked({
        let tasks = tasks.clone();
        let items = items.clone();

        move |index| {
            let index = index as usize;

            let mut tasks = tasks.borrow_mut();
            let Some(task) = tasks.get_mut(index) else {
                return;
            };

            task.toggle();
            items.set_row_data(index, task.to_item());
        }
    });

    ui.run()?;

    Ok(())
}

struct DownloadTask {
    name: String,
    total_size: u64,
    down_size: u64,
    status: DownloadStatus,
}

impl DownloadTask {
    fn new(name: &str, total_size: u64) -> Self {
        Self {
            name: name.to_owned(),
            total_size,
            down_size: 0,
            status: DownloadStatus::Waiting,
        }
    }

    /// 推进一轮下载。返回 true 表示这一行的显示需要刷新。
    fn tick(&mut self) -> bool {
        if self.status != DownloadStatus::Downloading {
            return false;
        }

        self.down_size = (self.down_size + 100).min(self.total_size);
        if self.down_size >= self.total_size {
            self.status = DownloadStatus::Completed;
        }
        true
    }

    /// 行内按钮的动作：Waiting/Paused 开始，Downloading 暂停，Completed 无操作。
    fn toggle(&mut self) {
        match self.status {
            DownloadStatus::Waiting | DownloadStatus::Paused => {
                self.status = DownloadStatus::Downloading;
            }
            DownloadStatus::Downloading => self.status = DownloadStatus::Paused,
            DownloadStatus::Completed => {}
        }
    }

    /// 进度百分比（0-100）。用整数运算，避免 f32 出现 "30.000002%" 这类显示。
    fn percent(&self) -> u64 {
        self.down_size
            .saturating_mul(100)
            .checked_div(self.total_size)
            .unwrap_or(0)
    }

    fn action(&self) -> &'static str {
        match self.status {
            DownloadStatus::Waiting => "Start",
            DownloadStatus::Downloading => "Pause",
            DownloadStatus::Paused => "Resume",
            DownloadStatus::Completed => "",
        }
    }

    /// 领域模型 -> Slint 列表行
    fn to_item(&self) -> DownloadItem {
        DownloadItem {
            name: self.name.clone().into(),
            progress: format!("{}%", self.percent()).into(),
            status: self.status.to_str().into(),
            action: self.action().into(),
            can_act: self.status != DownloadStatus::Completed,
        }
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
