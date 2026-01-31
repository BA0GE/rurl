use crate::core::types::{ChunkInfo, ChunkStatus};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::debug;

#[derive(Clone)]
pub struct Scheduler {
    chunks: Arc<RwLock<Vec<Arc<RwLock<ChunkInfo>>>>>,
    min_chunk_size: u64,
    dynamic_split: bool,
}

impl Scheduler {
    pub fn new(chunks: Arc<RwLock<Vec<Arc<RwLock<ChunkInfo>>>>>, min_chunk_size: u64, dynamic_split: bool) -> Self {
        Self {
            chunks,
            min_chunk_size,
            dynamic_split,
        }
    }

    pub async fn get_next_chunk(&self) -> Option<Arc<RwLock<ChunkInfo>>> {
        // 1. Try to find a Pending chunk
        {
            let chunks = self.chunks.read().await;
            for chunk in chunks.iter() {
                let mut c = chunk.write().await;
                if c.status == ChunkStatus::Pending {
                    c.status = ChunkStatus::Downloading;
                    return Some(chunk.clone());
                }
            }
        }

        // 2. If no pending chunks, and dynamic split is enabled, try to steal work
        if self.dynamic_split {
            return self.steal_work().await;
        }

        None
    }

    async fn steal_work(&self) -> Option<Arc<RwLock<ChunkInfo>>> {
        // Find the chunk with the largest remaining bytes
        let chunks_guard = self.chunks.read().await;
        
        let mut best_candidate: Option<(usize, u64)> = None; // (index, remaining)

        for (i, chunk_ref) in chunks_guard.iter().enumerate() {
            let c = chunk_ref.read().await;
            if c.status == ChunkStatus::Downloading {
                let remaining = c.remaining();
                if remaining > 2 * self.min_chunk_size {
                    if let Some((_, max_rem)) = best_candidate {
                        if remaining > max_rem {
                            best_candidate = Some((i, remaining));
                        }
                    } else {
                        best_candidate = Some((i, remaining));
                    }
                }
            }
        }

        if let Some((idx, _)) = best_candidate {
            // Drop read lock on chunks to acquire write lock
            drop(chunks_guard);
            
            // Re-acquire write lock on chunks (needed to add new chunk)
            let mut chunks_write = self.chunks.write().await;
            
            // Re-check the candidate (state might have changed)
            if idx >= chunks_write.len() {
                return None;
            }
            
            let target_chunk = chunks_write[idx].clone();
            let mut c = target_chunk.write().await;
            
            // Double check requirements
            if c.status != ChunkStatus::Downloading {
                return None;
            }
            let remaining = c.remaining();
            if remaining <= 2 * self.min_chunk_size {
                return None;
            }

            // Split!
            let old_end = c.end;
            let current = c.current;
            let mid = current + (old_end - current) / 2;
            
            c.end = mid; // Shrink original chunk
            
            debug!("拆分分块 {}: 范围 {}-{} -> {}-{}", c.id, current, old_end, current, mid);
            
            // Create new chunk
            let new_id = chunks_write.len();
            let new_chunk = ChunkInfo::new(new_id, mid + 1, old_end);
            let new_chunk_arc = Arc::new(RwLock::new(new_chunk));
            
            // Mark as downloading immediately since we are returning it to a worker
            {
                let mut nc = new_chunk_arc.write().await;
                nc.status = ChunkStatus::Downloading;
            }
            
            chunks_write.push(new_chunk_arc.clone());
            
            debug!("创建拆分分块 {}: 范围 {}-{}", new_id, mid + 1, old_end);
            
            return Some(new_chunk_arc);
        }

        None
    }
}
