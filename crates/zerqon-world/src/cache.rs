use crate::{Chunk, WorldError};
use std::collections::HashMap;
use std::sync::Arc;

pub struct ChunkCache {
    entries: HashMap<(i32, i32), Entry>,
    capacity: usize,
    max_bytes: usize,
    resident_bytes: usize,
    stamp: u64,
}

struct Entry {
    chunk: Arc<Chunk>,
    last_access: u64,
    bytes: usize,
}

impl ChunkCache {
    pub fn new(capacity: usize) -> Result<Self, WorldError> {
        Self::with_limits(capacity, usize::MAX)
    }
    pub fn with_limits(capacity: usize, max_bytes: usize) -> Result<Self, WorldError> {
        if capacity == 0 || max_bytes == 0 {
            return Err(WorldError::InvalidLimits);
        }
        Ok(Self {
            entries: HashMap::new(),
            capacity,
            max_bytes,
            resident_bytes: 0,
            stamp: 0,
        })
    }
    pub fn get(&mut self, x: i32, z: i32) -> Option<Arc<Chunk>> {
        let stamp = self.next_stamp();
        let entry = self.entries.get_mut(&(x, z))?;
        entry.last_access = stamp;
        Some(entry.chunk.clone())
    }
    pub fn insert(&mut self, chunk: Arc<Chunk>) -> Vec<Arc<Chunk>> {
        let bytes = std::mem::size_of::<Chunk>().saturating_add(chunk.heap_bytes());
        if bytes > self.max_bytes {
            return vec![chunk];
        }
        let key = (chunk.x, chunk.z);
        let mut removed = Vec::new();
        if let Some(previous) = self.entries.remove(&key) {
            self.resident_bytes -= previous.bytes;
            removed.push(previous.chunk);
        }
        while self.entries.len() >= self.capacity || self.resident_bytes > self.max_bytes - bytes {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_access)
                .map(|(key, _)| *key);
            if let Some(oldest) = oldest.and_then(|key| self.entries.remove(&key)) {
                self.resident_bytes -= oldest.bytes;
                removed.push(oldest.chunk);
            } else {
                break;
            }
        }
        let last_access = self.next_stamp();
        self.entries.insert(
            key,
            Entry {
                chunk,
                last_access,
                bytes,
            },
        );
        self.resident_bytes += bytes;
        removed
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }
    pub fn snapshot(&self) -> Vec<Arc<Chunk>> {
        self.entries
            .values()
            .map(|entry| Arc::clone(&entry.chunk))
            .collect()
    }

    fn next_stamp(&mut self) -> u64 {
        if self.stamp == u64::MAX {
            let mut entries: Vec<_> = self.entries.values_mut().collect();
            entries.sort_unstable_by_key(|entry| entry.last_access);
            for (index, entry) in entries.into_iter().enumerate() {
                entry.last_access = index as u64;
            }
            self.stamp = self.entries.len() as u64;
        }
        self.stamp += 1;
        self.stamp
    }
}
