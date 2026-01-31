use crate::core::types::TaskMetadata;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};

#[derive(Clone)]
pub struct FileManager;

impl FileManager {
    /// Initialize the target file with specific size
    pub async fn init_file(path: &Path, size: u64) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let file = File::create(path).await?;
        file.set_len(size).await?;
        Ok(())
    }

    /// Write data to a specific offset in the file
    pub async fn write_chunk(path: &Path, offset: u64, data: &[u8]) -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .open(path)
            .await
            .context("无法打开文件进行写入")?;

        file.seek(SeekFrom::Start(offset)).await?;
        file.write_all(data).await?;
        file.flush().await?;
        Ok(())
    }

    /// Save metadata to a sidecar file (.meta)
    pub async fn save_metadata(path: &Path, metadata: &TaskMetadata) -> Result<()> {
        let meta_path = Self::get_meta_path(path);
        let json = serde_json::to_string(metadata)?;
        let mut file = File::create(meta_path).await?;
        file.write_all(json.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    /// Load metadata from sidecar file
    pub async fn load_metadata(path: &Path) -> Result<Option<TaskMetadata>> {
        let meta_path = Self::get_meta_path(path);
        if !meta_path.exists() {
            return Ok(None);
        }

        let mut file = File::open(meta_path).await?;
        let mut content = String::new();
        file.read_to_string(&mut content).await?;

        let metadata: TaskMetadata = serde_json::from_str(&content)?;
        Ok(Some(metadata))
    }

    /// Delete metadata file
    pub async fn delete_metadata(path: &Path) -> Result<()> {
        let meta_path = Self::get_meta_path(path);
        if meta_path.exists() {
            fs::remove_file(meta_path).await?;
        }
        Ok(())
    }

    pub async fn sync_file(path: &Path) -> Result<()> {
        let file = File::open(path).await?;
        file.sync_all().await?;
        Ok(())
    }

    fn get_meta_path(path: &Path) -> PathBuf {
        let mut meta_path = path.to_path_buf();
        if let Some(file_name) = path.file_name() {
            let mut name = file_name.to_os_string();
            name.push(".meta");
            meta_path.set_file_name(name);
        }
        meta_path
    }
}
