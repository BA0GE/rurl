use crate::core::types::{ChunkInfo, ChunkStatus};
use crate::core::limiter::RateLimiter;
use crate::storage::FileManager;
use anyhow::{Result, anyhow};
use futures::StreamExt;
use reqwest::{Client, header};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{timeout, Duration};
use tracing::{error, debug, warn};

pub struct Worker {
    id: usize,
    client: Client,
    file_path: PathBuf,
    limiter: Option<RateLimiter>,
    user_agent: Option<String>,
    headers: Vec<String>,
    auth: Option<String>,
    method: String,
    body: Option<String>,
}

impl Worker {
    pub fn new(
        id: usize,
        client: Client,
        file_path: PathBuf,
        limiter: Option<RateLimiter>,
        user_agent: Option<String>,
        headers: Vec<String>,
        auth: Option<String>,
        method: String,
        body: Option<String>,
    ) -> Self {
        Self {
            id,
            client,
            file_path,
            limiter,
            user_agent,
            headers,
            auth,
            method,
            body,
        }
    }

    pub async fn download_chunk(
        &self,
        chunk: Arc<RwLock<ChunkInfo>>,
        url: String,
    ) -> Result<()> {
        // 1. Get initial range
        let (_start, end, current) = {
            let c = chunk.read().await;
            (c.start, c.end, c.current)
        };

        if current > end {
            return Ok(());
        }

        debug!("工作线程 {} 开始下载分块 {} 范围 {}-{}", self.id, {chunk.read().await.id}, current, end);

        // 2. Send Request
        let method = match self.method.as_str() {
            "GET" => reqwest::Method::GET,
            "POST" => reqwest::Method::POST,
            "PUT" => reqwest::Method::PUT,
            "HEAD" => reqwest::Method::HEAD,
            _ => reqwest::Method::GET,
        };

        let mut req = self.client.request(method, &url)
            .header(header::REFERER, "https://im.qq.com/")
            .header(header::ACCEPT, "*/*");
            
        // Only add Range header if we know the size (end != u64::MAX) or if method is GET
        // If end is u64::MAX, it means unknown size (e.g. POST stream).
        // Sending Range: bytes=0-18446744073709551615 might be weird for some servers.
        if end != u64::MAX {
             req = req.header(header::RANGE, format!("bytes={}-{}", current, end));
        } else {
            // For unknown size, maybe we don't send range? Or send 0-?
            // If it's a resume of unknown size, we might need Range: bytes=current-
            if current > 0 {
                req = req.header(header::RANGE, format!("bytes={}-", current));
            }
        }
        
        if let Some(ua) = &self.user_agent {
            req = req.header(header::USER_AGENT, ua);
        }

        // Custom Headers
        for h in &self.headers {
            if let Some((k, v)) = h.split_once(':') {
                 if let (Ok(kn), Ok(vn)) = (header::HeaderName::from_bytes(k.trim().as_bytes()), header::HeaderValue::from_str(v.trim())) {
                     req = req.header(kn, vn);
                 }
            }
        }

        // Auth (Basic Auth)
        if let Some(auth) = &self.auth {
            let parts: Vec<&str> = auth.splitn(2, ':').collect();
            if parts.len() == 2 {
                req = req.basic_auth(parts[0], Some(parts[1]));
            }
        }
        
        // Body
        if let Some(body) = &self.body {
            req = req.body(body.clone());
        }
        
        let res = req.send().await?;
        let status = res.status();
        if !status.is_success() {
             if status.is_redirection() {
                 return Err(anyhow!("检测到重定向 ({})。请使用 -L 或 --location 参数以允许跟随重定向。", status));
             }
             return Err(anyhow!("请求失败，状态码: {}", status));
        }

        let mut stream = res.bytes_stream();
        let mut offset = current;

        // 3. Stream loop
        let result = async {
            loop {
                // Set a 30-second timeout for reading the next chunk from the stream
                // If the server stops sending data (stalled connection), this will timeout and trigger a retry.
                let next_item = timeout(Duration::from_secs(30), stream.next()).await;

                match next_item {
                    Ok(Some(item)) => {
                        let bytes = item?;
                        let len = bytes.len() as u64;

                        // Rate limiting
                        if let Some(limiter) = &self.limiter {
                             limiter.acquire(len).await;
                        }

                        // Check if chunk end has changed (split occurred)
                        let current_end = {
                            let c = chunk.read().await;
                            c.end
                        };

                        if offset > current_end {
                            debug!("工作线程 {} 因分块拆分提前停止。当前偏移 {} > 结束 {}", self.id, offset, current_end);
                            break;
                        }
                        
                        // Truncate bytes if they exceed current_end
                        let mut write_bytes = bytes.clone();
                        if current_end != u64::MAX && offset + len > current_end.saturating_add(1) {
                             let allowed = (current_end.saturating_add(1).saturating_sub(offset)) as usize;
                             if allowed < write_bytes.len() {
                                 write_bytes = write_bytes.slice(0..allowed);
                             }
                        }

                        // Write to file
                        FileManager::write_chunk(&self.file_path, offset, &write_bytes).await?;
                        
                        offset += write_bytes.len() as u64;

                        // Update progress
                        {
                            let mut c = chunk.write().await;
                            c.current = offset;
                            c.status = ChunkStatus::Downloading;
                        }

                        // Check if we reached the end of the chunk
                        if offset > current_end {
                            break;
                        }
                    }
                    Ok(None) => {
                        // Stream finished
                        break;
                    }
                    Err(_) => {
                        // Timeout
                        warn!("工作线程 {} 读取数据超时 (30s)，可能是连接僵死，准备重试...", self.id);
                        return Err(anyhow!("读取数据超时 (30s)"));
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        }.await;

        if let Err(e) = result {
            // Reset status to Pending so it can be retried
            let mut c = chunk.write().await;
            c.status = ChunkStatus::Pending;
            error!("工作线程 {} 分块 {} 下载失败: {:?}", self.id, c.id, e);
            return Err(e);
        }

        // Final check
        {
            let mut c = chunk.write().await;
            if c.end == u64::MAX {
                c.status = ChunkStatus::Completed;
                debug!("工作线程 {} 完成分块 {} (大小未知)", self.id, c.id);
            } else if c.current >= c.end.saturating_add(1) {
                c.status = ChunkStatus::Completed;
                debug!("工作线程 {} 完成分块 {}", self.id, c.id);
            }
        }

        Ok(())
    }
}
