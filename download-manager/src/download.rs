//! 真实 HTTP 下载引擎。
//!
//! 一个任务 = 一个后台线程。后台线程写文件、推进原子计数，
//! UI 线程只是轮询这些原子量，因此 UI 永远不会被网络 I/O 卡住。

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// 一次下载尝试的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// 还没有开始过，或者刚开始、后台线程尚未有结论
    Idle,
    /// 被暂停打断，文件里保留了已完成的部分
    Paused,
    /// 完整下载完毕
    Done,
    Failed(String),
}

/// UI 线程与后台线程共享的任务状态。
pub struct Shared {
    /// 已写入磁盘的字节数
    down_size: AtomicU64,
    /// 总字节数；0 表示服务器没给 Content-Length
    total_size: AtomicU64,
    /// 请求暂停 / 中止当前这次传输
    cancel: AtomicBool,
    /// 所有权令牌：为 true 时说明有后台线程正在写这个文件。
    /// 用它来保证两次尝试不会同时写同一个文件。
    busy: AtomicBool,
    outcome: Mutex<Outcome>,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            down_size: AtomicU64::new(0),
            total_size: AtomicU64::new(0),
            cancel: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            outcome: Mutex::new(Outcome::Idle),
        })
    }

    pub fn down_size(&self) -> u64 {
        self.down_size.load(Ordering::Relaxed)
    }

    pub fn total_size(&self) -> u64 {
        self.total_size.load(Ordering::Relaxed)
    }

    pub fn outcome(&self) -> Outcome {
        self.outcome.lock().unwrap().clone()
    }

    /// 是否有后台线程正在写这个文件。
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    fn set_outcome(&self, outcome: Outcome) {
        *self.outcome.lock().unwrap() = outcome;
    }
}

/// 全局共享的 HTTP agent。
///
/// 这里显式把 TLS provider 设成 `NativeTls`：ureq 的文档明确说明 native-tls
/// 不会被默认选中，must be configured on the agent。选它有两个原因：
/// 它不需要现场编译 C 代码（默认 rustls 后端会拉入 ring，那正是构建失败的来源），
/// 而且它用操作系统的证书库。
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();

    AGENT.get_or_init(|| {
        ureq::config::Config::builder()
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .provider(ureq::tls::TlsProvider::NativeTls)
                    .build(),
            )
            .build()
            .into()
    })
}

/// 尝试开始（或继续）下载。
///
/// 返回 `false` 表示上一次尝试的线程还没退出（比如刚点完暂停又立刻点继续），
/// 此时不要启动新线程，否则两个线程会同时写同一个文件。调用者应当稍后重试。
pub fn try_start(shared: &Arc<Shared>, url: String, path: PathBuf) -> bool {
    if shared.busy.swap(true, Ordering::AcqRel) {
        return false;
    }

    shared.cancel.store(false, Ordering::Relaxed);
    shared.set_outcome(Outcome::Idle);

    let shared = shared.clone();
    std::thread::spawn(move || {
        // 用 guard 保住 busy 令牌：worker 无论是正常结束还是 panic，
        // 都会释放令牌。否则一次 panic 就会让这个任务永远卡在 Downloading，
        // 而且 UI 再也无法启动它（try_start 永远抢不到令牌）。
        let _guard = WorkerGuard(shared.clone());

        let outcome = match transfer(&url, &path, &shared) {
            Ok(true) => Outcome::Done,
            Ok(false) => Outcome::Paused,
            Err(message) => Outcome::Failed(message),
        };
        shared.set_outcome(outcome);
    });

    true
}

/// 释放 busy 令牌，并保证任务一定有一个结论。
struct WorkerGuard(Arc<Shared>);

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        let mut outcome = self.0.outcome.lock().unwrap();
        if matches!(*outcome, Outcome::Idle) {
            // transfer 没能给出结论，说明 worker 是 panic 出去的
            *outcome = Outcome::Failed("下载线程异常终止".to_string());
        }
        drop(outcome);

        // 必须最后释放：UI 看到 busy == false 时，文件一定已经写完了
        self.0.busy.store(false, Ordering::Release);
    }
}

/// 请求暂停。后台线程会在下一个数据块处退出，已下载的部分保留在磁盘上。
pub fn pause(shared: &Shared) {
    shared.cancel.store(true, Ordering::Relaxed);
}

/// 传输主体。返回 Ok(true) 表示下载完成，Ok(false) 表示被暂停打断。
fn transfer(url: &str, path: &Path, shared: &Shared) -> Result<bool, String> {
    // 断点续传：从已经落盘的字节数之后接着下
    let offset = shared.down_size.load(Ordering::Relaxed);

    let mut request = agent().get(url);
    if offset > 0 {
        request = request.header("Range", format!("bytes={offset}-"));
    }
    let mut response = request.call().map_err(describe)?;

    // 只有服务器认可 Range 才会回 206；回 200 说明它忽略了 Range，
    // 已有内容是无效的，必须从头写，否则文件会拼接错乱。
    let resumed = response.status().as_u16() == 206;
    if offset > 0 && !resumed {
        shared.down_size.store(0, Ordering::Relaxed);
    }

    if let Some(len) = response.body().content_length() {
        let total = if resumed { offset } else { 0 } + len;
        shared.total_size.store(total, Ordering::Relaxed);
    }

    let mut file = if resumed {
        OpenOptions::new().append(true).open(path)
    } else {
        File::create(path)
    }
    .map_err(|e| format!("无法创建 {}：{e}", path.display()))?;

    let mut written = if resumed { offset } else { 0 };
    let mut reader = response.body_mut().as_reader();
    let mut buffer = vec![0u8; 64 * 1024];

    loop {
        if shared.cancel.load(Ordering::Relaxed) {
            let _ = file.flush();
            return Ok(false);
        }

        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                file.write_all(&buffer[..n])
                    .map_err(|e| format!("写入失败：{e}"))?;
                written += n as u64;
                shared.down_size.store(written, Ordering::Relaxed);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("读取失败：{e}")),
        }
    }

    file.flush().map_err(|e| format!("刷新失败：{e}"))?;
    Ok(true)
}

fn describe(error: ureq::Error) -> String {
    match error {
        ureq::Error::StatusCode(code) => format!("服务器返回 HTTP {code}"),
        ureq::Error::ConnectionFailed => "无法连接服务器".to_string(),
        ureq::Error::BadUri(_) => "下载地址无效".to_string(),
        other => other.to_string(),
    }
}

/// 从 URL 推出一个安全的文件名。
pub fn file_name_from_url(url: &str) -> String {
    let path = url.split('#').next().unwrap_or(url);
    let path = path.split('?').next().unwrap_or(path);

    // 去掉 scheme://authority，只看真正的 path 部分；
    // 否则 http://example.com 这种没有路径的 URL 会把主机名当成文件名。
    let without_scheme = path.split_once("://").map_or(path, |(_, rest)| rest);
    let path = without_scheme.split_once('/').map_or("", |(_, tail)| tail);

    let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");

    let cleaned: String = last
        .chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim_matches(|c| c == '.' || c == ' ');

    if cleaned.is_empty() {
        "download.bin".to_string()
    } else {
        cleaned.to_string()
    }
}

/// 把字节数格式化成人类可读的形式（用于服务器没给 Content-Length 的情况）。
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 用滑动窗口内的字节增量算瞬时速度（字节/秒）。
/// 时间跨度过小时（刚启动）返回 0，避免除出离谱的数字。
pub fn speed_bps(elapsed: Duration, transferred: u64) -> f64 {
    let seconds = elapsed.as_secs_f64();
    if seconds < 0.2 {
        0.0
    } else {
        transferred as f64 / seconds
    }
}

/// "已下载 / 总大小"。总大小未知（服务器没给 Content-Length）时退化成 "已下载 / 未知"。
pub fn format_progress(down: u64, total: u64) -> String {
    if total == 0 {
        format!("{} / 未知", format_size(down))
    } else {
        format!("{} / {}", format_size(down), format_size(total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    /// 起一个最小的 HTTP 服务器，返回 (url, 服务器线程)。
    /// `delay` 让服务器每块之间停顿，用来测试"暂停"。
    fn spawn_server(body: Vec<u8>, support_range: bool, delay: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());

                let mut request_line = String::new();
                if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                    continue;
                }

                let mut start = 0usize;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 {
                        break;
                    }
                    let header = header.trim();
                    if header.is_empty() {
                        break;
                    }
                    if let Some(value) = header.strip_prefix("Range: bytes=") {
                        start = value.trim_end_matches('-').parse().unwrap_or(0);
                    }
                }

                let partial = support_range && start > 0 && start < body.len();
                let slice = if partial { &body[start..] } else { &body[..] };
                let status = if partial {
                    "206 Partial Content"
                } else {
                    "200 OK"
                };

                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    slice.len()
                );
                let _ = stream.write_all(head.as_bytes());
                for chunk in slice.chunks(8 * 1024) {
                    if stream.write_all(chunk).is_err() {
                        break;
                    }
                    let _ = stream.flush();
                    if !delay.is_zero() {
                        std::thread::sleep(delay);
                    }
                }
            }
        });

        format!("http://{addr}/file.bin")
    }

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("slint-download-manager-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        path
    }

    fn wait_until_idle(shared: &Shared, timeout: Duration) -> Outcome {
        let deadline = Instant::now() + timeout;
        while shared.busy.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        shared.outcome()
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn downloads_whole_file_and_reports_progress() {
        let body = payload(300 * 1024);
        let url = spawn_server(body.clone(), true, Duration::ZERO);
        let path = temp_path("whole.bin");

        let shared = Shared::new();
        assert!(try_start(&shared, url, path.clone()));

        assert_eq!(
            wait_until_idle(&shared, Duration::from_secs(20)),
            Outcome::Done
        );
        assert_eq!(shared.total_size(), body.len() as u64);
        assert_eq!(shared.down_size(), body.len() as u64);
        assert_eq!(std::fs::read(&path).unwrap(), body);
    }

    #[test]
    fn resumes_from_existing_bytes_with_range() {
        let body = payload(200 * 1024);
        let url = spawn_server(body.clone(), true, Duration::ZERO);
        let path = temp_path("resume.bin");

        // 先造出"上次下到一半"的状态
        let half = body.len() / 2;
        std::fs::write(&path, &body[..half]).unwrap();

        let shared = Shared::new();
        shared.down_size.store(half as u64, Ordering::Relaxed);
        assert!(try_start(&shared, url, path.clone()));

        assert_eq!(
            wait_until_idle(&shared, Duration::from_secs(20)),
            Outcome::Done
        );
        // 关键：文件必须是完整的、没有重复或错位
        assert_eq!(std::fs::read(&path).unwrap(), body);
    }

    #[test]
    fn pause_stops_the_transfer_and_keeps_partial_file() {
        let body = payload(512 * 1024);
        // 每 8KB 停 20ms，总共约 1.3 秒，足够我们中途暂停
        let url = spawn_server(body.clone(), true, Duration::from_millis(20));
        let path = temp_path("paused.bin");

        let shared = Shared::new();
        assert!(try_start(&shared, url, path.clone()));
        std::thread::sleep(Duration::from_millis(150));
        pause(&shared);

        assert_eq!(
            wait_until_idle(&shared, Duration::from_secs(10)),
            Outcome::Paused
        );

        let written = std::fs::metadata(&path).unwrap().len();
        assert!(written > 0, "暂停前应该已经写入了一些数据");
        assert!(written < body.len() as u64, "暂停时不应该已经下完");
    }

    #[test]
    fn file_name_from_url_handles_query_and_illegal_chars() {
        assert_eq!(file_name_from_url("http://host/a/b/file.zip"), "file.zip");
        assert_eq!(
            file_name_from_url("http://host/file.zip?token=1"),
            "file.zip"
        );
        assert_eq!(file_name_from_url("http://host/dir/"), "dir");
        assert_eq!(file_name_from_url("http://host"), "download.bin");
        assert_eq!(file_name_from_url("http://host/a:b*c.bin"), "a_b_c.bin");
    }

    #[test]
    fn format_size_is_readable() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(3 * 1024 * 1024), "3.0 MB");
    }

    #[test]
    fn worker_guard_releases_busy_even_when_the_thread_panics() {
        let shared = Shared::new();

        // 模拟 worker 抢到了令牌
        assert!(!shared.busy.swap(true, Ordering::AcqRel));

        let worker_shared = shared.clone();
        let handle = std::thread::spawn(move || {
            let _guard = WorkerGuard(worker_shared);
            panic!("模拟下载线程崩溃");
        });
        assert!(handle.join().is_err(), "worker 应该 panic");

        // 令牌必须被释放，否则任务会永远卡在 Downloading
        assert!(!shared.is_busy(), "panic 后 busy 令牌必须释放");
        assert_eq!(
            shared.outcome(),
            Outcome::Failed("下载线程异常终止".to_string())
        );
    }

    #[test]
    fn speed_is_bytes_per_second() {
        assert_eq!(speed_bps(Duration::from_secs(2), 4096), 2048.0);
        // 跨度过小时不计算
        assert_eq!(speed_bps(Duration::from_millis(50), 100), 0.0);
    }

    #[test]
    fn format_progress_shows_downloaded_over_total() {
        assert_eq!(format_progress(0, 2048), "0 B / 2.0 KB");
        assert_eq!(format_progress(1024, 2048), "1.0 KB / 2.0 KB");
        assert_eq!(format_progress(500, 0), "500 B / 未知");
    }
}
