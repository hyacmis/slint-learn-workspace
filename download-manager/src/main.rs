#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod download;

use download::{Outcome, Shared};
use slint::{Model, ModelRc, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

slint::include_modules!();

/// UI 轮询后台进度的间隔。真实下载的进度就是以这个节奏刷进列表的。
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 算瞬时速度的滑动窗口：用最近这段时间的字节增量，而不是全程平均。
const SPEED_WINDOW: Duration = Duration::from_secs(2);

fn main() -> Result<(), Box<dyn Error>> {
    let ui = AppWindow::new()?;
    let dir = downloads_dir()?;

    // ① 领域模型：Vec<DownloadTask>。现在是空的，任务全部由界面添加。
    let tasks = Rc::new(RefCell::new(Vec::<DownloadTask>::new()));

    // ② Slint 模型：Vec<DownloadTask> -> VecModel<DownloadItem> -> ModelRc
    let items = Rc::new(VecModel::<DownloadItem>::from(Vec::new()));
    ui.set_downloads(ModelRc::from(items.clone()));

    // ③ 每 100ms 轮询一次后台线程；只有真正变化的行才 set_row_data
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, POLL_INTERVAL, {
        let tasks = tasks.clone();
        let items = items.clone();
        let ui_weak = ui.as_weak();

        move || {
            let mut failure = None;
            let mut notice = None;

            {
                let mut tasks = tasks.borrow_mut();
                // 先快照所有任务的路径：应用服务器建议的文件名时要和别的任务去重
                let taken: Vec<PathBuf> = tasks.iter().map(|task| task.path.clone()).collect();

                for (index, task) in tasks.iter_mut().enumerate() {
                    // 后台探测到的 Content-Disposition 文件名到了就改名
                    let suggested = task.progress.take_suggested_name();
                    if suggested.is_some_and(|name| task.apply_suggested_name(&name, &taken, index))
                    {
                        items.set_row_data(index, task.to_item());
                    }

                    // 后台留下的提示（比如"服务器不支持断点续传"）优先展示，
                    // 免得用户以为 Resume 从头下是程序的问题
                    if let Some(text) = task.progress.take_notice() {
                        notice = Some(format!("{}：{text}", task.name));
                    }

                    if task.poll() {
                        items.set_row_data(index, task.to_item());

                        if task.status == DownloadStatus::Failed {
                            failure = Some(format!(
                                "{} 下载失败：{}",
                                task.name,
                                task.message.clone().unwrap_or_default()
                            ));
                        }
                    }
                }
            }

            let message = failure.or(notice);

            if let Some(message) = message
                && let Some(ui) = ui_weak.upgrade()
            {
                ui.set_message(message.into());
            }
        }
    });

    // ④ 新增下载：从输入框取 URL，推出文件名，加入列表
    ui.on_add_clicked({
        let ui_weak = ui.as_weak();
        let tasks = tasks.clone();
        let items = items.clone();
        let dir = dir.clone();

        move |url| {
            let url = url.trim().to_string();

            let Some(ui) = ui_weak.upgrade() else {
                return;
            };

            if !url.starts_with("http://") && !url.starts_with("https://") {
                ui.set_message("请输入以 http:// 或 https:// 开头的下载地址".into());
                return;
            }

            let taken: Vec<PathBuf> = tasks
                .borrow()
                .iter()
                .map(|task| task.path.clone())
                .collect();
            let path = unique_path(&dir, &download::file_name_from_url(&url), &taken);
            let task = DownloadTask::new(url.clone(), path);
            let name = task.name.clone();

            // 后台问服务器真实文件名（Content-Disposition），不阻塞 UI
            download::probe_name(url, task.progress.clone());

            items.push(task.to_item());
            tasks.borrow_mut().push(task);

            ui.set_url_text("".into());
            ui.set_message(format!("已添加 {name}，点行内按钮开始下载").into());
        }
    });

    // ⑤ 行内按钮：用行下标定位任务
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

    ui.set_message("填入一个 http(s) 下载地址，点「添加下载」。文件保存在 ./downloads/".into());
    ui.run()?;

    Ok(())
}

struct DownloadTask {
    url: String,
    name: String,
    path: PathBuf,
    status: DownloadStatus,
    /// 失败原因，仅 Failed 时有值
    message: Option<String>,
    /// 和后台下载线程共享的状态
    progress: Arc<Shared>,
    /// 最近 SPEED_WINDOW 内的采样：(时刻, 已下载字节数)
    samples: VecDeque<(Instant, u64)>,
    /// 当前速度，字节/秒
    speed_bps: f64,

    /// 上一次渲染出去的内容快照，用来避免无意义的重绘
    shown_down: u64,
    shown_total: u64,
    shown_speed: String,
    shown_status: DownloadStatus,

    /// 上一次尝试的线程还没退出时，把启动请求挂起到下一次轮询
    pending_start: bool,
}

impl DownloadTask {
    fn new(url: String, path: PathBuf) -> Self {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "download.bin".to_string());

        Self {
            url,
            name,
            path,
            status: DownloadStatus::Waiting,
            message: None,
            progress: Shared::new(),
            samples: VecDeque::new(),
            speed_bps: 0.0,
            shown_down: u64::MAX,
            shown_total: u64::MAX,
            shown_speed: String::new(),
            shown_status: DownloadStatus::Waiting,
            pending_start: false,
        }
    }

    /// 行内按钮的动作。
    fn toggle(&mut self) {
        match self.status {
            DownloadStatus::Waiting | DownloadStatus::Paused | DownloadStatus::Failed => {
                self.request_start();
            }
            DownloadStatus::Downloading => {
                self.status = DownloadStatus::Paused;
                download::pause(&self.progress);
            }
            DownloadStatus::Completed => {}
        }
    }

    fn request_start(&mut self) {
        self.message = None;
        self.status = DownloadStatus::Downloading;
        self.samples.clear();
        self.speed_bps = 0.0;
        self.shown_down = u64::MAX;
        self.shown_total = u64::MAX;
        self.shown_speed = String::new();

        if !download::try_start(&self.progress, self.url.clone(), self.path.clone()) {
            // 上一次尝试的线程还没退出：等它退出，下一次轮询再启动，
            // 避免两个线程同时写同一个文件
            self.pending_start = true;
        }
    }

    /// 轮询后台线程。返回 true 表示这一行需要刷新。
    fn poll(&mut self) -> bool {
        if self.pending_start && !self.progress.is_busy() {
            self.pending_start = false;
            self.request_start();
            return true;
        }

        if self.progress.is_busy() {
            self.measure_speed();
        } else {
            // 没在传输：速度归零，并把后台线程的结论搬进领域模型
            self.samples.clear();
            self.speed_bps = 0.0;

            match self.progress.outcome() {
                Outcome::Done if self.status != DownloadStatus::Completed => {
                    self.status = DownloadStatus::Completed;
                }
                Outcome::Failed(message) if self.status != DownloadStatus::Failed => {
                    self.status = DownloadStatus::Failed;
                    self.message = Some(message);
                }
                Outcome::Paused if self.status == DownloadStatus::Downloading => {
                    self.status = DownloadStatus::Paused;
                }
                _ => {}
            }
        }

        // 和上一次渲染出去的内容快照比较，任何一项变了才刷新这一行。
        // 不能只比百分比：chunked 响应拿不到总大小，百分比恒为 0，
        // 那样已下载大小和速度都会一直停在 0 不动。
        let down = self.progress.down_size();
        let total = self.progress.total_size();
        let speed = self.speed_text();

        let changed = down != self.shown_down
            || total != self.shown_total
            || speed != self.shown_speed
            || self.status != self.shown_status;

        self.shown_down = down;
        self.shown_total = total;
        self.shown_speed = speed;
        self.shown_status = self.status;

        changed
    }

    /// 用最近 SPEED_WINDOW 的采样算瞬时速度。
    fn measure_speed(&mut self) {
        let now = Instant::now();
        self.samples.push_back((now, self.progress.down_size()));

        while let Some(&(when, _)) = self.samples.front() {
            if now.duration_since(when) > SPEED_WINDOW {
                self.samples.pop_front();
            } else {
                break;
            }
        }

        self.speed_bps = match (self.samples.front(), self.samples.back()) {
            (Some(&(t0, d0)), Some(&(t1, d1))) => {
                download::speed_bps(t1.duration_since(t0), d1.saturating_sub(d0))
            }
            _ => 0.0,
        };
    }

    /// 服务器通过 Content-Disposition 建议的文件名到了。
    ///
    /// 只在任务还没开始过（Waiting）时才改名：一旦开始下载，文件可能已经
    /// 按旧名字建出来了，再改会让续传找不到文件。
    fn apply_suggested_name(
        &mut self,
        suggested: &str,
        taken: &[PathBuf],
        self_index: usize,
    ) -> bool {
        if self.status != DownloadStatus::Waiting {
            return false;
        }

        let dir = self
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let others: Vec<PathBuf> = taken
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != self_index)
            .map(|(_, path)| path.clone())
            .collect();

        let path = unique_path(&dir, &download::sanitize_file_name(suggested), &others);
        if path == self.path {
            return false;
        }

        self.name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "download.bin".to_string());
        self.path = path;
        true
    }

    /// 大小列："已下载 / 总大小"。
    fn size_text(&self) -> String {
        download::format_progress(self.progress.down_size(), self.progress.total_size())
    }

    /// 速度列：当前下载速度。刚启动还没测出来、或已经停下时显示占位符。
    fn speed_text(&self) -> String {
        if self.speed_bps < 1.0 {
            "—".to_string()
        } else {
            format!("{}/s", download::format_size(self.speed_bps as u64))
        }
    }

    fn action(&self) -> &'static str {
        match self.status {
            DownloadStatus::Waiting => "Start",
            DownloadStatus::Downloading => "Pause",
            DownloadStatus::Paused => "Resume",
            DownloadStatus::Failed => "Retry",
            DownloadStatus::Completed => "",
        }
    }

    /// 领域模型 -> Slint 列表行
    fn to_item(&self) -> DownloadItem {
        DownloadItem {
            name: self.name.clone().into(),
            size: self.size_text().into(),
            speed: self.speed_text().into(),
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
    Failed,
}

impl DownloadStatus {
    fn to_str(self) -> &'static str {
        match self {
            DownloadStatus::Waiting => "Waiting",
            DownloadStatus::Downloading => "Downloading",
            DownloadStatus::Paused => "Paused",
            DownloadStatus::Completed => "Completed",
            DownloadStatus::Failed => "Failed",
        }
    }
}

fn downloads_dir() -> Result<PathBuf, Box<dyn Error>> {
    let dir = std::env::current_dir()?.join("downloads");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// 重名的任务不能写同一个文件，冲突时加 -1 / -2 后缀。
fn unique_path(dir: &Path, name: &str, taken: &[PathBuf]) -> PathBuf {
    let original = Path::new(name);
    let stem = original
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string());
    let ext = original
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();

    let mut candidate = dir.join(name);
    let mut suffix = 1;
    while taken.contains(&candidate) {
        candidate = dir.join(format!("{stem}-{suffix}{ext}"));
        suffix += 1;
    }
    candidate
}
