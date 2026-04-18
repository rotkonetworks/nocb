use lru::LruCache;
use parking_lot::RwLock;
use std::num::NonZeroUsize;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct CachedEntry {
    pub content_type: String,
    pub inline_text: Option<String>,
    pub file_path: Option<String>,
    pub compressed: bool,
    pub size_bytes: usize,
    pub timestamp: i64,
    pub mime_type: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl CachedEntry {
    pub fn text(content: String, timestamp: i64, size: usize) -> Self {
        Self {
            content_type: "text".to_string(),
            inline_text: Some(content),
            file_path: None,
            compressed: false,
            size_bytes: size,
            timestamp,
            mime_type: None,
            width: None,
            height: None,
        }
    }

    pub fn text_file(path: String, compressed: bool, timestamp: i64, size: usize) -> Self {
        Self {
            content_type: "text_file".to_string(),
            inline_text: None,
            file_path: Some(path),
            compressed,
            size_bytes: size,
            timestamp,
            mime_type: None,
            width: None,
            height: None,
        }
    }

    pub fn image(
        path: String,
        mime: String,
        width: u32,
        height: u32,
        timestamp: i64,
        size: usize,
    ) -> Self {
        Self {
            content_type: "image".to_string(),
            inline_text: None,
            file_path: Some(path),
            compressed: false,
            size_bytes: size,
            timestamp,
            mime_type: Some(mime),
            width: Some(width),
            height: Some(height),
        }
    }
}

pub struct EntryCache {
    inner: Arc<RwLock<LruCache<String, CachedEntry>>>,
}

impl EntryCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(LruCache::new(
                NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::new(64).unwrap()),
            ))),
        }
    }

    pub fn put(&self, hash: String, entry: CachedEntry) {
        self.inner.write().put(hash, entry);
    }

    pub fn contains(&self, hash: &str) -> bool {
        self.inner.read().contains(hash)
    }

    pub fn remove(&self, hash: &str) -> Option<CachedEntry> {
        self.inner.write().pop(hash)
    }

    pub fn clear(&self) {
        self.inner.write().clear();
    }

    pub fn iter_sorted(&self) -> Vec<(String, CachedEntry)> {
        let cache = self.inner.read();
        let mut entries: Vec<_> = cache.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        entries.sort_by(|a, b| b.1.timestamp.cmp(&a.1.timestamp));
        entries
    }

    pub fn get_hashes(&self) -> Vec<String> {
        self.inner.read().iter().map(|(k, _)| k.clone()).collect()
    }
}
