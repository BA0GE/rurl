use crate::core::limiter::RateLimiter;
use crate::core::scheduler::Scheduler;
use crate::core::types::{ChunkInfo, ChunkStatus, DownloadTaskConfig, TaskStatus, TaskMetadata};
use crate::core::worker::Worker;
use crate::storage::FileManager;
use anyhow::{Result, anyhow};
use dashmap::DashMap;
use reqwest::{Client, header, redirect::Policy};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::task::JoinSet;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tracing::{info, error, warn, debug};
use uuid::Uuid;

pub struct DownloadEngine {
    tasks: Arc<DashMap<Uuid, Arc<RwLock<TaskHandle>>>>,
}

pub struct TaskHandle {
    pub id: Uuid,
    pub config: DownloadTaskConfig,
    pub status: TaskStatus,
    pub metadata: Arc<RwLock<TaskMetadata>>,
    pub limiter: Option<RateLimiter>,
}

impl DownloadEngine {
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(DashMap::new()),
        }
    }

    pub async fn create_task(&self, config: DownloadTaskConfig) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let metadata = Arc::new(RwLock::new(TaskMetadata {
            id,
            config: config.clone(),
            total_size: 0,
            chunks: Vec::new(),
            status: TaskStatus::Pending,
        }));

        let limiter = if let Some(bps) = config.limit_bps {
            Some(RateLimiter::new(bps))
        } else {
            None
        };

        let handle = Arc::new(RwLock::new(TaskHandle {
            id,
            config: config.clone(),
            status: TaskStatus::Pending,
            metadata,
            limiter,
        }));

        self.tasks.insert(id, handle.clone());
        
        let handle_clone = handle.clone();
        let handle_error = handle.clone();
        
        tokio::spawn(async move {
            if let Err(e) = Self::run_task(handle_clone).await {
                error!("任务 {} 失败: {:?}", id, e);
                let mut h = handle_error.write().await;
                h.status = TaskStatus::Error(e.to_string());
                let mut m = h.metadata.write().await;
                m.status = TaskStatus::Error(e.to_string());
            }
        });

        Ok(id)
    }

    pub async fn update_task_config(&self, id: Uuid, new_config: DownloadTaskConfig) -> Result<()> {
        if let Some(handle) = self.tasks.get(&id) {
            let mut h = handle.write().await;
            h.config.max_threads = new_config.max_threads;
            h.config.limit_bps = new_config.limit_bps;
            h.config.user_agent = new_config.user_agent;
            h.config.proxy = new_config.proxy;
            h.config.headers = new_config.headers;
            h.config.checksum = new_config.checksum;
            h.config.max_retries = new_config.max_retries;
            h.config.auth = new_config.auth;
            
            // Sync to metadata
            {
                let mut m = h.metadata.write().await;
                m.config = h.config.clone();
            }
            
            if let Some(bps) = new_config.limit_bps {
                if let Some(limiter) = &h.limiter {
                    limiter.set_rate(bps).await;
                } else {
                    h.limiter = Some(RateLimiter::new(bps));
                }
            }
            Ok(())
        } else {
            Err(anyhow!("未找到任务"))
        }
    }

    async fn run_task(handle: Arc<RwLock<TaskHandle>>) -> Result<()> {
        let (mut config, id, metadata_rw, limiter) = {
            let h = handle.read().await;
            (h.config.clone(), h.id, h.metadata.clone(), h.limiter.clone())
        };

        // Build Client
        let mut client_builder = Client::builder()
            .user_agent(config.user_agent.as_deref().unwrap_or("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"))
            .http1_only(); // Force HTTP/1.1 to avoid HTTP/2 hanging issues with some CDNs

        if config.compressed {
            client_builder = client_builder.gzip(true).brotli(true).deflate(true);
            warn!("启用压缩模式: 将强制使用单线程下载且无法预知文件大小 (进度条可能不准确)");
        } else {
            client_builder = client_builder.no_gzip().no_brotli().no_deflate();
        }

        client_builder = client_builder
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(300)) // Global timeout 5 mins
            .danger_accept_invalid_certs(config.insecure)
            .redirect(if config.location { Policy::default() } else { Policy::none() });

        if let Some(proxy_url) = &config.proxy {
            let proxy = reqwest::Proxy::all(proxy_url)?;
            client_builder = client_builder.proxy(proxy);
        }

        let client = client_builder.build()?;

        let mut chunks_vec = Vec::new();
        let mut is_resume = false;

        if let Ok(Some(saved_meta)) = FileManager::load_metadata(&config.file_path).await {
             info!("恢复任务 {}", id);
             let mut meta = metadata_rw.write().await;
             meta.chunks = saved_meta.chunks.clone();
             meta.total_size = saved_meta.total_size;
             for c in &mut meta.chunks {
                 if c.status == ChunkStatus::Downloading {
                     c.status = ChunkStatus::Pending;
                     // Don't reset current, resume from where we left off
                 }
             }
             
             // Check if main file exists
             if !config.file_path.exists() {
                 warn!("发现元数据但主文件不存在，将重新开始下载");
                 is_resume = false;
                 // Delete orphan metadata
                 let _ = FileManager::delete_metadata(&config.file_path).await;
             } else {
                 chunks_vec = meta.chunks.clone();
                 is_resume = true;
             }
        }

        if !is_resume {
             // Use HEAD or GET or custom method based on config
             // If method is not GET/HEAD, we should probably use that method to get headers.
             // But usually HEAD is sufficient for length.
             // If body is present, we must use the specified method (likely POST).
             // However, `reqwest` HEAD request doesn't support body easily in `client.head()`.
             // We should build a request manually.
             
             // If custom method (POST/PUT) or body exists, we might not want to use HEAD first unless explicitly told so.
             // For safety, if method is GET, use HEAD first. If method is POST, use POST directly (but we need content-length).
             // Wait, if we use POST, we get the whole body stream. We can't just "check size" without consuming it or cancelling.
             // Servers usually don't support "HEAD with POST body" to get response size.
             // So if method is POST, we might be forced to single-threaded stream mode OR just try HEAD first (which might fail or be wrong).
             
             // Strategy:
             // 1. If method is GET, try HEAD first to get size.
             // 2. If method is POST/PUT/etc, just start request. If we can get Content-Length from headers, great.
             //    If not, we stream it (single thread).
             //    Actually, if we send POST, we initiate the download immediately. We can't easily "pause" it to set up chunks unless we abort and restart (which might have side effects).
             
             // Current architecture relies on knowing total_size to create chunks.
             // If we can't get total_size beforehand, we must fallback to single chunk download.
             
             let total_size;
             let accept_ranges;
             
             if config.method == "GET" {
                 // Try HEAD first for GET
                 let mut head_req = client.head(&config.urls[0]);

                 // Add headers to HEAD request
                 for h in &config.headers {
                     if let Some((k, v)) = h.split_once(':') {
                         if let (Ok(kn), Ok(vn)) = (header::HeaderName::from_bytes(k.trim().as_bytes()), header::HeaderValue::from_str(v.trim())) {
                             head_req = head_req.header(kn, vn);
                         }
                     }
                 }
                 
                 // Add Auth to HEAD request
                 if let Some(auth) = &config.auth {
                    if let Some((u, p)) = auth.split_once(':') {
                        head_req = head_req.basic_auth(u, Some(p));
                    }
                 }

                 let head_res = head_req.send().await;
                 if let Ok(res) = head_res {
                     if res.status().is_success() {
                         total_size = res.headers()
                             .get(header::CONTENT_LENGTH)
                             .and_then(|v| v.to_str().ok())
                             .and_then(|v| v.parse::<u64>().ok())
                             .unwrap_or(0);
                         accept_ranges = res.headers()
                             .get(header::ACCEPT_RANGES)
                             .map(|v| v == "bytes")
                             .unwrap_or(false);
                     } else {
                         if res.status().is_redirection() {
                             return Err(anyhow!("HEAD 请求遇到重定向 ({}). 请使用 -L 或 --location 参数以允许跟随重定向。", res.status()));
                         }
                         // HEAD failed, fallback to GET (stream will be handled later? No, we need size now)
                         // Actually, if HEAD fails, we might just try GET and see headers, but that starts download.
                         // For now, let's stick to existing logic: HEAD failure -> Error (or maybe fallback)
                         // But existing logic was: client.head()... return Err if fail.
                         // Let's keep it simple for now.
                         return Err(anyhow!("HEAD 请求失败: {}", res.status()));
                     }
                 } else {
                     return Err(anyhow!("HEAD 请求失败"));
                 }
             } else {
                 // For POST/PUT, we can't really "probe". We have to assume single thread or try to trust Content-Length if we could peek?
                 // But we can't peek without sending request.
                 // If we send request, we consume the body.
                 // So for non-GET, we force single thread and dynamic split off.
                 // We will handle the actual download in the "worker" (which is designed for chunks).
                 // This is tricky: The current engine spawns workers for chunks.
                 // If we have 1 chunk (0-end), the worker will download it.
                 // But the worker issues a NEW request.
                 // So for POST, we will issue the POST request inside the worker.
                 // But we need `total_size` here to initialize the file and chunk info.
                 // If we don't know total_size, we can't use the current `ChunkInfo` logic which requires `end`.
                 // UNLESS we set `end` to u64::MAX?
                 
                 // If we don't know size, we can't use multi-thread.
                 // Let's set total_size to 0 or something to indicate "unknown".
                 // But FileManager::init_file uses total_size.
                 
                 // Hack: Send the request with Range: bytes=0-0 to check if it works?
                 // Many POST endpoints ignore Range.
                 // If we can't determine size, we treat it as single chunk unknown size.
                 
                 // Let's try to do a request (maybe with Range 0-0 if we want to probe?)
                 // Or just assume unknown size.
                 
                 warn!("非 GET 请求，无法预知文件大小，将使用单线程下载。");
                 config.max_threads = 1;
                 config.dynamic_split = false;
                 total_size = 0; // Unknown
                 accept_ranges = false;
             }
             
             // If total_size is 0 (unknown) and not GET, we need to handle it.
             // Current logic: if total_size == 0 -> create empty file and return.
             // We need to change that.
             
             if total_size == 0 && config.method == "GET" {
                 // Existing logic for empty file
                 FileManager::init_file(&config.file_path, 0).await?;
                 let mut meta = metadata_rw.write().await;
                 meta.total_size = 0;
                 meta.status = TaskStatus::Completed;
                 FileManager::save_metadata(&config.file_path, &meta).await?;
                 return Ok(());
             }

             if total_size > 0 {
                FileManager::init_file(&config.file_path, total_size).await?;
                debug!("文件已初始化: {:?}, 大小: {}", config.file_path, total_size);
             } else {
                 // Unknown size (likely POST/chunked encoding)
                 // Just open file for writing (create new)
                 // FileManager::init_file might assume sparse file creation with set_len.
                 // If size 0, it creates empty file. That's fine. We will append?
                 // FileManager::write_chunk uses seek+write.
                 // If we don't know size, we can still write at offset 0.
                 FileManager::init_file(&config.file_path, 0).await?;
                 debug!("文件已初始化 (大小未知): {:?}", config.file_path);
             }
             
             if !accept_ranges && config.method == "GET" {
                 warn!("服务器不支持范围请求 (Range)。强制单线程下载。");
                 config.max_threads = 1;
                 config.dynamic_split = false;
             }

             let chunk_size = if total_size > 0 { total_size / config.max_threads as u64 } else { 0 };
             
             if total_size > 0 {
                for i in 0..config.max_threads {
                    let start = i as u64 * chunk_size;
                    let end = if i == config.max_threads - 1 { total_size - 1 } else { (i + 1) as u64 * chunk_size - 1 };
                    if start <= end {
                       chunks_vec.push(ChunkInfo::new(i as usize, start, end));
                    }
                }
             } else {
                 // Single chunk, unknown end
                 chunks_vec.push(ChunkInfo::new(0, 0, u64::MAX)); 
             }
             
             {
                 let mut meta = metadata_rw.write().await;
                 meta.total_size = total_size;
                 meta.chunks = chunks_vec.clone();
                 meta.status = TaskStatus::Running;
                 FileManager::save_metadata(&config.file_path, &meta).await?;
             }
        }

        let chunks_arc: Vec<Arc<RwLock<ChunkInfo>>> = chunks_vec.into_iter()
            .map(|c| Arc::new(RwLock::new(c)))
            .collect();
        let chunks_shared = Arc::new(RwLock::new(chunks_arc));
        
        let scheduler = Scheduler::new(chunks_shared.clone(), config.min_chunk_size, config.dynamic_split);
        
        let mut join_set = JoinSet::new();
        let mut active_workers = 0;
        let mut url_index = 0;
        
        // Periodic metadata save timer
        let mut last_save_time = std::time::Instant::now();

        loop {
            // Save metadata periodically (every 2 seconds)
            if last_save_time.elapsed() > std::time::Duration::from_secs(2) {
                let mut meta = metadata_rw.write().await;
                // Sync current chunks state to metadata
                let chunks_read = chunks_shared.read().await;
                let mut current_chunks = Vec::new();
                for c in chunks_read.iter() {
                    current_chunks.push(c.read().await.clone());
                }
                meta.chunks = current_chunks;
                
                // Update status if needed
                if meta.status == TaskStatus::Pending {
                     meta.status = TaskStatus::Running;
                }
                
                // Save to file
                if let Err(e) = FileManager::save_metadata(&config.file_path, &meta).await {
                    error!("保存任务 {} 元数据失败: {:?}", id, e);
                }
                last_save_time = std::time::Instant::now();
            }

            // Sync dynamic configuration
            {
                let h = handle.read().await;
                config.max_threads = h.config.max_threads;
                config.user_agent = h.config.user_agent.clone();
                config.headers = h.config.headers.clone();
                config.auth = h.config.auth.clone();
                config.checksum = h.config.checksum.clone();
                config.max_retries = h.config.max_retries;
                // Note: Proxy change requires client rebuild, not supported dynamically yet
            }
            
            let all_done = {
                let chunks = chunks_shared.read().await;
                let mut done = true;
                for c in chunks.iter() {
                    let chunk = c.read().await;
                    if chunk.status != ChunkStatus::Completed {
                        done = false;
                        break;
                    }
                }
                done
            };
            
            if all_done && active_workers == 0 {
                debug!("任务 {} 下载成功完成。", id);

                FileManager::delete_metadata(&config.file_path).await?;
                if let Err(e) = FileManager::sync_file(&config.file_path).await {
                    warn!("文件同步失败: {}", e);
                }
                
                // Checksum Verification
                if let Some(expected) = &config.checksum {
                    info!("正在校验文件完整性...");
                    let expected_lower = expected.to_lowercase();
                    
                    let valid = if expected_lower.len() == 32 {
                        let mut file = File::open(&config.file_path).await?;
                        let mut context = md5::Context::new();
                        let mut buffer = [0u8; 65536]; // 64KB buffer
                        let mut total_read = 0;
                        loop {
                            let n = file.read(&mut buffer).await?;
                            if n == 0 { break; }
                            context.consume(&buffer[..n]);
                            total_read += n;
                        }
                        #[allow(deprecated)]
                        let digest = context.compute();
                        let hex_digest = format!("{:x}", digest);
                        info!("校验完成: 读取 {} 字节, 计算 MD5: {}, 期望: {}", total_read, hex_digest, expected_lower);
                        hex_digest == expected_lower
                    } else if expected_lower.len() == 40 {
                        use sha1::{Sha1, Digest};
                        let mut file = File::open(&config.file_path).await?;
                        let mut hasher = Sha1::new();
                        let mut buffer = [0u8; 65536];
                        loop {
                            let n = file.read(&mut buffer).await?;
                            if n == 0 { break; }
                            hasher.update(&buffer[..n]);
                        }
                        let result = hasher.finalize();
                        format!("{:x}", result) == expected_lower
                    } else if expected_lower.len() == 64 {
                        use sha2::{Sha256, Digest};
                        let mut file = File::open(&config.file_path).await?;
                        let mut hasher = Sha256::new();
                        let mut buffer = [0u8; 65536];
                        loop {
                            let n = file.read(&mut buffer).await?;
                            if n == 0 { break; }
                            hasher.update(&buffer[..n]);
                        }
                        let result = hasher.finalize();
                        format!("{:x}", result) == expected_lower
                    } else {
                        warn!("未知的校验和格式长度: {}", expected_lower.len());
                        false
                    };

                    if !valid {
                        return Err(anyhow!("校验和不匹配！下载可能已损坏。"));
                    }
                    info!("校验和匹配，文件完整。");
                }

                {
                     let mut h = handle.write().await;
                     h.status = TaskStatus::Completed;
                     let mut m = h.metadata.write().await;
                     m.status = TaskStatus::Completed;
                }

                break;
            }

            while active_workers < config.max_threads {
                if let Some(chunk) = scheduler.get_next_chunk().await {
                    let url = config.urls[url_index % config.urls.len()].clone();
                    url_index += 1;
                    
                    let worker = Worker::new(
                        active_workers as usize, 
                        client.clone(), 
                        config.file_path.clone(), 
                        limiter.clone(), 
                        config.user_agent.clone(),
                        config.headers.clone(),
                        config.auth.clone(),
                        config.method.clone(),
                        config.body.clone()
                    );
                    let chunk_clone = chunk.clone();
                    
                    active_workers += 1;
                    join_set.spawn(async move {
                         let chunk_id = chunk_clone.read().await.id;
                         match worker.download_chunk(chunk_clone, url).await {
                             Ok(_) => Ok(chunk_id),
                             Err(e) => Err((chunk_id, e)),
                         }
                    });
                } else {
                    break;
                }
            }
            
            if active_workers > 0 {
                tokio::select! {
                    Some(res) = join_set.join_next() => {
                        active_workers -= 1;
                        match res {
                            Ok(Ok(_)) => {
                                let mut meta = metadata_rw.write().await;
                                let chunks_read = chunks_shared.read().await;
                                let mut meta_chunks = Vec::new();
                                for c in chunks_read.iter() {
                                    meta_chunks.push(c.read().await.clone());
                                }
                                meta.chunks = meta_chunks;
                                FileManager::save_metadata(&config.file_path, &meta).await.ok();
                            }
                            Ok(Err((chunk_id, e))) => {
                                let err_msg = e.to_string();
                                error!("分块 {} 下载出错: {}", chunk_id, err_msg);

                                if err_msg.contains("遇到重定向") {
                                    return Err(anyhow!("任务失败: 检测到重定向 (3xx)。请添加 -L 或 --location 参数以允许跟随重定向。"));
                                }
                                
                                // Handle Retry
                                let chunks = chunks_shared.read().await;
                                
                                for c in chunks.iter() {
                                    let mut chunk = c.write().await;
                                    if chunk.id == chunk_id {
                                        chunk.retries += 1;
                                        if chunk.retries > config.max_retries {
                                            return Err(anyhow!("分块 {} 失败次数过多 ({})，终止任务。", chunk_id, chunk.retries));
                                        }
                                        info!("分块 {} 将重试 (第 {} 次)...", chunk_id, chunk.retries);
                                        chunk.status = ChunkStatus::Pending;
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                error!("线程 Join 错误: {}", e);
                            }
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                         // Periodically update metadata for progress monitoring
                         let mut meta = metadata_rw.write().await;
                         let chunks_read = chunks_shared.read().await;
                         let mut meta_chunks = Vec::new();
                         for c in chunks_read.iter() {
                             meta_chunks.push(c.read().await.clone());
                         }
                         meta.chunks = meta_chunks;
                    }
                }
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        
        Ok(())
    }
    
    pub fn get_task(&self, id: Uuid) -> Option<Arc<RwLock<TaskHandle>>> {
        self.tasks.get(&id).map(|v| v.clone())
    }
}
