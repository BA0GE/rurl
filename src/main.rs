use clap::{CommandFactory, Parser, builder::styling::{AnsiColor, Effects, Styles}};
use rurl::api::create_router;
use rurl::core::downloader::DownloadEngine;
use rurl::core::types::{ChunkStatus, DownloadTaskConfig, TaskStatus};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle, ProgressDrawTarget, BinaryBytes, ProgressState};
use reqwest::{Url, Client, redirect::Policy};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::fmt;
use std::collections::HashMap;
use parking_lot::RwLock;
use std::io::{self, Write, IsTerminal, BufRead};

static GLOBAL_MULTI_PROGRESS: RwLock<Option<MultiProgress>> = RwLock::new(None);

struct ProgressLogWriter;

impl io::Write for ProgressLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let guard = GLOBAL_MULTI_PROGRESS.read();
        if let Some(mp) = &*guard {
            mp.suspend(|| {
                io::stderr().write_all(buf).ok();
            });
        } else {
            io::stderr().write_all(buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::fs;
use tokio::time::{sleep, Duration};
use tracing::{info, error, debug};
use futures::StreamExt;

const AFTER_HELP: &str = "\
使用示例:

  1. 基本下载:
     rurl https://example.com/file.zip

  2. 指定输出文件名:
     rurl https://example.com/file.zip -o my_file.zip

  3. 多线程下载 (例如 16 线程):
     rurl https://example.com/file.zip -t 16

  4. 断点续传 (自动检测):
     rurl https://example.com/file.zip

  5. 使用管道 (Streaming):
     rurl https://example.com/stream -o - | tar -xvf -

  6. 发送 JSON 数据:
     rurl https://api.example.com/data --json '{\"key\":\"value\"}'

  7. 自定义 Header 和 User-Agent:
     rurl https://example.com/file -H \"Authorization: Bearer token\" -A \"MyAgent/1.0\"

  8. 限制下载速度 (例如 5MB/s):
     rurl https://example.com/file.zip --limit-bps 5m
";

fn styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::Green.on_default().effects(Effects::BOLD))
        .usage(AnsiColor::Green.on_default().effects(Effects::BOLD))
        .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
        .placeholder(AnsiColor::Cyan.on_default())
}

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let s_lower = s.to_lowercase();
    
    let (num_str, multiplier) = if s_lower.ends_with("kb") {
        (&s[..s.len()-2], 1024)
    } else if s_lower.ends_with("k") {
        (&s[..s.len()-1], 1024)
    } else if s_lower.ends_with("mb") {
        (&s[..s.len()-2], 1024 * 1024)
    } else if s_lower.ends_with("m") {
        (&s[..s.len()-1], 1024 * 1024)
    } else if s_lower.ends_with("gb") {
        (&s[..s.len()-2], 1024 * 1024 * 1024)
    } else if s_lower.ends_with("g") {
        (&s[..s.len()-1], 1024 * 1024 * 1024)
    } else {
        (s, 1)
    };

    match num_str.trim().parse::<f64>() {
        Ok(n) => Ok((n * multiplier as f64) as u64),
        Err(_) => Err(format!("无效的大小格式: {}", s)),
    }
}

#[derive(Parser, Debug)]
#[command(
    author, 
    version, 
    about = "rurl - 极速多线程下载器", 
    after_help = AFTER_HELP, 
    styles = styles(),
    disable_help_flag = true,
    disable_version_flag = true,
    override_usage = "rurl [选项] [链接]...",
    help_template = "\
{name} {version}
{author-with-newline}{about-with-newline}
用法: {usage}

{after-help}

{all-args}
"
)]
struct Cli {
    /// 下载链接 (支持多个镜像链接)
    #[arg(required = false, help_heading = "参数", value_name = "链接")]
    urls: Vec<String>,

    /// 输出文件路径 (下载时必填)
    #[arg(short = 'o', long, help_heading = "选项", value_name = "路径")]
    output: Option<PathBuf>,

    /// 使用远程文件名 (类似于 curl -O)
    #[arg(short = 'O', long, alias = "remote-name", help_heading = "选项")]
    remote_name: bool,

    /// 最大线程数 (默认: 4)
    #[arg(short = 't', long, default_value_t = 4, help_heading = "选项", hide_default_value = true, value_name = "数量")]
    max_threads: u32,

    /// 最小分块大小 (例如 1m, 1024) (默认: 1m)
    #[arg(long, default_value = "1m", value_parser = parse_size, help_heading = "选项", hide_default_value = true, value_name = "大小")]
    min_chunk_size: u64,

    /// 禁用动态分块 (工作窃取)
    #[arg(long, default_value_t = false, help_heading = "选项")]
    no_dynamic_split: bool,

    /// 限速 (例如 1m, 500k)
    #[arg(short = 'l', long, value_parser = parse_size, alias = "limit-rate", help_heading = "选项", value_name = "速度")]
    limit_bps: Option<u64>,

    /// 启动 Web API 服务器
    #[arg(long, help_heading = "选项")]
    server: bool,

    /// Web API 端口 (默认: 3000)
    #[arg(short = 'P', long, default_value_t = 3000, help_heading = "选项", hide_default_value = true, value_name = "端口")]
    port: u16,

    /// User Agent (用户代理)
    #[arg(short = 'A', long, alias = "user-agent", help_heading = "选项", value_name = "UA字符串")]
    user_agent: Option<String>,

    /// 代理地址 (http://... 或 socks5://...)
    #[arg(short = 'x', long, alias = "proxy", help_heading = "选项", value_name = "URL")]
    proxy: Option<String>,

    /// 自定义 Header (格式 "Key: Value"，可多次使用)
    #[arg(short = 'H', long, alias = "header", help_heading = "选项", value_name = "键值对")]
    header: Vec<String>,

    /// 文件校验和 (支持 md5, sha1, sha256, 长度自动识别)
    #[arg(long, alias = "checksum", help_heading = "选项", value_name = "哈希值")]
    checksum: Option<String>,

    /// 最大重试次数 (默认: 10)
    #[arg(short = 'r', long, default_value_t = 10, alias = "retry", help_heading = "选项", hide_default_value = true, value_name = "次数")]
    max_retries: u32,

    /// 认证信息 (user:pass)
    #[arg(short = 'u', long, alias = "user", help_heading = "选项", value_name = "用户:密码")]
    auth: Option<String>,

    /// 强制覆盖已存在的文件
    #[arg(long, help_heading = "选项")]
    force: bool,

    /// HTTP 数据 Body (implies POST)
    #[arg(short = 'd', long, alias = "data", help_heading = "选项", value_name = "数据")]
    data: Option<String>,

    /// JSON 数据 Body (implies POST + Content-Type: application/json)
    #[arg(long, help_heading = "选项", value_name = "JSON")]
    json: Option<String>,

    /// HTTP 请求方法 (GET, POST, etc.)
    #[arg(short = 'X', long, alias = "request", help_heading = "选项", value_name = "方法")]
    method: Option<String>,

    /// 允许不安全的 SSL 连接
    #[arg(short = 'k', long, alias = "insecure", help_heading = "选项")]
    insecure: bool,

    /// 允许重定向 (默认不跟随)
    #[arg(short = 'L', long, alias = "location", help_heading = "选项")]
    location: bool,

    /// 启用压缩 (gzip/brotli/deflate)
    #[arg(long, help_heading = "选项")]
    compressed: bool,

    /// 包含 HTTP 头信息 (在输出中)
    #[arg(short = 'i', long, alias = "include", help_heading = "选项")]
    include: bool,

    /// 静默模式 (不显示进度条)
    #[arg(short = 's', long, alias = "silent", help_heading = "选项")]
    silent: bool,

    /// 仅获取 HTTP 头 (HEAD 请求)
    #[arg(short = 'I', long, alias = "head", help_heading = "选项")]
    head: bool,

    /// 显示详细日志 (调试模式)
    #[arg(short = 'v', long, alias = "verbose", help_heading = "选项")]
    verbose: bool,

    /// 显示帮助信息
    #[arg(short = 'h', long, action = clap::ArgAction::Help, help_heading = "选项")]
    help: Option<bool>,

    /// 显示版本信息
    #[arg(short = 'V', long, action = clap::ArgAction::Version, help_heading = "选项")]
    version: Option<bool>,
}

#[tokio::main]
async fn main() {
    let mut cli = Cli::parse();

    let log_level = if cli.verbose { "rurl=debug" } else { "rurl=info" };
    
    // Initialize tracing
    // Use ProgressLogWriter to ensure logs don't interfere with progress bars
    let subscriber = tracing_subscriber::fmt()
        .with_writer(|| ProgressLogWriter)
        .with_env_filter(log_level)
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    let engine = Arc::new(DownloadEngine::new());

    let server_handle = if cli.server {
        let engine_clone = engine.clone();
        let port = cli.port;
        Some(tokio::spawn(async move {
            let app = create_router(engine_clone);
            let addr = SocketAddr::from(([0, 0, 0, 0], port));
            info!("Web API 监听于 {}", addr);
            let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
            axum::serve(listener, app).await.unwrap();
        }))
    } else {
        None
    };

    if cli.server {
        sleep(Duration::from_millis(100)).await;
    }

    if cli.urls.is_empty() && !std::io::stdin().is_terminal() {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            if let Ok(l) = line {
                let trimmed = l.trim();
                if !trimmed.is_empty() {
                    cli.urls.push(trimmed.to_string());
                }
            }
        }
    }

    if cli.urls.is_empty() && !cli.server {
        Cli::command().print_help().unwrap();
        return;
    }

    // Determine Output Mode
    // Default: stdout, unless -o or -O or server is present.
    // Explicit stdout: -o -
    
    let stdout_mode = if let Some(path) = &cli.output {
        path.to_string_lossy() == "-"
    } else {
        !cli.remote_name && !cli.server
    };

        if !cli.urls.is_empty() {
        let urls: Vec<String> = cli.urls.iter()
            .map(|u| u.trim().trim_matches(|c| c == '\'' || c == '"' || c == '`'))
            .filter(|u| !u.is_empty())
            .map(|u| {
                if !u.starts_with("http://") && !u.starts_with("https://") && !u.starts_with("socks5://") {
                    format!("http://{}", u)
                } else {
                    u.to_string()
                }
            })
            .collect();

        if urls.is_empty() {
            error!("未提供有效的 URL");
            std::process::exit(1);
        }

        if cli.head {
            let url = &urls[0];
            let mut client_builder = Client::builder()
                .danger_accept_invalid_certs(cli.insecure)
                .redirect(if cli.location { Policy::default() } else { Policy::none() });
            
            if cli.compressed {
                client_builder = client_builder.gzip(true).brotli(true).deflate(true);
            } else {
                client_builder = client_builder.no_gzip().no_brotli().no_deflate();
            }

            if let Some(ua) = &cli.user_agent {
                client_builder = client_builder.user_agent(ua);
            }
            if let Some(proxy_url) = &cli.proxy {
                if let Ok(p) = reqwest::Proxy::all(proxy_url) {
                    client_builder = client_builder.proxy(p);
                }
            }
            
            let client = match client_builder.build() {
                Ok(c) => c,
                Err(e) => {
                    error!("构建 HTTP 客户端失败: {}", e);
                    std::process::exit(1);
                }
            };
            
            let mut req = client.head(url);
             if let Some(auth_str) = &cli.auth {
                if let Some((u, p)) = auth_str.split_once(':') {
                    req = req.basic_auth(u, Some(p));
                }
            }
            for h in &cli.header {
                if let Some((k, v)) = h.split_once(':') {
                    if let (Ok(kn), Ok(vn)) = (
                        reqwest::header::HeaderName::from_bytes(k.trim().as_bytes()),
                        reqwest::header::HeaderValue::from_str(v.trim())
                    ) {
                        req = req.header(kn, vn);
                    }
                }
            }
            
            match req.send().await {
                 Ok(res) => {
                     println!("{:?} {}", res.version(), res.status());
                     for (k, v) in res.headers() {
                         println!("{}: {}", k, v.to_str().unwrap_or("[二进制数据]"));
                     }
                 }
                 Err(e) => {
                     error!("请求发送失败: {}", e);
                     std::process::exit(1);
                 }
             }
            return;
        }

        if stdout_mode {
            if urls.len() > 1 {
                error!("管道输出模式下不支持多个 URL");
                return;
            }
            let url = &urls[0];
            
            let mut client_builder = Client::builder()
                .danger_accept_invalid_certs(cli.insecure)
                .redirect(if cli.location { Policy::default() } else { Policy::none() });

            if cli.compressed {
                client_builder = client_builder.gzip(true).brotli(true).deflate(true);
            } else {
                client_builder = client_builder.no_gzip().no_brotli().no_deflate();
            }

            if let Some(ua) = &cli.user_agent {
                client_builder = client_builder.user_agent(ua);
            }
            if let Some(proxy_url) = &cli.proxy {
                if let Ok(p) = reqwest::Proxy::all(proxy_url) {
                    client_builder = client_builder.proxy(p);
                }
            }
            let mut headers_map = reqwest::header::HeaderMap::new();
            for h in &cli.header {
                if let Some((k, v)) = h.split_once(':') {
                    if let (Ok(kn), Ok(vn)) = (
                        reqwest::header::HeaderName::from_bytes(k.trim().as_bytes()),
                        reqwest::header::HeaderValue::from_str(v.trim())
                    ) {
                        headers_map.insert(kn, vn);
                    }
                }
            }
            client_builder = client_builder.default_headers(headers_map);

            let client = match client_builder.build() {
                Ok(c) => c,
                Err(e) => {
                    error!("构建 HTTP 客户端失败: {}", e);
                    return;
                }
            };

            let mut request_builder = client.get(url);
            if let Some(auth_str) = &cli.auth {
                if let Some((u, p)) = auth_str.split_once(':') {
                    request_builder = request_builder.basic_auth(u, Some(p));
                }
            }

            match request_builder.send().await {
                Ok(response) => {
                    if cli.include {
                        println!("{:?} {}", response.version(), response.status());
                        for (k, v) in response.headers() {
                            println!("{}: {}", k, v.to_str().unwrap_or("[二进制数据]"));
                        }
                        println!(); 
                    }

                    if !response.status().is_success() {
                        if response.status().is_redirection() {
                            // Allow 3xx to pass through (body will be printed)
                        } else {
                            // For 4xx/5xx, warn but continue to print body (curl behavior)
                             if !cli.silent {
                                 // Only warn if we are not expecting raw output?
                                 // Actually, let's just log debug/info and proceed.
                                 debug!("HTTP 状态码: {}", response.status());
                             }
                        }
                    }
                    
                    let total_size = response.content_length();

                    // Binary Check
                    if io::stdout().is_terminal() {
                        let content_type = response.headers()
                            .get(reqwest::header::CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("");
                        
                        let is_text = content_type.is_empty() || content_type.starts_with("text/") || content_type.contains("json") || content_type.contains("xml") || content_type.contains("javascript");
                        let explicit_stdout = cli.output.as_ref().map(|p| p.to_string_lossy() == "-").unwrap_or(false);
                        
                        if !explicit_stdout && !is_text {
                            error!("警告：二进制输出可能会打乱您的终端显示。请使用 \"--output -\" 强制输出到终端，或者使用 \"--output <FILE>\" 保存到文件。");
                            return;
                        }
                    }

                    // In stdout mode, we output to stderr for progress bar
                    let pb = ProgressBar::new(total_size.unwrap_or(0));
                    // Hide progress bar if silent is requested OR if we are outputting to a terminal (to avoid clutter)
                    if cli.silent || io::stdout().is_terminal() {
                        pb.set_draw_target(ProgressDrawTarget::hidden());
                    }
                    pb.set_style(ProgressStyle::with_template(
                        "{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {binary_bytes}/{binary_total_bytes} ({binary_bytes_per_sec}) {msg}"
                    ).unwrap().progress_chars("#>-"));
                    
                    let mut stream = response.bytes_stream();
                    let mut stdout = io::stdout();
                    
                    while let Some(item) = stream.next().await {
                        match item {
                            Ok(chunk) => {
                                if let Err(e) = stdout.write_all(&chunk) {
                                    error!("写入 stdout 失败: {}", e);
                                    break;
                                }
                                pb.inc(chunk.len() as u64);
                            }
                            Err(e) => {
                                error!("下载流出错: {}", e);
                                break;
                            }
                        }
                    }
                    pb.finish_with_message("完成");
                }
                Err(e) => error!("请求发送失败: {}", e),
            }
            return;
        }

        // File Download Mode (Multi-threaded)
        let output_path = match cli.output {
            Some(path) => path,
            None => {
                // Determine filename from URL since -O is set (or implied if logic was different, but here strictly -O)
                let url_str = &urls[0];
                match Url::parse(url_str) {
                    Ok(url) => {
                        let filename = url.path_segments()
                            .and_then(|segments| segments.last())
                            .and_then(|name| if name.is_empty() { None } else { Some(name) })
                            .unwrap_or("downloaded_file");
                        PathBuf::from(filename)
                    }
                    Err(_) => {
                         let filename = url_str.split('/').last().unwrap_or("downloaded_file");
                         let filename = if filename.is_empty() { "downloaded_file" } else { filename };
                         let filename = filename.split('?').next().unwrap_or(filename);
                         PathBuf::from(filename)
                    }
                }
            }
        };

        if output_path.exists() {
            let mut meta_name = output_path.file_name().unwrap().to_os_string();
            meta_name.push(".meta");
            let meta_path = output_path.with_file_name(meta_name);
            
            if meta_path.exists() {
                if cli.force {
                    info!("强制模式：删除旧文件和元数据，重新开始下载...");
                    if let Err(e) = fs::remove_file(&output_path).await {
                        error!("删除文件失败: {}", e);
                        return;
                    }
                    if let Err(e) = fs::remove_file(&meta_path).await {
                        error!("删除元数据失败: {}", e);
                        return;
                    }
                } else {
                    info!("检测到未完成的下载任务，准备断点续传...");
                }
            } else {
                if cli.force {
                    info!("强制模式：覆盖已存在的文件...");
                    if let Err(e) = fs::remove_file(&output_path).await {
                        error!("删除文件失败: {}", e);
                        return;
                    }
                } else {
                    print!("文件 '{}' 已存在。是否覆盖？[y/N] ", output_path.display());
                    io::stdout().flush().unwrap();
                    let mut input = String::new();
                    io::stdin().read_line(&mut input).unwrap();
                    if input.trim().eq_ignore_ascii_case("y") {
                        if let Err(e) = fs::remove_file(&output_path).await {
                            error!("删除文件失败: {}", e);
                            return;
                        }
                    } else {
                        info!("已取消下载。");
                        return;
                    }
                }
            }
        }
        
        let mut method = cli.method.unwrap_or_else(|| "GET".to_string());
        let mut body = cli.data;
        let mut headers = cli.header;

        if let Some(json_data) = cli.json {
            if method == "GET" {
                method = "POST".to_string();
            }
            body = Some(json_data);
            headers.push("Content-Type: application/json".to_string());
        } else if body.is_some() {
            if method == "GET" {
                method = "POST".to_string();
            }
            // Add default content type if not present
            let has_ct = headers.iter().any(|h| h.to_lowercase().starts_with("content-type:"));
            if !has_ct {
                headers.push("Content-Type: application/x-www-form-urlencoded".to_string());
            }
        }

        let config = DownloadTaskConfig {
            urls: urls.clone(),
            file_path: output_path.clone(),
            max_threads: cli.max_threads,
            min_chunk_size: cli.min_chunk_size,
            dynamic_split: !cli.no_dynamic_split,
            limit_bps: cli.limit_bps,
            user_agent: cli.user_agent,
            proxy: cli.proxy,
            headers: headers,
            checksum: cli.checksum,
            max_retries: cli.max_retries,
            auth: cli.auth,
            method: method,
            body: body,
            insecure: cli.insecure,
            location: cli.location,
            compressed: cli.compressed,
        };

        debug!("正在启动下载任务...");
        match engine.create_task(config).await {
            Ok(task_id) => {
                debug!("任务已创建，ID: {}", task_id);

                let m = MultiProgress::with_draw_target(
                    if cli.silent {
                        ProgressDrawTarget::hidden()
                    } else {
                        ProgressDrawTarget::stderr_with_hz(10)
                    }
                );
                
                {
                    let mut guard = GLOBAL_MULTI_PROGRESS.write();
                    *guard = Some(m.clone());
                }

                let spinner_style = ProgressStyle::with_template(
                    "{spinner:.green} [{elapsed_precise}] {msg}"
                ).unwrap();

                let initial_downloaded_arc = Arc::new(AtomicU64::new(0));
                let initial_downloaded_style1 = initial_downloaded_arc.clone();
                let initial_downloaded_style2 = initial_downloaded_arc.clone();

                let main_style = ProgressStyle::with_template(
                    "{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {binary_bytes}/{binary_total_bytes} ({my_speed}, {my_eta}) {msg}"
                ).unwrap()
                .with_key("my_speed", move |state: &ProgressState, w: &mut dyn fmt::Write| {
                    let initial = initial_downloaded_style1.load(Ordering::Relaxed);
                    let current = state.pos();
                    let elapsed = state.elapsed().as_secs_f64();
                    if elapsed > 0.0 && current >= initial {
                        let diff = current - initial;
                        let bps = diff as f64 / elapsed;
                        write!(w, "{}/s", BinaryBytes(bps as u64)).unwrap();
                    } else {
                        write!(w, "0 B/s").unwrap();
                    }
                })
                .with_key("my_eta", move |state: &ProgressState, w: &mut dyn fmt::Write| {
                    let initial = initial_downloaded_style2.load(Ordering::Relaxed);
                    let current = state.pos();
                    let elapsed = state.elapsed().as_secs_f64();
                    if elapsed > 0.0 && current >= initial {
                        let diff = current - initial;
                        let bps = diff as f64 / elapsed;
                        if bps > 0.0 {
                             if let Some(len) = state.len() {
                                 let remaining = len.saturating_sub(current);
                                 let secs = remaining as f64 / bps;
                                 let d = Duration::from_secs(secs as u64);
                                 let h = d.as_secs() / 3600;
                                 let m = (d.as_secs() % 3600) / 60;
                                 let s = d.as_secs() % 60;
                                 if h > 0 {
                                     write!(w, "{}时{}分{}秒", h, m, s).unwrap();
                                 } else if m > 0 {
                                     write!(w, "{}分{}秒", m, s).unwrap();
                                 } else {
                                     write!(w, "{}秒", s).unwrap();
                                 }
                             } else {
                                 write!(w, "-").unwrap();
                             }
                        } else {
                             write!(w, "-").unwrap();
                        }
                    } else {
                        write!(w, "-").unwrap();
                    }
                })
                .progress_chars("#>-");

                let main_pb = m.add(ProgressBar::new(0));
                main_pb.set_style(spinner_style);
                main_pb.set_message("正在连接资源...");
                main_pb.enable_steady_tick(Duration::from_millis(100));
                
                let mut first_update = true;
                let mut style_updated = false;

                let mut chunk_bars: HashMap<usize, ProgressBar> = HashMap::new();

                loop {
                    if let Some(handle) = engine.get_task(task_id) {
                        let (status, total, chunks) = {
                            let h = handle.read().await;
                            let m = h.metadata.read().await;
                            (m.status.clone(), m.total_size, m.chunks.clone())
                        };

                        if total > 0 {
                            if !style_updated {
                                main_pb.set_style(main_style.clone());
                                style_updated = true;
                            }
                            if main_pb.length() != Some(total) {
                                main_pb.set_length(total);
                            }
                        }
                        
                        let current_total_downloaded: u64 = chunks.iter().map(|c| c.current - c.start).sum();
                        
                        if first_update && current_total_downloaded > 0 {
                            main_pb.set_position(current_total_downloaded);
                            initial_downloaded_arc.store(current_total_downloaded, Ordering::Relaxed);
                            first_update = false;
                        } else {
                            main_pb.set_position(current_total_downloaded);
                        }
                        
                        let active_chunks = chunks.iter().filter(|c| c.status == ChunkStatus::Downloading).count();
                        main_pb.set_message(format!("| 线程数: {}", active_chunks));

                        // Cleanup finished chunks
                        chunk_bars.retain(|id, pb| {
                            if let Some(chunk) = chunks.iter().find(|c| c.id == *id) {
                                if chunk.status != ChunkStatus::Downloading {
                                    pb.finish_and_clear();
                                    return false;
                                }
                                true
                            } else {
                                pb.finish_and_clear();
                                false
                            }
                        });

                        // Add/Update downloading chunks
                        for chunk in &chunks {
                            if chunk.status == ChunkStatus::Downloading {
                                 let bar = chunk_bars.entry(chunk.id).or_insert_with(|| {
                                     let initial_pos = chunk.current - chunk.start;
                                     let pb = m.add(ProgressBar::new(chunk.end - chunk.start));
                                     pb.set_style(ProgressStyle::with_template("  ├─ [{bar:20.cyan/blue}] {bytes}/{total_bytes} ({chunk_speed}) (线程 {msg})")
                                        .unwrap()
                                        .with_key("chunk_speed", move |state: &ProgressState, w: &mut dyn fmt::Write| {
                                            let current = state.pos();
                                            let elapsed = state.elapsed().as_secs_f64();
                                            if elapsed > 0.0 && current >= initial_pos {
                                                let diff = current - initial_pos;
                                                let bps = diff as f64 / elapsed;
                                                write!(w, "{}/s", BinaryBytes(bps as u64)).unwrap();
                                            } else {
                                                write!(w, "0 B/s").unwrap();
                                            }
                                        })
                                        .progress_chars("#>-"));
                                     pb.set_message(format!("{}", chunk.id));
                                     pb
                                 });
                                 bar.set_position(chunk.current - chunk.start);
                                 let len = chunk.end - chunk.start;
                                 if bar.length() != Some(len) {
                                     bar.set_length(len);
                                 }
                            }
                        }

                        if status == TaskStatus::Completed {
                            main_pb.finish_with_message("| 下载完成");
                            break;
                        } else if let TaskStatus::Error(msg) = status {
                            main_pb.abandon_with_message(format!("下载失败: {}", msg));
                            std::process::exit(1);
                        }
                    } else {
                        break;
                    }
                    sleep(Duration::from_millis(100)).await;
                }
                
                {
                    let mut guard = GLOBAL_MULTI_PROGRESS.write();
                    *guard = None;
                }
            }
            Err(e) => {
                error!("创建下载任务失败: {}", e);
            }
        }
    }

    if let Some(handle) = server_handle {
        handle.await.unwrap();
    }
}
