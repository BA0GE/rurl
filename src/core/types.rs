use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadTaskConfig {
    pub urls: Vec<String>,
    pub file_path: PathBuf,
    pub max_threads: u32,
    pub min_chunk_size: u64,
    pub dynamic_split: bool,
    pub limit_bps: Option<u64>, // Bytes per second
    pub user_agent: Option<String>,
    pub proxy: Option<String>,
    pub headers: Vec<String>,
    pub checksum: Option<String>,
    pub max_retries: u32,
    pub auth: Option<String>, // user:pass
    #[serde(default = "default_method")]
    pub method: String,
    pub body: Option<String>,
    #[serde(default)]
    pub insecure: bool,
    #[serde(default)]
    pub location: bool,
    #[serde(default)]
    pub compressed: bool,
}

fn default_method() -> String {
    "GET".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TaskStatus {
    Pending,
    Running,
    Paused,
    Completed,
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ChunkStatus {
    Pending,
    Downloading,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkInfo {
    pub id: usize,
    pub start: u64,
    pub end: u64, // Inclusive
    pub current: u64, // Current offset (start + downloaded bytes)
    pub status: ChunkStatus,
    pub retries: u32,
}

impl ChunkInfo {
    pub fn new(id: usize, start: u64, end: u64) -> Self {
        Self {
            id,
            start,
            end,
            current: start,
            status: ChunkStatus::Pending,
            retries: 0,
        }
    }

    pub fn len(&self) -> u64 {
        if self.end == u64::MAX {
            return u64::MAX;
        }
        self.end.saturating_sub(self.start).saturating_add(1)
    }

    pub fn remaining(&self) -> u64 {
        if self.end == u64::MAX {
            return u64::MAX;
        }
        if self.current > self.end {
            return 0;
        }
        self.end.saturating_sub(self.current).saturating_add(1)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMetadata {
    pub id: Uuid,
    pub config: DownloadTaskConfig,
    pub total_size: u64,
    pub chunks: Vec<ChunkInfo>,
    pub status: TaskStatus,
}
