//! 真实 HTTP 下载引擎。
//!
//! 一个任务 = 一个后台线程；支持断点续传，并在服务器支持 `Range` 时
//! 自动切成固定大小的分块、由若干连接并行领取下载（多连接 / 分块队列）。
//! 后台线程写文件、推进原子计数，UI 线程只轮询这些原子量，
//! 因此 UI 永远不会被网络 I/O 卡住。
//!
//! HTTP 走 `reqwest::blocking`：对外是同步 API（`send()` 直接返回
//! `blocking::Response`，它自己实现了 `Read`），我们的代码里没有 async。
//! 但 reqwest 内部仍然是 tokio —— blocking 客户端只是在自己的线程上跑一个
//! current_thread runtime。也就是说 tokio 会被编译进来，只是不暴露给调用方。

use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// 同时下载的连接数。
const CONNECTIONS: usize = 4;
/// 每个分块的大小。
///
/// 分块比"把文件均分成 N 段"更均衡：某个连接恰好很慢时，其它连接可以继续领新块，
/// 不会出现"3 个连接闲着、1 个在爬"的长尾。代价是请求数变多。
const CHUNK_SIZE: u64 = 2 << 20;
/// 每次读取的缓冲区大小
const READ_BUFFER: usize = 64 * 1024;

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
    /// 服务器通过 Content-Disposition 建议的文件名，由 `probe_name` 在后台填入
    suggested_name: Mutex<Option<String>>,
    /// 一次性提示（例如"服务器不支持断点续传，已从头开始"），UI 取走后显示
    notice: Mutex<Option<String>>,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            down_size: AtomicU64::new(0),
            total_size: AtomicU64::new(0),
            cancel: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            outcome: Mutex::new(Outcome::Idle),
            suggested_name: Mutex::new(None),
            notice: Mutex::new(None),
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

    /// 取走后台探测到的建议文件名（取一次就没了）。
    pub fn take_suggested_name(&self) -> Option<String> {
        self.suggested_name.lock().unwrap().take()
    }

    /// 取走后台留下的提示（取一次就没了）。
    pub fn take_notice(&self) -> Option<String> {
        self.notice.lock().unwrap().take()
    }

    fn set_notice(&self, text: impl Into<String>) {
        *self.notice.lock().unwrap() = Some(text.into());
    }

    fn set_outcome(&self, outcome: Outcome) {
        *self.outcome.lock().unwrap() = outcome;
    }
}

/// 探测类请求（HEAD / 只要 1 字节的 GET）的超时：这些请求本来就该很快，
/// 卡住的服务器不值得让后台线程一直等。
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// 正式传输的"卡住多久算失败"。
///
/// 注意 reqwest 的 blocking 客户端并不是给整份文件一个时限：
/// `Read for Response` 里是 `wait::timeout(self.body_mut().read(buf), timeout)`，
/// 也就是**每一次 read** 各自计时（而 read 一旦有几个字节就返回，不会攒满缓冲区）。
/// 所以这个值等于"连续 N 秒一个字节都没收到才断开"，慢但一直在动的下载不会被砍；
/// 顺带也让暂停最多 N 秒内生效（卡住的 read 超时返回后才能看到 cancel 标志）。
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// 全局共享的 HTTP 客户端（顺带复用连接池）。
///
/// 一个 `blocking::Client` 内部只有**一个** runtime 线程，所有请求（包括 4 个
/// 分块线程同时发的请求）都复用这一个，不会每个任务多点出一个 runtime。
/// 所以把它做成全局的不是为了省内存，而是为了让连接和代理探测只做一次。
///
/// TLS 用 rustls + webpki 根证书（`Cargo.toml` 里选的 `rustls-tls-webpki-roots`），
/// 加密实现是 ring，由 reqwest 这个特性自己带，不用手动装 provider。
/// 别换成 native-tls：那是 SChannel，在受限环境里连 `AcquireCredentialsHandle`
/// 都过不去，报 `SEC_E_NO_CREDENTIALS`；同一环境下 Windows 自带的 `curl.exe`
/// 也会失败，而 rustls 正常。
///
/// 哪天要升到 0.13：那边 `rustls-tls-webpki-roots` 已被删除，默认后端是
/// aws-lc-rs（要 CMake），替代写法是特性改用 `rustls-no-provider`、把
/// `rustls` 的 `ring` 特性加进依赖，并在 `build()` 之前调用一次
/// `rustls::crypto::ring::default_provider().install_default()`。
fn client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            // reqwest 的默认值是 30 秒，语义见 STALL_TIMEOUT 的注释
            .timeout(STALL_TIMEOUT)
            .connect_timeout(Duration::from_secs(30))
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            // 只有 TLS backend 初始化失败才会走到这里，属程序员错误；
            // 真发生了由 WorkerGuard 兜底，任务会变成 Failed 而不是卡住。
            .expect("创建 HTTP 客户端失败")
    })
}

/// 后台问服务器这个文件"应该叫什么"（HTTP 的 `Content-Disposition` 头）。
///
/// 只影响显示名和落盘文件名，失败了也无所谓，所以不返回错误。
/// 典型场景：`https://host/download?id=123` 这种 URL 从路径里抠不出文件名，
/// 但服务器会用这个头告诉客户端真实文件名。
pub fn probe_name(url: String, shared: Arc<Shared>) {
    std::thread::spawn(move || {
        if let Some(name) = fetch_name(&url) {
            *shared.suggested_name.lock().unwrap() = Some(name);
        }
    });
}

fn fetch_name(url: &str) -> Option<String> {
    // 先用 HEAD：不传正文，最省
    if let Ok(response) = client().head(url).timeout(PROBE_TIMEOUT).send()
        && response.status().is_success()
        && let Some(name) = name_from_headers(response.headers())
    {
        return Some(name);
    }

    // 有些服务器不支持 HEAD，退回一个只要 1 字节的 GET
    let response = client()
        .get(url)
        .timeout(PROBE_TIMEOUT)
        .header("Range", "bytes=0-0")
        .send()
        .ok()?;
    name_from_headers(response.headers())
}

fn name_from_headers(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let value = headers.get("content-disposition")?.to_str().ok()?;
    parse_file_name(value)
}

/// 从 `Content-Disposition` 里抠出文件名。
///
/// 优先 `filename*=UTF-8''...`（RFC 5987），其次 `filename=...`。
/// 手写解析而不是引 `regex`，避免为一个次要功能多一个依赖。
fn parse_file_name(header: &str) -> Option<String> {
    let mut plain = None;
    let mut extended = None;

    for part in header.split(';') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let value = value.trim();

        match key.trim().to_ascii_lowercase().as_str() {
            "filename" => plain = Some(value.trim_matches('"').to_string()),
            "filename*" => {
                // 形如 UTF-8''%E4%B8%AD%E6%96%87.txt
                let encoded = value.rsplit("''").next().unwrap_or(value);
                extended = Some(percent_decode(encoded.trim_matches('"')));
            }
            _ => {}
        }
    }

    extended.or(plain).filter(|name| !name.is_empty())
}

/// 只处理 %XX（按 UTF-8 字节解），够用且不用引依赖。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2]))
        {
            out.push(high * 16 + low);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }

    String::from_utf8_lossy(&out).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
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

        let outcome = match run(&url, &path, &shared) {
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
            // run 没能给出结论，说明 worker 是 panic 出去的
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
///
/// 直接写正式文件名，不搞 `.download` 之类的临时后缀：暂停后文件就停在那儿，
/// 下次点 Resume 从里面的断点接着下。进度记在旁边的 `foo.zip.dlstate` 里。
fn run(url: &str, path: &Path, shared: &Arc<Shared>) -> Result<bool, String> {
    match probe(url)? {
        // 至少两块才值得并行
        Some(total) if total > CHUNK_SIZE => chunked(url, path, shared, total),
        // 服务器不支持 Range，或者文件太小：单连接就够了
        _ => single(url, path, shared),
    }
}

/// 探测服务器是否支持 Range，以及总大小。
///
/// 返回 `Some(total)` 表示可以分块；`None` 表示只能单连接下载。
/// 用一个 `Range: bytes=0-0` 的 GET 来问：支持的话服务器会回
/// `206` + `Content-Range: bytes 0-0/TOTAL`。
fn probe(url: &str) -> Result<Option<u64>, String> {
    let response = client()
        .get(url)
        .timeout(PROBE_TIMEOUT)
        .header("Range", "bytes=0-0")
        .send()
        .map_err(|error| describe(error, url))?;

    // 服务器不接受这个 Range（例如 416），不代表文件下不了，退回单连接。
    // 注意 reqwest 不像 ureq 那样默认把 4xx/5xx 变成 Err，状态码得自己判。
    if response.status().as_u16() != 206 {
        return Ok(None);
    }

    let content_range = response
        .headers()
        .get("content-range")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();

    Ok(content_range
        .rsplit('/')
        .next()
        .and_then(|total| total.trim().parse::<u64>().ok()))
}

/// 状态码不是 2xx 就直接失败。
///
/// reqwest 默认不会把 4xx/5xx 当错误（要 `error_for_status()` 才行），
/// 而下载器最怕的就是把 404 的错误页正文当成文件内容写进磁盘。
fn check_status(response: &reqwest::blocking::Response) -> Result<(), String> {
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("服务器返回 HTTP {}", status.as_u16()))
    }
}

/// 单连接下载（同时负责整个文件的断点续传）。
fn single(url: &str, path: &Path, shared: &Shared) -> Result<bool, String> {
    // 断点续传：从已经落盘的字节数之后接着下
    let offset = shared.down_size.load(Ordering::Relaxed);

    let mut request = client().get(url);
    if offset > 0 {
        request = request.header("Range", format!("bytes={offset}-"));
    }
    let mut response = request.send().map_err(|error| describe(error, url))?;
    check_status(&response)?;

    // 只有服务器认可 Range 才会回 206；回 200 说明它忽略了 Range，
    // 已有内容是无效的，必须从头写，否则文件会拼接错乱。
    let resumed = response.status().as_u16() == 206;
    if offset > 0 && !resumed {
        shared.down_size.store(0, Ordering::Relaxed);
        // 别让用户以为"续传失败是程序 bug"：是服务器不给 Range，重下没得选
        shared.set_notice("服务器不支持断点续传，已从头开始下载");
    }

    // blocking::Response 的 content_length 就是这次响应的 Content-Length，
    // 带着 gzip 自动解压的话它会和实际写盘的字节数不一致 ——
    // 所以 Cargo.toml 里干脆没开压缩特性。
    if let Some(len) = response.content_length() {
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
    let mut buffer = vec![0u8; READ_BUFFER];

    // blocking::Response 自己实现了 Read：按块读，不会把整个文件读进内存
    loop {
        if shared.cancel.load(Ordering::Relaxed) {
            let _ = file.flush();
            return Ok(false);
        }

        match response.read(&mut buffer) {
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

/// 分块并行下载。
///
/// 文件先用 `set_len` 撑到最终大小，每个分块直接 `seek` 到自己的偏移写进去，
/// 所以不需要按顺序、也不需要最后再合并。
/// 哪些块已经下完记录在一个伴随文件里，用来续传。
fn chunked(url: &str, path: &Path, shared: &Arc<Shared>, total: u64) -> Result<bool, String> {
    let chunk_count = total.div_ceil(CHUNK_SIZE);

    // 注意用 set_len 而不是 vec![0; total]：后者会真的分配一整块内存
    // （4GB 文件就要 4GB 内存）才能把文件撑大。
    // truncate(false) 很关键：文件里可能已经有上次下好的分块，绝不能清空
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| format!("无法创建 {}：{e}", path.display()))?;
    file.set_len(total)
        .map_err(|e| format!("无法预分配文件：{e}"))?;
    drop(file);

    let state = Arc::new(Mutex::new(ChunkState::open(path, total)?));

    // 注意 down_size 是"已经落盘的字节数"（含下了一半的块），不是下完的整块之和：
    // 否则暂停时进度条会倒退，续传的起点也就跟着错了。
    let downloaded = state.lock().unwrap().downloaded();
    shared.total_size.store(total, Ordering::Relaxed);
    shared.down_size.store(downloaded, Ordering::Relaxed);

    if state.lock().unwrap().all_done() {
        // 上一轮其实已经下完了，只是没来得及清理
        state.lock().unwrap().discard();
        return Ok(true);
    }

    // 所有连接共用一个"下一块"计数器：谁空谁领，天然负载均衡
    let next = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::with_capacity(CONNECTIONS);

    for _ in 0..CONNECTIONS {
        let url = url.to_owned();
        let path = path.to_path_buf();
        let shared = Arc::clone(shared);
        let state = Arc::clone(&state);
        let next = Arc::clone(&next);

        handles.push(std::thread::spawn(move || {
            worker(&url, &path, total, chunk_count, &shared, &state, &next)
        }));
    }

    let mut failure = None;
    for handle in handles {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(message)) => {
                failure.get_or_insert(message);
            }
            Err(_) => {
                failure.get_or_insert_with(|| "分块线程异常终止".to_string());
            }
        }
    }

    if let Some(message) = failure {
        return Err(message);
    }
    if shared.cancel.load(Ordering::Relaxed) {
        return Ok(false);
    }

    state.lock().unwrap().discard();
    Ok(true)
}

/// 一个连接：反复从队列里领分块，直到领完或被暂停。
fn worker(
    url: &str,
    path: &Path,
    total: u64,
    chunk_count: u64,
    shared: &Shared,
    state: &Mutex<ChunkState>,
    next: &AtomicU64,
) -> Result<(), String> {
    // 每个连接用自己的文件句柄：各自 seek 互不干扰，也就不用加锁
    let mut file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| format!("无法打开 {}：{e}", path.display()))?;

    loop {
        if shared.cancel.load(Ordering::Relaxed) {
            return Ok(());
        }

        let index = next.fetch_add(1, Ordering::Relaxed);
        if index >= chunk_count {
            return Ok(());
        }

        let (chunk_start, end) = chunk_range(total, index);
        // 这一块上次下到了哪儿：0 表示没下过，块长表示已经下完（跳过）。
        let already = state.lock().unwrap().saved(index);
        if already >= end - chunk_start {
            continue;
        }

        // 从上次的断点接着下，而不是整块重来 —— 暂停时通常一块都没下完，
        // 整块重来就等于把所有已写入的字节全丢掉。
        let written = download_chunk(url, chunk_start + already, end, &mut file, shared)?;

        if shared.cancel.load(Ordering::Relaxed) {
            // 这一块没下完：记下断点，下次接着下。已写入的那部分数据就在文件里，
            // 不用重下（同一个偏移重复写也是幂等的，只是浪费带宽）。
            state
                .lock()
                .unwrap()
                .set_progress(index, already + written)?;
            return Ok(());
        }

        state
            .lock()
            .unwrap()
            .set_progress(index, end - chunk_start)?;
    }
}

/// 第 `index` 块对应的字节区间 `[start, end)`。
///
/// 必须是 `(total, index)` 的纯函数：续传时要靠它重建每块的偏移，
/// 如果划分不稳定，进度位图里的记录就会错位。
fn chunk_range(total: u64, index: u64) -> (u64, u64) {
    let start = index * CHUNK_SIZE;
    (start, (start + CHUNK_SIZE).min(total))
}

fn chunk_len(total: u64, index: u64) -> u64 {
    let (start, end) = chunk_range(total, index);
    end - start
}

/// 下载一个分块，直接写到文件里对应的偏移上。
///
/// `start..end` 是**这次实际要下的区间**：续传时 `start` 会比块的起点大
/// （那一块的前半截上次已经写进文件了），返回值为这次真正写入的字节数，
/// 调用方靠它把"这一块下到哪儿了"记进进度文件。
fn download_chunk(
    url: &str,
    start: u64,
    end: u64,
    file: &mut File,
    shared: &Shared,
) -> Result<u64, String> {
    let mut response = client()
        .get(url)
        .header("Range", format!("bytes={start}-{}", end - 1))
        .send()
        .map_err(|error| describe(error, url))?;
    check_status(&response)?;

    // 服务器没按 Range 回：绝不能把整段内容写到这个偏移上
    if response.status().as_u16() != 206 {
        return Err("服务器没有按 Range 返回分块".to_string());
    }

    file.seek(SeekFrom::Start(start))
        .map_err(|e| format!("定位失败：{e}"))?;

    let mut buffer = vec![0u8; READ_BUFFER];
    let mut written = 0u64;

    loop {
        if shared.cancel.load(Ordering::Relaxed) {
            let _ = file.flush();
            // 被暂停打断：返回已经写进去的字节数，让调用方记下断点
            return Ok(written);
        }

        match response.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                // 有些服务器会忽略 Range 的结束位置，这里自己截住，避免越界写进下一块
                let room = (end - start - written) as usize;
                let n = n.min(room);
                if n == 0 {
                    break;
                }
                file.write_all(&buffer[..n])
                    .map_err(|e| format!("写入失败：{e}"))?;
                written += n as u64;
                shared.down_size.fetch_add(n as u64, Ordering::Relaxed);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("读取失败：{e}")),
        }
    }

    file.flush().map_err(|e| format!("刷新失败：{e}"))?;
    Ok(written)
}

/// 记录"每一块下到哪儿了"的伴随文件：`foo.zip` -> `foo.zip.dlstate`。
///
/// 每块一个 **u64**（小端，这块已经落盘的字节数），文件长度 = 块数 × 8。
/// 之所以不记"这块完成没有"（1 bit / 1 byte）：
/// 分块是 2MB，一块要下几秒，用户多半在块还没下完时就按了暂停；
/// 只记完成状态的话，那些已经写进文件的字节全会被丢掉、进度条跳回 0，
/// 表现就是"暂停后 Resume 又从头下载一遍"。记字节数才能从断点接着下。
fn state_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".dlstate");
    path.with_file_name(name)
}

/// 每块进度记录的字节数（就是一个 u64）。
const STATE_RECORD: u64 = 8;

struct ChunkState {
    path: PathBuf,
    total: u64,
    /// 每块已落盘的字节数（== 块长 表示这块完成）
    progress: Vec<u64>,
    /// 写进度用；`discard` 时要先把句柄放掉才能删文件（Windows 不允许删打开的文件）
    file: Option<File>,
}

impl ChunkState {
    fn open(main: &Path, total: u64) -> Result<Self, String> {
        let path = state_path(main);
        let chunk_count = total.div_ceil(CHUNK_SIZE) as usize;

        // 只有长度**正好**是 块数×8 才认。两个原因：
        //   1. 我们自己每次都把它预分配到这么长，正常文件一定等长；
        //   2. 旧版本写的是"每块 1 字节"的位图，长度正好等于块数。
        //      若把那种文件按 u64 记录去读，`0x01 0x01 …` 会被解释成天文数字，
        //      于是把**根本没下过的块**判成已完成 —— 文件永远缺几块还不报错。
        //      长度不匹配就一律当成"没有进度"，下面的 set_len 会把它重写成新格式。
        let expected = chunk_count * STATE_RECORD as usize;
        let mut progress = vec![0u64; chunk_count];
        if let Ok(bytes) = fs::read(&path)
            && bytes.len() == expected
        {
            for (index, record) in bytes.as_chunks::<8>().0.iter().enumerate() {
                // 夹到块长以内：文件被人手改过也不会越界
                progress[index] = u64::from_le_bytes(*record).min(chunk_len(total, index as u64));
            }
        }

        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| format!("无法创建 {}：{e}", path.display()))?;

        // 建好就把长度撑到"块数 × 8"：记录定长，第 N 块的偏移永远是 N*8，
        // 写任意一块都不会改变文件长度（也顺带截断上面判定作废的旧文件）。
        file.set_len(chunk_count as u64 * STATE_RECORD)
            .map_err(|e| format!("无法初始化 {}：{e}", path.display()))?;

        Ok(Self {
            path,
            total,
            progress,
            file: Some(file),
        })
    }

    /// 第 `index` 块已经落盘多少字节。续传就从这里接着下。
    fn saved(&self, index: u64) -> u64 {
        self.progress.get(index as usize).copied().unwrap_or(0)
    }

    fn is_done(&self, index: u64) -> bool {
        self.saved(index) >= chunk_len(self.total, index)
    }

    /// 记录第 `index` 块已经落盘 `bytes` 字节，并立刻写盘。
    fn set_progress(&mut self, index: u64, bytes: u64) -> Result<(), String> {
        let bytes = bytes.min(chunk_len(self.total, index));
        if self.progress.get(index as usize).copied() == Some(bytes) {
            return Ok(());
        }
        let Some(slot) = self.progress.get_mut(index as usize) else {
            return Err(format!("分块下标越界：{index}"));
        };
        *slot = bytes;

        let file = self
            .file
            .as_mut()
            .ok_or_else(|| "进度文件已经关闭".to_string())?;
        file.seek(SeekFrom::Start(index * STATE_RECORD))
            .map_err(|e| format!("写入进度失败：{e}"))?;
        file.write_all(&bytes.to_le_bytes())
            .map_err(|e| format!("写入进度失败：{e}"))?;
        Ok(())
    }

    /// 已经落盘的总字节数（含下了一半的块），用来算续传起点的进度。
    fn downloaded(&self) -> u64 {
        self.progress.iter().sum()
    }

    fn all_done(&self) -> bool {
        (0..self.progress.len() as u64).all(|index| self.is_done(index))
    }

    /// 全部完成后把进度文件删掉。
    fn discard(&mut self) {
        self.file = None;
        let _ = fs::remove_file(&self.path);
    }
}

fn describe(error: reqwest::Error, url: &str) -> String {
    let host = host_of(url);

    // URL 都解析不了（reqwest 在构造请求时就失败了）
    if error.is_builder() {
        return "下载地址无效".to_string();
    }
    if error.is_timeout() {
        return format!("连接 {host} 超时");
    }
    if error.is_connect() {
        if is_connection_refused(&error) {
            return format!("无法连接 {host}（连接被拒绝，通常是网络不通或被墙）");
        }
        // reqwest 的 Display 会把 cause 链一路拼进来，太长；只取最里面那句
        return match root_cause(&error) {
            Some(cause) => format!("无法连接 {host}：{cause}"),
            None => format!("无法连接 {host}"),
        };
    }

    // 其余（读正文时断线、TLS 握手以外的问题……）交给 reqwest 自己的描述
    error.to_string()
}

/// 顺着 `source()` 走到最底层的原因。
/// reqwest 的错误是层层包装的（`error sending request ... : error trying to
/// connect ... : tcp connect error ...`），对用户来说只有最里面那句有用。
fn root_cause(error: &(dyn Error + 'static)) -> Option<String> {
    let mut current = error.source();
    let mut last = None;
    while let Some(cause) = current {
        last = Some(cause.to_string());
        current = cause.source();
    }
    last
}

/// 连接被拒绝要单独认出来：这是"网络不通 / 被墙"最典型的形态，
/// 但 reqwest 只会把它包在 `is_connect()` 里面。
fn is_connection_refused(error: &reqwest::Error) -> bool {
    let mut current = Error::source(error);
    while let Some(cause) = current {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            return io.kind() == ErrorKind::ConnectionRefused;
        }
        current = cause.source();
    }
    false
}

/// 从 URL 里抠出主机名，只用于报错提示。
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', ':']).next().unwrap_or(url)
}

/// 去掉文件名里的非法字符（Windows 上尤其重要）。
pub fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
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

/// 从 URL 推出一个安全的文件名。
pub fn file_name_from_url(url: &str) -> String {
    let path = url.split('#').next().unwrap_or(url);
    let path = path.split('?').next().unwrap_or(path);

    // 去掉 scheme://authority，只看真正的 path 部分；
    // 否则 http://example.com 这种没有路径的 URL 会把主机名当成文件名。
    let without_scheme = path.split_once("://").map_or(path, |(_, rest)| rest);
    let path = without_scheme.split_once('/').map_or("", |(_, tail)| tail);

    let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    sanitize_file_name(last)
}

/// 把字节数格式化成人类可读的形式。
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
    use std::net::{TcpListener, TcpStream};
    use std::time::{Duration, Instant};

    /// 服务器观察到的 Range 头。三种情况必须分开，否则测不出"续传"和"重下"的区别：
    ///   * `None`     —— 完全没带 Range（整份请求）
    ///   * `From(n)`  —— `bytes=n-`，断点续传就是发这个
    ///   * `Between(n, m)` —— `bytes=n-m`，分块下载用这个
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum SeenRange {
        None,
        From(usize),
        Between(usize, usize),
    }

    /// 服务器观察到客户端请求的 Range。
    /// 用来证明测试真的走了分块 / 续传路径，而不是悄悄退回从头下载。
    type RangeLog = Arc<Mutex<Vec<SeenRange>>>;

    /// 起一个最小的 HTTP 服务器，返回 url。
    /// 每个连接单独起线程，因为分块下载会同时开好几个连接。
    fn spawn_server(body: Vec<u8>, support_range: bool, delay: Duration) -> String {
        spawn_server_logged(body, support_range, delay).0
    }

    fn spawn_server_logged(
        body: Vec<u8>,
        support_range: bool,
        delay: Duration,
    ) -> (String, RangeLog) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body = Arc::new(body);
        let log: RangeLog = Arc::new(Mutex::new(Vec::new()));
        let log_for_server = Arc::clone(&log);

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let body = Arc::clone(&body);
                let log = Arc::clone(&log_for_server);
                std::thread::spawn(move || serve(stream, &body, support_range, delay, &log));
            }
        });

        (format!("http://{addr}/file.bin"), log)
    }

    /// 处理一个连接：解析请求头，按 Range 返回 200 或 206。
    fn serve(
        mut stream: TcpStream,
        body: &[u8],
        support_range: bool,
        delay: Duration,
        log: &Mutex<Vec<SeenRange>>,
    ) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());

        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }

        let mut saw_range = false;
        let mut start = 0usize;
        let mut end: Option<usize> = None;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                break;
            }
            let header = header.trim();
            if header.is_empty() {
                break;
            }
            // 注意：reqwest 底下的 hyper 会把头名规范成小写，实际发的是 "range: bytes=..."
            let lower = header.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("range: bytes=") {
                let (from, to) = value.split_once('-').unwrap_or((value, ""));
                saw_range = true;
                start = from.trim().parse().unwrap_or(0);
                end = to.trim().parse::<usize>().ok();
            }
        }

        log.lock().unwrap().push(match (saw_range, end) {
            (false, _) => SeenRange::None,
            (true, None) => SeenRange::From(start),
            (true, Some(end)) => SeenRange::Between(start, end),
        });

        let total = body.len();
        let range_start = start.min(total);
        // `bytes=N-`（没有结束位置）就是"从 N 一直到结尾"，跟真实服务器一致
        let range_stop = end.map_or(total, |e| (e + 1).min(total)).max(range_start);

        // 带 Range 头、且确实只请求了一部分，才算 206。
        // 注意最后一块的 stop 正好等于 total，不能用 stop < total 来判断。
        let partial = support_range && saw_range && (range_start > 0 || range_stop < total);

        // 不支持 Range（或没带 Range）时必须回整份内容 ——
        // 不能只回 start 之后那一段，否则客户端会以为拿到的是完整文件。
        let (slice_start, slice_stop) = if partial {
            (range_start, range_stop)
        } else {
            (0, total)
        };
        let slice = &body[slice_start..slice_stop];
        let status = if partial {
            "206 Partial Content"
        } else {
            "200 OK"
        };

        let mut head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
            slice.len()
        );
        if partial {
            head.push_str(&format!(
                "Content-Range: bytes {}-{}/{total}\r\n",
                slice_start,
                slice_stop - 1
            ));
        }
        head.push_str("\r\n");

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

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("slint-download-manager-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        // 数据文件和进度文件都清一遍，测试之间不能互相干扰
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(state_path(&path));
        path
    }

    fn wait_until_idle(shared: &Shared, timeout: Duration) -> Outcome {
        let deadline = Instant::now() + timeout;
        while shared.is_busy() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        shared.outcome()
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// 让下载一直跑在"不支持 Range"的老路上
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
        let body = payload(800 * 1024);
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

        // 半截数据直接留在正式文件名里，下次从它的末尾接着下
        let written = std::fs::metadata(&path).unwrap().len();
        assert!(written > 0, "暂停前应该已经写入了一些数据");
        assert!(written < body.len() as u64, "暂停时不应该已经下完");
    }

    #[test]
    fn chunked_download_produces_an_identical_file() {
        // 9 MiB / 2 MiB = 5 块，4 个连接：会有人领到第二块
        let body = payload(9 * 1024 * 1024);
        let (url, ranges) = spawn_server_logged(body.clone(), true, Duration::ZERO);
        let path = temp_path("chunked.bin");

        let shared = Shared::new();
        assert!(try_start(&shared, url, path.clone()));

        assert_eq!(
            wait_until_idle(&shared, Duration::from_secs(30)),
            Outcome::Done
        );
        assert_eq!(shared.total_size(), body.len() as u64);
        assert_eq!(shared.down_size(), body.len() as u64);
        assert_eq!(std::fs::read(&path).unwrap(), body);
        assert!(!state_path(&path).exists(), "完成后进度文件应该被清理");

        // 关键：必须真的走了多连接分块，而不是偷偷退回单连接。
        // 单连接路径不会带 Range 头；探测请求只带 bytes=0-0（end == start）。
        let chunk_requests = ranges
            .lock()
            .unwrap()
            .iter()
            .filter(|range| matches!(range, SeenRange::Between(start, end) if end > start))
            .count();
        assert!(
            chunk_requests >= 2,
            "应该发起多个 Range 分块请求，实际只有 {chunk_requests} 个"
        );
    }

    #[test]
    fn chunked_download_resumes_from_the_state_file() {
        let body = payload(9 * 1024 * 1024);
        let url = spawn_server(body.clone(), true, Duration::ZERO);
        let path = temp_path("chunked-resume.bin");
        let total = body.len() as u64;
        let chunk_count = total.div_ceil(CHUNK_SIZE);

        // 造出"上一轮已经把第 0、2 块下完"的现场：
        // 预分配文件 + 写入这两块 + 写进度文件
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .unwrap();
        file.set_len(total).unwrap();
        for index in [0u64, 2] {
            let (start, end) = chunk_range(total, index);
            file.seek(SeekFrom::Start(start)).unwrap();
            file.write_all(&body[start as usize..end as usize]).unwrap();
        }
        file.flush().unwrap();
        drop(file);

        // 进度文件：每块一个 u64（已落盘字节数），第 0、2 块是整块完成
        let mut state = vec![0u8; chunk_count as usize * STATE_RECORD as usize];
        for index in [0u64, 2] {
            let offset = index as usize * STATE_RECORD as usize;
            state[offset..offset + STATE_RECORD as usize]
                .copy_from_slice(&chunk_len(total, index).to_le_bytes());
        }
        std::fs::write(state_path(&path), &state).unwrap();

        let shared = Shared::new();
        assert!(try_start(&shared, url, path.clone()));

        assert_eq!(
            wait_until_idle(&shared, Duration::from_secs(30)),
            Outcome::Done
        );
        // 续传后文件必须完整且无错位
        assert_eq!(std::fs::read(&path).unwrap(), body);
    }

    #[test]
    fn chunked_download_can_be_paused_and_resumed() {
        let body = payload(6 * 1024 * 1024);
        // 慢一点，好让我们在中途暂停
        let url = spawn_server(body.clone(), true, Duration::from_millis(3));
        let path = temp_path("chunked-pause.bin");

        let shared = Shared::new();
        assert!(try_start(&shared, url.clone(), path.clone()));
        std::thread::sleep(Duration::from_millis(200));
        pause(&shared);

        assert_eq!(
            wait_until_idle(&shared, Duration::from_secs(15)),
            Outcome::Paused
        );
        assert!(state_path(&path).exists(), "暂停时进度文件必须保留");

        // 继续：应该接着下完
        assert!(try_start(&shared, url, path.clone()));
        assert_eq!(
            wait_until_idle(&shared, Duration::from_secs(60)),
            Outcome::Done
        );
        assert_eq!(std::fs::read(&path).unwrap(), body);
    }

    #[test]
    fn server_without_range_support_falls_back_to_single_connection() {
        let body = payload(5 * 1024 * 1024);
        let url = spawn_server(body.clone(), false, Duration::ZERO);
        let path = temp_path("no-range.bin");

        let shared = Shared::new();
        assert!(try_start(&shared, url, path.clone()));

        assert_eq!(
            wait_until_idle(&shared, Duration::from_secs(30)),
            Outcome::Done
        );
        assert_eq!(std::fs::read(&path).unwrap(), body);
    }

    #[test]
    fn chunk_range_covers_the_file_without_gaps_or_overlap() {
        for total in [
            1u64,
            CHUNK_SIZE - 1,
            CHUNK_SIZE,
            CHUNK_SIZE + 1,
            9 * 1024 * 1024,
            3 * CHUNK_SIZE + 12345,
        ] {
            let count = total.div_ceil(CHUNK_SIZE);
            let mut cursor = 0;

            for index in 0..count {
                let (start, end) = chunk_range(total, index);
                assert_eq!(start, cursor, "第 {index} 块起点不连续");
                assert!(end > start, "第 {index} 块是空的");
                cursor = end;
            }

            assert_eq!(cursor, total, "分块总和必须等于文件大小");
        }
    }

    #[test]
    fn chunk_state_round_trips_through_the_file() {
        let path = temp_path("state.bin");
        File::create(&path)
            .unwrap()
            .set_len(3 * CHUNK_SIZE)
            .unwrap();
        let total = 3 * CHUNK_SIZE;

        {
            let mut state = ChunkState::open(&path, total).unwrap();
            assert!(!state.is_done(1));
            state.set_progress(0, CHUNK_SIZE).unwrap();
            state.set_progress(2, CHUNK_SIZE).unwrap();
            assert_eq!(state.downloaded(), 2 * CHUNK_SIZE);
            assert!(!state.all_done());
        }

        // 重新打开应该能读回刚才写下的进度
        let mut state = ChunkState::open(&path, total).unwrap();
        assert!(state.is_done(0));
        assert!(!state.is_done(1));
        assert!(state.is_done(2));

        state.discard();
        assert!(!state_path(&path).exists());
    }

    #[test]
    fn content_disposition_file_name_is_parsed() {
        assert_eq!(
            parse_file_name("attachment; filename=\"a.zip\"").as_deref(),
            Some("a.zip")
        );
        assert_eq!(
            parse_file_name("attachment; filename=b.tar.gz").as_deref(),
            Some("b.tar.gz")
        );
        // RFC 5987
        assert_eq!(
            parse_file_name("attachment; filename*=UTF-8''%E4%B8%AD%E6%96%87.txt").as_deref(),
            Some("中文.txt")
        );
        assert_eq!(parse_file_name("inline"), None);
    }

    #[test]
    fn sanitize_file_name_rejects_illegal_characters() {
        assert_eq!(sanitize_file_name("a:b*c.bin"), "a_b_c.bin");
        assert_eq!(sanitize_file_name("..."), "download.bin");
        assert_eq!(sanitize_file_name(""), "download.bin");
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
    fn format_progress_shows_downloaded_over_total() {
        assert_eq!(format_progress(0, 2048), "0 B / 2.0 KB");
        assert_eq!(format_progress(1024, 2048), "1.0 KB / 2.0 KB");
        assert_eq!(format_progress(500, 0), "500 B / 未知");
    }

    /// 旧版本（每块 1 字节的位图）留下的 `.dlstate` 必须被拒绝，而不是被当 u64 读。
    ///
    /// 误读的后果很严重：位图里的 `1` 会变成天文数字般的 u64，从而把没下过的块
    /// 判成已完成，续传时直接跳过 —— 文件永远缺那几块，而且全程不报错。
    #[test]
    fn chunk_state_ignores_an_old_style_bitmap_file() {
        let path = temp_path("old-bitmap.bin");
        File::create(&path)
            .unwrap()
            .set_len(3 * CHUNK_SIZE)
            .unwrap();
        let total = 3 * CHUNK_SIZE;

        // 旧格式：3 块 -> 3 个字节，全填 1（"三块都下完了"）
        std::fs::write(state_path(&path), [1u8, 1, 1]).unwrap();

        let state = ChunkState::open(&path, total).unwrap();
        assert_eq!(state.downloaded(), 0, "旧格式必须被整体忽略");
        assert!(!state.is_done(0), "不能把没下过的块判成已完成");
        // 打开后文件已经被重写成新格式的空进度
        assert_eq!(
            std::fs::metadata(state_path(&path)).unwrap().len(),
            3 * STATE_RECORD
        );
    }

    /// 分块进度必须**按字节**记住，不能只记"整块下完没有"。
    ///
    /// 用户按暂停时，一块 2MB 基本都没下完；如果进度文件只记完成状态，
    /// 那些已经写进文件的字节会被判成"没下过"——进度条跳回 0，整份重下。
    #[test]
    fn chunk_state_remembers_partial_progress_within_a_chunk() {
        let path = temp_path("partial-state.bin");
        File::create(&path)
            .unwrap()
            .set_len(3 * CHUNK_SIZE)
            .unwrap();
        let total = 3 * CHUNK_SIZE;

        // 模拟"第 0 块下到一半、第 1 块没动、第 2 块下完就暂停"
        {
            let mut state = ChunkState::open(&path, total).unwrap();
            state.set_progress(0, 300 * 1024).unwrap();
            state.set_progress(2, CHUNK_SIZE).unwrap();
        }
        assert_eq!(
            std::fs::metadata(state_path(&path)).unwrap().len(),
            3 * STATE_RECORD,
            "进度文件应该按 块数×8 预分配"
        );

        let state = ChunkState::open(&path, total).unwrap();
        assert_eq!(state.saved(0), 300 * 1024, "半截进度也必须记住");
        assert!(!state.is_done(0));
        assert_eq!(state.saved(1), 0);
        assert!(state.is_done(2));
        assert_eq!(state.downloaded(), 300 * 1024 + CHUNK_SIZE);
    }

    /// 上一条的端到端版本：带着"半截进度"启动，已落盘的字节不能再请求一次。
    ///
    /// 第 0 块已经整块下完，第 1 块下到一半 —— 续传应该只请求第 1 块的后半截 + 其余块。
    #[test]
    fn chunked_download_resumes_from_the_exact_byte_within_a_chunk() {
        let body = payload(9 * 1024 * 1024);
        let total = body.len() as u64;
        let (url, ranges) = spawn_server_logged(body.clone(), true, Duration::ZERO);
        let path = temp_path("chunked-mid-chunk.bin");

        // 造现场：第 0 块整块落盘，第 1 块只落盘前 300KB
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .unwrap();
        file.set_len(total).unwrap();
        let (start, end) = chunk_range(total, 0);
        file.seek(SeekFrom::Start(start)).unwrap();
        file.write_all(&body[start as usize..end as usize]).unwrap();
        let (start, _) = chunk_range(total, 1);
        file.seek(SeekFrom::Start(start)).unwrap();
        file.write_all(&body[start as usize..start as usize + 300 * 1024])
            .unwrap();
        drop(file);
        {
            let mut state = ChunkState::open(&path, total).unwrap();
            state.set_progress(0, CHUNK_SIZE).unwrap();
            state.set_progress(1, 300 * 1024).unwrap();
        }

        let shared = Shared::new();
        assert!(try_start(&shared, url, path.clone()));
        assert_eq!(
            wait_until_idle(&shared, Duration::from_secs(30)),
            Outcome::Done
        );
        assert_eq!(std::fs::read(&path).unwrap(), body);

        // 第 1 块必须从 300KB 处接着下，而不是从块起点重来
        let (chunk1_start, _) = chunk_range(total, 1);
        let started_at: Vec<u64> = ranges
            .lock()
            .unwrap()
            .iter()
            .filter_map(|range| match range {
                SeenRange::From(start) | SeenRange::Between(start, _) => Some(*start as u64),
                SeenRange::None => None,
            })
            .collect();
        assert!(
            !started_at.contains(&chunk1_start),
            "第 1 块前 300KB 已经落盘了，不应该从块起点重新请求"
        );
        assert!(
            started_at.contains(&(chunk1_start + 300 * 1024)),
            "续传应该从块内的断点开始"
        );
    }

    /// 端到端：真实地"跑一半 → 暂停 → 继续"，测量续传这一轮到底重新下了多少字节。
    ///
    /// 这是用户报告的原始场景。断言写得很硬：续传只应该补没下完的部分，
    /// 一旦重新下一遍全量（serve == total），就是 bug 复发了。
    #[test]
    fn pause_then_resume_only_downloads_the_missing_bytes() {
        let body = payload(9 * 1024 * 1024);
        let total = body.len() as u64;
        // 每 8KB 停 10ms：一块 2MB 要 ~2.5s，所以暂停时一块都下不完，
        // 正好覆盖"只记整块完成就丢光进度"的那种时刻。
        let (url, ranges) = spawn_server_logged(body.clone(), true, Duration::from_millis(10));
        let path = temp_path("pause-resume-bytes.bin");

        let shared = Shared::new();
        assert!(try_start(&shared, url.clone(), path.clone()));
        std::thread::sleep(Duration::from_millis(300));
        pause(&shared);
        assert_eq!(
            wait_until_idle(&shared, Duration::from_secs(30)),
            Outcome::Paused
        );

        let paused_at = shared.down_size();
        assert!(paused_at > 0, "暂停时应该已经有数据落盘");
        assert!(paused_at < total, "300ms 内不应该已经下完");

        ranges.lock().unwrap().clear();
        assert!(try_start(&shared, url, path.clone()));
        assert_eq!(
            wait_until_idle(&shared, Duration::from_secs(120)),
            Outcome::Done
        );
        assert_eq!(std::fs::read(&path).unwrap(), body, "续传后必须完整无错位");

        // 统计续传这一轮服务器实际发出的字节数（`bytes=0-0` 的探测不算）
        let served: u64 = ranges
            .lock()
            .unwrap()
            .iter()
            .filter_map(|range| match range {
                SeenRange::From(start) => Some(total - *start as u64),
                SeenRange::Between(start, end) if end > start => Some((end - start + 1) as u64),
                _ => None,
            })
            .sum();

        assert!(
            served < total,
            "续传又把整份文件下了一遍（暂停时已经落盘 {paused_at} 字节）"
        );
        assert_eq!(
            served,
            total - paused_at,
            "续传应该只补没下完的字节：暂停时落盘 {paused_at}，总共 {total}"
        );
    }

    /// 服务器不支持 Range 时，"续传"只能从头来 —— 这是协议限制，不是 bug。
    /// 但必须给用户一个明确提示，否则看起来就跟"Resume 又从头下载"一模一样。
    #[test]
    fn resuming_against_a_server_without_range_reports_a_notice() {
        let body = payload(800 * 1024);
        let url = spawn_server(body.clone(), false, Duration::from_millis(5));
        let path = temp_path("no-range-resume.bin");

        let shared = Shared::new();
        assert!(try_start(&shared, url.clone(), path.clone()));
        std::thread::sleep(Duration::from_millis(150));
        pause(&shared);
        assert_eq!(
            wait_until_idle(&shared, Duration::from_secs(10)),
            Outcome::Paused
        );
        let partial = std::fs::metadata(&path).unwrap().len();
        assert!(partial > 0 && partial < body.len() as u64);

        assert!(try_start(&shared, url, path.clone()));
        assert_eq!(
            wait_until_idle(&shared, Duration::from_secs(20)),
            Outcome::Done
        );
        assert_eq!(std::fs::read(&path).unwrap(), body);
        assert_eq!(
            shared.take_notice().as_deref(),
            Some("服务器不支持断点续传，已从头开始下载")
        );
    }

    /// 单连接路径的暂停 → 续传：必须带一个非零起点的 Range，而不是从 0 重下。
    #[test]
    fn single_connection_download_resumes_instead_of_restarting() {
        // 800KB < CHUNK_SIZE，而且服务器支持 Range，所以第一次会走单连接
        let body = payload(800 * 1024);
        let (url, ranges) = spawn_server_logged(body.clone(), true, Duration::from_millis(5));
        let path = temp_path("single-resume.bin");

        let shared = Shared::new();
        assert!(try_start(&shared, url.clone(), path.clone()));
        std::thread::sleep(Duration::from_millis(200));
        pause(&shared);
        assert_eq!(
            wait_until_idle(&shared, Duration::from_secs(10)),
            Outcome::Paused
        );

        let partial = std::fs::metadata(&path).unwrap().len();
        assert!(partial > 0, "暂停前应该已经写入了一些数据");
        assert!(
            partial < body.len() as u64,
            "200ms 内不应该已经下完，否则这条测试没意义"
        );

        ranges.lock().unwrap().clear();
        assert!(try_start(&shared, url, path.clone()));
        assert_eq!(
            wait_until_idle(&shared, Duration::from_secs(20)),
            Outcome::Done
        );
        assert_eq!(std::fs::read(&path).unwrap(), body, "续传后必须完整无错位");

        // 续传请求必须从中间开始（探测请求是 bytes=0-0，起点 0 会被这里压过去）
        let resumed_from = ranges
            .lock()
            .unwrap()
            .iter()
            .filter_map(|range| match range {
                SeenRange::From(start) | SeenRange::Between(start, _) => Some(*start),
                SeenRange::None => None,
            })
            .max();
        assert_eq!(
            resumed_from,
            Some(partial as usize),
            "续传应该正好从已落盘的位置开始，而不是从头再来"
        );
    }

    /// 手工验证用：`cargo test -p download-manager -- --ignored`
    ///
    /// 上面所有测试走的都是 127.0.0.1 的明文 HTTP，碰不到 TLS 和代理。
    /// 换 HTTP 客户端时最容易坏的就是这两层，所以留一个默认不跑的真实请求。
    #[test]
    #[ignore = "会访问外网，只在手工验证 TLS / 代理时用 --ignored 跑"]
    fn real_https_download_works() {
        let path = temp_path("real-https.html");
        let shared = Shared::new();

        assert!(try_start(
            &shared,
            "https://example.com/".to_string(),
            path.clone()
        ));
        assert_eq!(
            wait_until_idle(&shared, Duration::from_secs(60)),
            Outcome::Done
        );

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("Example Domain"), "拿到的不是期望的页面");
    }
}
