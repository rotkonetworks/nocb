mod cache;
mod service;
use cache::{CachedEntry, EntryCache};
use service::{ClipboardRecoveryLayer, ClipboardRouter, Request};

use anyhow::{Context, Result};
use arboard::{Clipboard, ImageData};
use blake3::Hasher;
use parking_lot::RwLock;
use rusqlite::{Connection as SqliteConnection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use zeroize::Zeroize;

#[cfg(unix)]
use sd_notify::NotifyState;

const MAX_INLINE_SIZE: usize = 512;
const POLL_INTERVAL_MS: u64 = 100;
const HASH_PREFIX_LEN: usize = 8;
const MAX_CLIPBOARD_SIZE: usize = 100 * 1024 * 1024; // 100MB
const MAX_IPC_MESSAGE_SIZE: usize = 4096;
const IPC_MAGIC: &[u8] = b"NOCB\x00\x01";
const LRU_CACHE_SIZE: usize = 64;
const CLIPBOARD_TIMEOUT_MS: u64 = 5000; // 5 second timeout for clipboard ops
const WATCHDOG_INTERVAL_SECS: u64 = 30; // Send watchdog ping every 30s
const MAX_REINIT_ATTEMPTS: u32 = 5; // Max clipboard reinits before giving up

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub cache_dir: PathBuf,
    pub max_entries: usize,
    pub max_display_length: usize,
    pub max_print_entries: usize,
    pub blacklist: Vec<String>,
    pub trim_whitespace: bool,
    pub static_entries: Vec<String>,
    pub compress_threshold: usize,
    #[serde(default = "default_max_age_days")]
    pub max_age_days: u32,
}

fn default_max_age_days() -> u32 {
    30
}

impl Default for Config {
    fn default() -> Self {
        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("nocb");

        Self {
            cache_dir,
            max_entries: 10000,
            max_display_length: 200,
            max_print_entries: 1000,
            blacklist: Vec::new(),
            trim_whitespace: true,
            static_entries: Vec::new(),
            compress_threshold: 4096,
            max_age_days: 30,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let config_path = dirs::config_dir()
            .unwrap_or_default()
            .join("nocb")
            .join("config.toml");

        if config_path.exists() {
            let content = fs::read_to_string(&config_path)?;
            Ok(toml::from_str(&content)?)
        } else {
            let config = Self::default();
            if let Some(parent) = config_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&config_path, toml::to_string_pretty(&config)?)?;
            Ok(config)
        }
    }
}

#[derive(Debug, Clone)]
pub enum ContentType {
    Text(String),
    TextFile {
        hash: String,
        compressed: bool,
    },
    Image {
        mime: String,
        hash: String,
        width: u32,
        height: u32,
    },
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub id: Option<i64>,
    pub hash: String,
    pub timestamp: u64,
    pub app_name: String,
    pub content: ContentType,
    pub size_bytes: usize,
}

impl Entry {
    fn new(content: ContentType, app_name: String, hash: String, size: usize) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Self {
            id: None,
            hash,
            timestamp,
            app_name,
            content,
            size_bytes: size,
        }
    }
}

#[derive(Debug)]
pub enum Command {
    Copy(String),
    Exit,
    Clear,
    Prune(Vec<String>),
}

enum ClipboardContent<'a> {
    Text(String),
    Image(ImageData<'a>),
}

pub struct ClipboardManager {
    config: Config,
    db: SqliteConnection,
    clipboard: Arc<RwLock<Option<Clipboard>>>,
    last_clipboard_hash: Option<String>,
    cache: EntryCache,
    clipboard_failures: u32,
    total_reinit_attempts: u32,
}

impl ClipboardManager {
    pub async fn new(config: Config) -> Result<Self> {
        fs::create_dir_all(&config.cache_dir)?;
        fs::create_dir_all(config.cache_dir.join("blobs"))?;

        let db_path = config.cache_dir.join("index.db");
        let db = SqliteConnection::open(&db_path)?;
        Self::init_db(&db)?;

        let clipboard = Clipboard::new().context("Failed to initialize clipboard")?;
        let cache = EntryCache::new(LRU_CACHE_SIZE);

        Ok(Self {
            config,
            db,
            clipboard: Arc::new(RwLock::new(Some(clipboard))),
            last_clipboard_hash: None,
            cache,
            clipboard_failures: 0,
            total_reinit_attempts: 0,
        })
    }

    async fn reinitialize_clipboard(&mut self) -> Result<()> {
        self.total_reinit_attempts += 1;
        eprintln!("Reinitializing clipboard connection (attempt {}/{})...",
            self.total_reinit_attempts, MAX_REINIT_ATTEMPTS);

        if self.total_reinit_attempts > MAX_REINIT_ATTEMPTS {
            eprintln!("Too many clipboard reinit attempts, exiting for systemd restart");
            anyhow::bail!("Clipboard unrecoverable after {} reinit attempts", MAX_REINIT_ATTEMPTS);
        }

        // Drop the old clipboard
        {
            let mut clipboard_guard = self.clipboard.write();
            *clipboard_guard = None;
        }

        // Small delay to let X11/Wayland clean up (async, non-blocking)
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Create new clipboard with timeout to prevent hanging
        let clipboard_result = tokio::time::timeout(
            Duration::from_millis(CLIPBOARD_TIMEOUT_MS),
            tokio::task::spawn_blocking(Clipboard::new)
        ).await;

        match clipboard_result {
            Ok(Ok(Ok(new_clipboard))) => {
                let mut clipboard_guard = self.clipboard.write();
                *clipboard_guard = Some(new_clipboard);
                self.clipboard_failures = 0;
                self.total_reinit_attempts = 0;
                eprintln!("Clipboard reinitialized successfully");
                Ok(())
            }
            Ok(Ok(Err(e))) => {
                eprintln!("Failed to reinitialize clipboard: {}", e);
                Err(anyhow::anyhow!("Clipboard reinitialization failed: {}", e))
            }
            Ok(Err(e)) => {
                eprintln!("Clipboard task panicked: {}", e);
                Err(anyhow::anyhow!("Clipboard task panicked: {}", e))
            }
            Err(_) => {
                eprintln!("Clipboard reinitialization timed out");
                Err(anyhow::anyhow!("Clipboard reinitialization timed out"))
            }
        }
    }

    async fn with_clipboard_async<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Clipboard) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let clipboard = self.clipboard.clone();
        let result = tokio::time::timeout(
            Duration::from_millis(CLIPBOARD_TIMEOUT_MS),
            tokio::task::spawn_blocking(move || {
                let mut guard = clipboard.write();
                match guard.as_mut() {
                    Some(cb) => f(cb),
                    None => anyhow::bail!("Clipboard not available"),
                }
            })
        ).await;

        match result {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => anyhow::bail!("Clipboard task panicked: {}", e),
            Err(_) => anyhow::bail!("Clipboard operation timed out"),
        }
    }

    fn init_db(db: &SqliteConnection) -> Result<()> {
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA cache_size = -64000;
             PRAGMA mmap_size = 268435456;
             PRAGMA temp_store = MEMORY;
             PRAGMA foreign_keys = ON;",
        )?;

        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS entries (
                id INTEGER PRIMARY KEY,
                hash TEXT NOT NULL UNIQUE,
                timestamp INTEGER NOT NULL,
                app_name TEXT NOT NULL,
                content_type TEXT NOT NULL,
                file_path TEXT,
                inline_text TEXT,
                mime_type TEXT,
                size_bytes INTEGER NOT NULL,
                compressed INTEGER DEFAULT 0,
                width INTEGER,
                height INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_timestamp ON entries(timestamp DESC);
            CREATE INDEX IF NOT EXISTS idx_hash ON entries(hash);",
        )?;

        // schema migration for existing databases
        let has_compressed: bool = db.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('entries') WHERE name='compressed'",
            [],
            |row| row.get::<_, i64>(0).map(|count| count > 0),
        )?;

        if !has_compressed {
            db.execute(
                "ALTER TABLE entries ADD COLUMN compressed INTEGER DEFAULT 0",
                [],
            )?;
        }

        let has_width: bool = db.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('entries') WHERE name='width'",
            [],
            |row| row.get::<_, i64>(0).map(|count| count > 0),
        )?;

        if !has_width {
            db.execute("ALTER TABLE entries ADD COLUMN width INTEGER", [])?;
            db.execute("ALTER TABLE entries ADD COLUMN height INTEGER", [])?;
        }

        // FTS5 for full-text search / completion
        let has_fts: bool = db
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='entries_fts'",
                [],
                |row| row.get::<_, i64>(0).map(|count| count > 0),
            )
            .unwrap_or(false);

        if !has_fts {
            // Create FTS5 virtual table
            db.execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
                    hash,
                    content,
                    tokenize='trigram'
                );

                -- Populate FTS from existing entries
                INSERT INTO entries_fts(hash, content)
                SELECT hash, inline_text FROM entries WHERE inline_text IS NOT NULL;

                -- Trigger: insert into FTS when entry added
                CREATE TRIGGER IF NOT EXISTS entries_fts_insert AFTER INSERT ON entries
                WHEN NEW.inline_text IS NOT NULL
                BEGIN
                    INSERT INTO entries_fts(hash, content) VALUES (NEW.hash, NEW.inline_text);
                END;

                -- Trigger: delete from FTS when entry removed
                CREATE TRIGGER IF NOT EXISTS entries_fts_delete AFTER DELETE ON entries
                BEGIN
                    DELETE FROM entries_fts WHERE hash = OLD.hash;
                END;

                -- Trigger: update FTS when entry updated
                CREATE TRIGGER IF NOT EXISTS entries_fts_update AFTER UPDATE ON entries
                WHEN NEW.inline_text IS NOT NULL
                BEGIN
                    DELETE FROM entries_fts WHERE hash = OLD.hash;
                    INSERT INTO entries_fts(hash, content) VALUES (NEW.hash, NEW.inline_text);
                END;",
            )?;
        }

        Ok(())
    }

    pub async fn run_daemon(self) -> Result<()> {
        use tokio::sync::Mutex as AsyncMutex;
        use tower::{ServiceBuilder, ServiceExt};

        let (tx, mut rx) = mpsc::channel(10);

        #[cfg(unix)]
        let sock_path = std::env::temp_dir().join("nocb.sock");

        #[cfg(windows)]
        let sock_path = PathBuf::from(r"\\.\pipe\nocb");

        let cache_dir = self.config.cache_dir.clone();
        let max_entries = self.config.max_entries;
        let max_age_days = self.config.max_age_days;

        let manager = Arc::new(AsyncMutex::new(self));

        let sock_path_clone = sock_path.clone();
        let ipc_handle = tokio::spawn(async move {
            let result = Self::ipc_server(tx, sock_path_clone.clone()).await;
            #[cfg(unix)]
            let _ = std::fs::remove_file(&sock_path_clone);
            result
        });

        let service = ServiceBuilder::new()
            .layer(ClipboardRecoveryLayer::new(manager.clone()))
            .service(ClipboardRouter::new(manager.clone()));

        let mut interval = tokio::time::interval(Duration::from_millis(POLL_INTERVAL_MS));
        let mut cleanup_counter = 0u64;
        let mut watchdog_interval =
            tokio::time::interval(Duration::from_secs(WATCHDOG_INTERVAL_SECS));

        #[cfg(unix)]
        let _ = sd_notify::notify(false, &[NotifyState::Ready]);

        let result: Result<()> = loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(e) = service.clone().oneshot(Request::Poll).await {
                        eprintln!("Fatal poll error: {}", e);
                        break Err(e);
                    }

                    cleanup_counter += 1;
                    if cleanup_counter % 1000 == 0 {
                        let cache_dir = cache_dir.clone();
                        tokio::task::spawn_blocking(move || {
                            let _ = Self::cleanup_old_entries_blocking(&cache_dir, max_entries, max_age_days);
                        });
                    }
                }

                _ = watchdog_interval.tick() => {
                    #[cfg(unix)]
                    let _ = sd_notify::notify(false, &[NotifyState::Watchdog]);
                }

                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { break Ok(()) };
                    let req = match cmd {
                        Command::Exit => break Ok(()),
                        Command::Copy(s) => Request::Copy(s),
                        Command::Clear => Request::Clear,
                        Command::Prune(h) => Request::Prune(h),
                    };
                    if let Err(e) = service.clone().oneshot(req).await {
                        eprintln!("Command error: {}", e);
                    }
                }
            }
        };

        #[cfg(unix)]
        let _ = std::fs::remove_file(&sock_path);

        ipc_handle.abort();

        result
    }

    async fn handle_ipc_command(cmd: &str, tx: &mpsc::Sender<Command>) -> Result<()> {
        let cmd = cmd.trim();
        match cmd {
            cmd if cmd.starts_with("COPY:") => {
                let selection = cmd[5..].to_string();
                tx.send(Command::Copy(selection)).await?;
            }
            cmd if cmd.starts_with("PRUNE:") => {
                let hashes_str = cmd[6..].to_string();
                let hashes: Vec<String> = hashes_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .collect();
                tx.send(Command::Prune(hashes)).await?;
            }
            "CLEAR" => {
                tx.send(Command::Clear).await?;
            }
            _ => {
                eprintln!("Unknown command: {}", cmd);
            }
        }
        Ok(())
    }

    #[cfg(all(unix, target_os = "linux"))]
    fn verify_peer_uid(stream: &tokio::net::UnixStream) -> bool {
        match stream.peer_cred() {
            Ok(cred) => cred.uid() == unsafe { libc::getuid() },
            Err(_) => false,
        }
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    fn verify_peer_uid(_stream: &tokio::net::UnixStream) -> bool {
        true
    }

    async fn ipc_server(tx: mpsc::Sender<Command>, sock_path: PathBuf) -> Result<()> {
        let _ = std::fs::remove_file(&sock_path);

        #[cfg(unix)]
        {
            use tokio::net::UnixListener;

            let listener = UnixListener::bind(&sock_path)?;

            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o700);
            std::fs::set_permissions(&sock_path, perms)?;

            loop {
                let (mut stream, _addr) = listener.accept().await?;

                #[cfg(target_os = "linux")]
                if !Self::verify_peer_uid(&stream) {
                    continue;
                }

                let tx = tx.clone();

                tokio::spawn(async move {
                    let _ = tokio::time::timeout(Duration::from_secs(5), async {
                        use tokio::io::AsyncReadExt;
                        let mut buf = vec![0u8; MAX_IPC_MESSAGE_SIZE];

                        match stream.read(&mut buf).await {
                            Ok(n) if n > IPC_MAGIC.len() => {
                                if &buf[..IPC_MAGIC.len()] != IPC_MAGIC {
                                    return;
                                }

                                if let Ok(cmd) = String::from_utf8(buf[IPC_MAGIC.len()..n].to_vec()) {
                                    let _ = Self::handle_ipc_command(&cmd, &tx).await;
                                }
                            }
                            _ => {}
                        }
                    }).await;
                });
            }
        }

        #[cfg(windows)]
        {
            use tokio::io::AsyncReadExt;
            use tokio::net::windows::named_pipe::{PipeMode, ServerOptions};

            let pipe_name = r"\\.\pipe\nocb";

            loop {
                let mut server = ServerOptions::new()
                    .first_pipe_instance(true)
                    .pipe_mode(PipeMode::Message)
                    .create(pipe_name)?;

                server.connect().await?;
                let tx = tx.clone();

                tokio::spawn(async move {
                    let mut buf = vec![0u8; MAX_IPC_MESSAGE_SIZE];

                    match server.read(&mut buf).await {
                        Ok(n) if n > IPC_MAGIC.len() => {
                            if &buf[..IPC_MAGIC.len()] != IPC_MAGIC {
                                return;
                            }

                            if let Ok(cmd) = String::from_utf8(buf[IPC_MAGIC.len()..n].to_vec()) {
                                let _ = Self::handle_ipc_command(&cmd, &tx).await;
                            }
                        }
                        _ => {}
                    }
                });
            }
        }
    }

    pub async fn send_copy_command(selection: &str) -> Result<()> {
        Self::send_command(&format!("COPY:{}", selection)).await
    }

    pub async fn send_command(cmd: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        use tokio::time::timeout;

        #[cfg(unix)]
        {
            use tokio::net::UnixStream;

            let sock_path = std::env::temp_dir().join("nocb.sock");

            let mut stream = timeout(Duration::from_secs(2), UnixStream::connect(&sock_path))
                .await
                .context("Connection timeout")?
                .context("Failed to connect to daemon")?;

            let mut msg = Vec::with_capacity(IPC_MAGIC.len() + cmd.len());
            msg.extend_from_slice(IPC_MAGIC);
            msg.extend_from_slice(cmd.as_bytes());

            stream.write_all(&msg).await?;
            stream.shutdown().await?;
        }

        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ClientOptions;

            let pipe_name = r"\\.\pipe\nocb";

            let mut client = ClientOptions::new().open(pipe_name)?;

            let mut msg = Vec::with_capacity(IPC_MAGIC.len() + cmd.len());
            msg.extend_from_slice(IPC_MAGIC);
            msg.extend_from_slice(cmd.as_bytes());

            timeout(Duration::from_secs(2), client.write_all(&msg))
                .await
                .context("Write timeout")?
                .context("Failed to write to pipe")?;
        }

        Ok(())
    }

    fn is_clipboard_dead(err: &arboard::Error) -> bool {
        let s = format!("{}", err);
        s.contains("stopped") || s.contains("handler")
    }

    async fn poll_clipboard(&mut self) -> Result<()> {
        const MAX_FAILURES: u32 = 3;

        // Track if we need to reinitialize after releasing the lock
        let mut needs_reinit = false;
        let mut reinit_reason: Option<String> = None;

        let content = {
            // Try non-blocking first, then fall back to brief blocking attempt
            match self.clipboard.try_write() {
                Some(mut clipboard_guard) => {
                    let result = match clipboard_guard.as_mut() {
                        Some(clipboard) => {
                            // Try to get clipboard content
                            match clipboard.get_text() {
                                Ok(text) => {
                                    self.clipboard_failures = 0;
                                    Some(ClipboardContent::Text(text))
                                }
                                Err(text_err) => {
                                    if Self::is_clipboard_dead(&text_err) {
                                        self.clipboard_failures += 1;
                                        reinit_reason = Some(format!("Clipboard error ({}/{}): {}", self.clipboard_failures, MAX_FAILURES, text_err));
                                        if self.clipboard_failures >= MAX_FAILURES {
                                            needs_reinit = true;
                                        }
                                        None
                                    } else {
                                        match clipboard.get_image() {
                                            Ok(img) => {
                                                self.clipboard_failures = 0;
                                                Some(ClipboardContent::Image(img))
                                            }
                                            Err(img_err) => {
                                                if Self::is_clipboard_dead(&img_err) {
                                                    self.clipboard_failures += 1;
                                                    reinit_reason = Some(format!("Clipboard error ({}/{}): {}", self.clipboard_failures, MAX_FAILURES, img_err));
                                                    if self.clipboard_failures >= MAX_FAILURES {
                                                        needs_reinit = true;
                                                    }
                                                    None
                                                } else {
                                                    // Normal "no content" case
                                                    self.clipboard_failures = 0;
                                                    None
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        None => {
                            // Clipboard not initialized
                            needs_reinit = true;
                            reinit_reason = Some("Clipboard not initialized".to_string());
                            None
                        }
                    };
                    result
                }
                None => {
                    // Clipboard is locked by copy operation - skip this poll cycle
                    return Ok(());
                },
            }
        };

        // Handle reinitialization after lock is released
        if let Some(reason) = reinit_reason {
            eprintln!("{}", reason);
        }
        if needs_reinit {
            self.reinitialize_clipboard().await?;
            return Ok(());
        }
        if content.is_none() && self.clipboard_failures > 0 {
            return Ok(());
        }

        if let Some(content) = content {
            let entry = match content {
                ClipboardContent::Text(text) => {
                    if text.trim().is_empty() && self.config.trim_whitespace {
                        return Ok(());
                    }

                    let text = if self.config.trim_whitespace {
                        text.trim().to_string()
                    } else {
                        text
                    };

                    let hash = self.hash_data(text.as_bytes());

                    if self.last_clipboard_hash.as_ref() == Some(&hash) {
                        return Ok(());
                    }

                    if self.entry_exists(&hash)? {
                        self.last_clipboard_hash = Some(hash);
                        return Ok(());
                    }

                    let size = text.len();
                    if size > MAX_CLIPBOARD_SIZE {
                        return Ok(());
                    }

                    let content = if size <= MAX_INLINE_SIZE {
                        ContentType::Text(text)
                    } else {
                        let compressed = size > self.config.compress_threshold;
                        let stored_hash = self.store_text_blob(&hash, &text, compressed)?;
                        ContentType::TextFile {
                            hash: stored_hash,
                            compressed,
                        }
                    };

                    Some(Entry::new(content, "unknown".to_string(), hash, size))
                }
                ClipboardContent::Image(img) => {
                    let (width, height) = (img.width as u32, img.height as u32);
                    let data = self.image_to_png(&img)?;
                    let hash = self.hash_data(&data);

                    if self.last_clipboard_hash.as_ref() == Some(&hash) {
                        return Ok(());
                    }

                    if self.entry_exists(&hash)? {
                        self.last_clipboard_hash = Some(hash);
                        return Ok(());
                    }

                    if data.len() > MAX_CLIPBOARD_SIZE {
                        return Ok(());
                    }

                    let stored_hash = self.store_blob(&hash, &data)?;
                    let content = ContentType::Image {
                        mime: "image/png".to_string(),
                        hash: stored_hash.clone(),
                        width,
                        height,
                    };

                    Some(Entry::new(
                        content,
                        "unknown".to_string(),
                        stored_hash,
                        data.len(),
                    ))
                }
            };

            if let Some(entry) = entry {
                self.last_clipboard_hash = Some(entry.hash.clone());
                self.add_entry(entry).await?;
            }
        } else {
            self.last_clipboard_hash = None;
        }

        Ok(())
    }

    fn entry_exists(&self, hash: &str) -> Result<bool> {
        if self.cache.contains(hash) {
            let _ = self.db.execute(
                "UPDATE entries SET timestamp = ?1 WHERE hash = ?2",
                params![
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64,
                    hash
                ],
            );
            return Ok(true);
        }

        let exists: bool = self
            .db
            .query_row(
                "SELECT 1 FROM entries WHERE hash = ?1",
                params![hash],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);

        Ok(exists)
    }

    fn image_to_png(&self, img: &ImageData) -> Result<Vec<u8>> {
        use image::{ImageBuffer, Rgba};

        let width = img.width as u32;
        let height = img.height as u32;

        let img_buffer =
            ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(width, height, img.bytes.to_vec())
                .context("Failed to create image buffer")?;

        let mut png_data = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut png_data);
        image::ImageEncoder::write_image(
            encoder,
            &img_buffer,
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )?;

        Ok(png_data)
    }

    fn hash_data(&self, data: &[u8]) -> String {
        let mut hasher = Hasher::new();
        hasher.update(data);
        hasher.finalize().to_hex().to_string()
    }

    fn store_blob(&self, hash: &str, data: &[u8]) -> Result<String> {
        if hash.contains('/') || hash.contains('\\') || hash.contains("..") {
            anyhow::bail!("Invalid hash");
        }
        let path = self.config.cache_dir.join("blobs").join(hash);
        if !path.exists() {
            fs::write(&path, data)?;
        }
        Ok(hash.to_string())
    }

    fn store_text_blob(&self, hash: &str, text: &str, compress: bool) -> Result<String> {
        let filename = if compress {
            format!("{}.txt.zst", hash)
        } else {
            format!("{}.txt", hash)
        };

        let path = self.config.cache_dir.join("blobs").join(&filename);
        if !path.exists() {
            if compress {
                use zstd::stream::write::Encoder;
                let file = fs::File::create(&path)?;
                let mut encoder = Encoder::new(file, 3)?;
                encoder.write_all(text.as_bytes())?;
                encoder.finish()?;
            } else {
                fs::write(&path, text)?;
            }
        }
        Ok(hash.to_string())
    }

    async fn add_entry(&mut self, entry: Entry) -> Result<()> {
        if self
            .config
            .blacklist
            .iter()
            .any(|app| entry.app_name.contains(app))
        {
            return Ok(());
        }

        let timestamp = entry.timestamp as i64;
        let cached_entry = match &entry.content {
            ContentType::Text(text) => {
                self.db.execute(
                    "INSERT OR REPLACE INTO entries (hash, timestamp, app_name, content_type, inline_text, size_bytes)
                     VALUES (?1, ?2, ?3, 'text', ?4, ?5)",
                     params![entry.hash, timestamp, entry.app_name, text, entry.size_bytes as i64],
                )?;

                CachedEntry::text(text.clone(), timestamp, entry.size_bytes)
            }
            ContentType::TextFile { hash, compressed } => {
                let file_path = if *compressed {
                    format!("{}.txt.zst", hash)
                } else {
                    format!("{}.txt", hash)
                };
                self.db.execute(
                    "INSERT OR REPLACE INTO entries (hash, timestamp, app_name, content_type, file_path, size_bytes, compressed)
                     VALUES (?1, ?2, ?3, 'text_file', ?4, ?5, ?6)",
                     params![entry.hash, timestamp, entry.app_name, file_path, entry.size_bytes as i64, *compressed as i64],
                )?;

                CachedEntry::text_file(file_path, *compressed, timestamp, entry.size_bytes)
            }
            ContentType::Image {
                mime,
                hash,
                width,
                height,
            } => {
                self.db.execute(
                    "INSERT OR REPLACE INTO entries (hash, timestamp, app_name, content_type, file_path, mime_type, size_bytes, width, height)
                     VALUES (?1, ?2, ?3, 'image', ?4, ?5, ?6, ?7, ?8)",
                     params![entry.hash, timestamp, entry.app_name, hash, mime, entry.size_bytes as i64, *width as i64, *height as i64],
                )?;

                CachedEntry::image(
                    hash.clone(),
                    mime.clone(),
                    *width,
                    *height,
                    timestamp,
                    entry.size_bytes,
                )
            }
        };

        self.cache.put(entry.hash.clone(), cached_entry);
        Ok(())
    }

    fn cleanup_old_entries_blocking(cache_dir: &Path, max_entries: usize, max_age_days: u32) -> Result<()> {
        let db_path = cache_dir.join("index.db");
        let db = SqliteConnection::open(&db_path)?;

        // Clean by max entries
        let mut stmt = db.prepare(
            "SELECT hash, file_path FROM entries
             WHERE id NOT IN (
                 SELECT id FROM entries ORDER BY timestamp DESC LIMIT ?1
             )",
        )?;

        let entries_to_delete: Vec<(String, Option<String>)> = stmt
            .query_map(params![max_entries as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        drop(stmt);

        // Clean by age
        let cutoff = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()
            - (max_age_days as u64 * 86400);

        let mut stmt = db.prepare("SELECT hash, file_path FROM entries WHERE timestamp < ?1")?;

        let old_entries: Vec<(String, Option<String>)> = stmt
            .query_map(params![cutoff as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        drop(stmt);

        // Delete all collected entries
        for (hash, file_path) in entries_to_delete.into_iter().chain(old_entries) {
            db.execute("DELETE FROM entries WHERE hash = ?1", params![hash])?;

            // Remove associated files
            if let Some(fp) = file_path {
                let path = cache_dir.join("blobs").join(&fp);
                let _ = fs::remove_file(&path);
            }

            // Also try to remove legacy file paths
            let legacy_path = cache_dir.join("blobs").join(&hash);
            let _ = fs::remove_file(&legacy_path);
            let txt_path = cache_dir.join("blobs").join(format!("{}.txt", hash));
            let _ = fs::remove_file(&txt_path);
            let zst_path = cache_dir.join("blobs").join(format!("{}.txt.zst", hash));
            let _ = fs::remove_file(&zst_path);
        }

        // WAL checkpoint + VACUUM occasionally (every 100 cleanups)
        static VACUUM_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let count = VACUUM_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if count % 10 == 0 {
            let _ = db.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
        }
        if count % 100 == 0 {
            let _ = db.execute("VACUUM", []);
        }

        Ok(())
    }

    fn delete_entry(&mut self, hash: &str) -> Result<()> {
        // Handle both full hashes and prefixes
        let entries_to_delete: Vec<(String, Option<String>)> = if hash.len() == 64 {
            // Full hash provided
            let file_path: Option<String> = self
                .db
                .query_row(
                    "SELECT file_path FROM entries WHERE hash = ?1",
                    params![hash],
                    |row| row.get(0),
                )
                .optional()?;
            if file_path.is_some() || self.db.query_row("SELECT 1 FROM entries WHERE hash = ?1", params![hash], |_| Ok(())).optional()?.is_some() {
                vec![(hash.to_string(), file_path)]
            } else {
                vec![]
            }
        } else {
            // Hash prefix provided - find all matching entries
            let mut stmt = self.db.prepare(
                "SELECT hash, file_path FROM entries WHERE hash LIKE ?1 || '%'"
            )?;
            stmt.query_map(params![hash], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };

        // Delete all matching entries
        for (full_hash, file_path) in entries_to_delete {
            self.db
                .execute("DELETE FROM entries WHERE hash = ?1", params![full_hash])?;

            self.cache.remove(&full_hash);

            // Remove associated files
            if let Some(file_path) = file_path {
                let path = self.config.cache_dir.join("blobs").join(&file_path);
                let _ = fs::remove_file(&path);
            }

            // Also try to remove legacy file paths (backward compatibility)
            let legacy_path = self.config.cache_dir.join("blobs").join(&full_hash);
            let _ = fs::remove_file(&legacy_path);
            let txt_path = self.config.cache_dir.join("blobs").join(format!("{}.txt", full_hash));
            let _ = fs::remove_file(&txt_path);
            let zst_path = self.config.cache_dir.join("blobs").join(format!("{}.txt.zst", full_hash));
            let _ = fs::remove_file(&zst_path);
        }

        Ok(())
    }

    pub fn read_text_preview(&self, file_path: &str, max_len: usize) -> Option<String> {
        let path = self.config.cache_dir.join("blobs").join(file_path);

        if file_path.ends_with(".zst") {
            self.read_compressed_preview(&path, max_len)
        } else {
            self.read_plain_preview(&path, max_len)
        }
    }

    pub fn read_compressed_preview(&self, path: &Path, max_len: usize) -> Option<String> {
        use zstd::stream::read::Decoder;

        // Cap to prevent overflow (max 10MB)
        let max_len = max_len.min(10 * 1024 * 1024);

        let file = fs::File::open(path).ok()?;
        let mut decoder = Decoder::new(file).ok()?;
        let mut buffer = vec![0u8; max_len];
        let n = std::io::Read::read(&mut decoder, &mut buffer).ok()?;

        String::from_utf8(buffer[..n].to_vec()).ok()
    }

    pub fn read_plain_preview(&self, path: &Path, max_len: usize) -> Option<String> {
        // Cap to prevent overflow (max 10MB)
        let max_len = max_len.min(10 * 1024 * 1024);

        let data = fs::read(path).ok()?;
        let len = data.len().min(max_len);
        String::from_utf8(data[..len].to_vec()).ok()
    }

    pub fn format_time_ago(&self, timestamp: i64) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let ago_secs = now.saturating_sub(timestamp as u64);

        match ago_secs {
            0..=59 => format!("{}s", ago_secs),
            60..=3599 => format!("{}m", ago_secs / 60),
            3600..=86399 => format!("{}h", ago_secs / 3600),
            _ => format!("{}d", ago_secs / 86400),
        }
    }

    pub fn format_size(&self, bytes: i64) -> String {
        match bytes {
            0..=1023 => format!("{}B", bytes),
            1024..=1048575 => format!("{}K", bytes / 1024),
            _ => format!("{}M", bytes / (1024 * 1024)),
        }
    }

    pub fn print_history(&self) -> Result<()> {
        // Print static entries
        for entry in &self.config.static_entries {
            println!("{}", entry.replace('\n', " "));
        }

        let _cached_hashes = self.cache.get_hashes();

        // Process cached entries first
        let mut printed_hashes = std::collections::HashSet::new();
        let mut printed_count = 0;

        // Sort cached entries by timestamp
        let cached_entries = self.cache.iter_sorted();

        // Print cached entries
        for (hash, cached) in cached_entries.iter().take(self.config.max_print_entries) {
            if printed_count >= self.config.max_print_entries {
                break;
            }

            self.print_cached_entry(hash, cached)?;
            printed_hashes.insert(hash.clone());
            printed_count += 1;
        }

        // Only query database for remaining entries if needed
        if printed_count < self.config.max_print_entries {
            let mut stmt = self.db.prepare(
                "SELECT hash, app_name, content_type, inline_text, file_path, mime_type, size_bytes, timestamp, width, height
                 FROM entries 
                 WHERE hash NOT IN (SELECT value FROM json_each(?1))
                 ORDER BY timestamp DESC LIMIT ?2"
            )?;

            let excluded_json = serde_json::to_string(&printed_hashes.iter().collect::<Vec<_>>())?;
            let remaining_limit = self.config.max_print_entries - printed_count;

            let rows = stmt.query_map(params![excluded_json, remaining_limit as i64], |row| {
                let hash: String = row.get(0)?;
                let app_name: String = row.get(1)?;
                let content_type: String = row.get(2)?;
                let inline_text: Option<String> = row.get(3)?;
                let file_path: Option<String> = row.get(4)?;
                let mime_type: Option<String> = row.get(5)?;
                let size_bytes: i64 = row.get(6)?;
                let timestamp: i64 = row.get(7)?;
                let width: Option<i64> = row.get(8)?;
                let height: Option<i64> = row.get(9)?;

                Ok((
                    hash,
                    app_name,
                    content_type,
                    inline_text,
                    file_path,
                    mime_type,
                    size_bytes,
                    timestamp,
                    width,
                    height,
                ))
            })?;

            for row in rows {
                let (
                    hash,
                    _app_name,
                    content_type,
                    inline_text,
                    file_path,
                    mime_type,
                    size_bytes,
                    timestamp,
                    width,
                    height,
                ) = row?;
                let time_str = self.format_time_ago(timestamp);
                let hash_prefix = &hash[..HASH_PREFIX_LEN.min(hash.len())];

                let size_str = self.format_size(size_bytes);

                match content_type.as_str() {
                    "text" => {
                        if let Some(text) = inline_text {
                            let available_chars = 80 - time_str.len() - 1 - 9 - 1;
                            let display = self.truncate_to_fit(&text, available_chars);
                            let line = format!("{} {}", time_str, display);
                            println!("{:<70} #{}", line, hash_prefix);
                        }
                    }
                    "text_file" => {
                        if let Some(fp) = file_path {
                            let available_chars =
                                80 - time_str.len() - 1 - 9 - 1 - size_str.len() - 3;
                            let preview = self
                                .read_text_preview(&fp, available_chars * 4)
                                .map(|p| self.truncate_to_fit(&p, available_chars))
                                .filter(|p| !p.is_empty());

                            if let Some(display) = preview {
                                println!(
                                    "{} {} [{}] #{}",
                                    time_str, display, size_str, hash_prefix
                                );
                            } else {
                                let line = format!("{} [Text: {}]", time_str, size_str);
                                println!("{:<70} #{}", line, hash_prefix);
                            }
                        }
                    }
                    "image" => {
                        let mime_short = mime_type
                            .as_ref()
                            .map(|m| m.split('/').last().unwrap_or("?"))
                            .unwrap_or("?");

                        if let (Some(w), Some(h)) = (width, height) {
                            let dims_str = format!("{}x{}px", w, h);
                            let available = 80
                                - time_str.len()
                                - 1
                                - 9
                                - 7
                                - mime_short.len()
                                - size_str.len()
                                - 2;
                            if dims_str.len() <= available {
                                println!(
                                    "{} [IMG:{} {} {}] #{}",
                                    time_str, dims_str, mime_short, size_str, hash_prefix
                                );
                            } else {
                                println!(
                                    "{} [IMG:{} {}] #{}",
                                    time_str, mime_short, size_str, hash_prefix
                                );
                            }
                        } else {
                            println!(
                                "{} [IMG:{} {}] #{}",
                                time_str, mime_short, size_str, hash_prefix
                            );
                        }
                    }
                    _ => {}
                }
            }
        }

        Ok(())
    }

    fn print_cached_entry(&self, hash: &str, cached: &CachedEntry) -> Result<()> {
        let time_str = self.format_time_ago(cached.timestamp);
        let hash_prefix = &hash[..HASH_PREFIX_LEN.min(hash.len())];
        let size_str = self.format_size(cached.size_bytes as i64);

        match cached.content_type.as_str() {
            "text" => {
                if let Some(text) = &cached.inline_text {
                    let available_chars = 80 - time_str.len() - 1 - 9 - 1;
                    let display = self.truncate_to_fit(text, available_chars);
                    let line = format!("{} {}", time_str, display);
                    println!("{:<70} #{}", line, hash_prefix);
                }
            }
            "text_file" => {
                if let Some(fp) = &cached.file_path {
                    let available_chars = 80 - time_str.len() - 1 - 9 - 1 - size_str.len() - 3;
                    let preview = self
                        .read_text_preview(fp, available_chars * 4)
                        .map(|p| self.truncate_to_fit(&p, available_chars))
                        .filter(|p| !p.is_empty());

                    if let Some(display) = preview {
                        let line = format!("{} {} [{}]", time_str, display, size_str);
                        println!("{:<70} #{}", line, hash_prefix);
                    } else {
                        let line = format!("{} [Text: {}]", time_str, size_str);
                        println!("{:<70} #{}", line, hash_prefix);
                    }
                }
            }
            "image" => {
                let mime_short = cached
                    .mime_type
                    .as_ref()
                    .map(|m| m.split('/').last().unwrap_or("?"))
                    .unwrap_or("?");

                if let (Some(w), Some(h)) = (cached.width, cached.height) {
                    let dims_str = format!("{}x{}px", w, h);
                    let available =
                        80 - time_str.len() - 1 - 9 - 7 - mime_short.len() - size_str.len() - 2;
                    if dims_str.len() <= available {
                        println!(
                            "{} [IMG:{} {} {}] #{}",
                            time_str, dims_str, mime_short, size_str, hash_prefix
                        );
                    } else {
                        println!(
                            "{} [IMG:{} {}] #{}",
                            time_str, mime_short, size_str, hash_prefix
                        );
                    }
                } else {
                    println!(
                        "{} [IMG:{} {}] #{}",
                        time_str, mime_short, size_str, hash_prefix
                    );
                }
            }
            _ => {}
        }

        Ok(())
    }

    fn truncate_to_fit(&self, text: &str, max_chars: usize) -> String {
        let text = text.replace('\n', " ").replace('\t', " ");

        if text.len() <= max_chars {
            text
        } else {
            let mut end = max_chars.saturating_sub(1).min(text.len());
            while !text.is_char_boundary(end) && end > 0 {
                end -= 1;
            }
            format!("{}…", &text[..end])
        }
    }

    async fn copy_selection(&mut self, selection: &str) -> Result<()> {
        // extract hash from #hash format anywhere in string
        if let Some(hash_pos) = selection.rfind('#') {
            let hash_start = hash_pos + 1;
            let hash_end = selection[hash_start..]
                .find(|c: char| c.is_whitespace())
                .map(|i| hash_start + i)
                .unwrap_or(selection.len());

            let hash = &selection[hash_start..hash_end];

            if !hash.is_empty() && hash.len() >= 8 {
                match self.copy_by_hash(hash).await {
                    Ok(_) => return Ok(()),
                    Err(_) => {
                        // fallback to literal copy
                    }
                }
            }
        }

        // copy as literal text
        let text = selection.to_string();
        self.with_clipboard_async(move |clipboard| {
            clipboard.set_text(text)?;
            Ok(())
        }).await
    }

    async fn copy_by_hash(&self, hash_prefix: &str) -> Result<()> {
        // Require minimum 8 characters to avoid collisions
        if hash_prefix.len() < 8 {
            anyhow::bail!("Hash prefix too short - minimum 8 characters required");
        }

        let mut matches = Vec::new();

        // cache lookup first for performance
        for (full_hash, cached) in self.cache.iter_sorted() {
            if full_hash.starts_with(hash_prefix) {
                matches.push((full_hash.clone(), cached.clone()));
            }
        }

        // If multiple matches found, require more specificity
        if matches.len() > 1 {
            anyhow::bail!("Ambiguous hash prefix '{}' - {} matches found. Use more characters.", hash_prefix, matches.len());
        }

        if let Some((full_hash, cached)) = matches.first() {
            if full_hash.starts_with(hash_prefix) {
                return match cached.content_type.as_str() {
                    "text" => {
                        if let Some(text) = &cached.inline_text {
                            let text = text.clone();
                            self.with_clipboard_async(move |clipboard| {
                                clipboard.set_text(text)?;
                                Ok(())
                            }).await
                        } else {
                            anyhow::bail!("Missing text content")
                        }
                    }
                    "text_file" => {
                        if let Some(fp) = &cached.file_path {
                            let path = self.config.cache_dir.join("blobs").join(fp);
                            let mut text = if cached.compressed {
                                use zstd::stream::read::Decoder;
                                let file = fs::File::open(path)?;
                                let mut decoder = Decoder::new(file)?;
                                let mut text = String::new();
                                decoder.read_to_string(&mut text)?;
                                text
                            } else {
                                fs::read_to_string(path)?
                            };

                            let text_clone = text.clone();
                            let result = self.with_clipboard_async(move |clipboard| {
                                clipboard.set_text(text_clone)?;
                                Ok(())
                            }).await;
                            text.zeroize();
                            result
                        } else {
                            anyhow::bail!("Missing file path")
                        }
                    }
                    "image" => {
                        if let Some(fp) = &cached.file_path {
                            let path = self.config.cache_dir.join("blobs").join(fp);
                            let data = fs::read(&path)?;

                            let img = image::load_from_memory(&data)?;
                            let rgba = img.to_rgba8();
                            let (width, height) = (rgba.width() as usize, rgba.height() as usize);

                            let img_data = ImageData {
                                width,
                                height,
                                bytes: rgba.into_raw().into(),
                            };

                            self.with_clipboard_async(move |clipboard| {
                                clipboard.set_image(img_data)?;
                                Ok(())
                            }).await
                        } else {
                            anyhow::bail!("Missing image path")
                        }
                    }
                    _ => anyhow::bail!("Unknown content type"),
                };
            }
        }

        // cache miss, query database - check for ambiguous matches
        let mut stmt = self.db.prepare(
            "SELECT hash, content_type, inline_text, file_path, compressed
             FROM entries WHERE hash LIKE ?1 || '%'
             ORDER BY timestamp DESC"
        )?;

        let db_matches: Vec<(String, String, Option<String>, Option<String>, Option<bool>)> = stmt
            .query_map(params![hash_prefix], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get::<_, Option<i64>>(4)?.map(|v| v != 0),
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        if db_matches.len() > 1 {
            anyhow::bail!("Ambiguous hash prefix '{}' - {} matches found in database. Use more characters.", hash_prefix, db_matches.len());
        }

        if let Some((_full_hash, content_type, inline_text, file_path, compressed)) = db_matches.first() {

            match content_type.as_str() {
                "text" => {
                    if let Some(text) = inline_text {
                        let text = text.clone();
                        self.with_clipboard_async(move |clipboard| {
                            clipboard.set_text(text)?;
                            Ok(())
                        }).await?;
                    }
                }
                "text_file" => {
                    if let Some(fp) = file_path {
                        let path = self.config.cache_dir.join("blobs").join(&fp);
                        let mut text = if compressed.unwrap_or(false) {
                            use zstd::stream::read::Decoder;
                            let file = fs::File::open(path)?;
                            let mut decoder = Decoder::new(file)?;
                            let mut text = String::new();
                            decoder.read_to_string(&mut text)?;
                            text
                        } else {
                            fs::read_to_string(path)?
                        };

                        let text_clone = text.clone();
                        self.with_clipboard_async(move |clipboard| {
                            clipboard.set_text(text_clone)?;
                            Ok(())
                        }).await?;
                        text.zeroize();
                    }
                }
                "image" => {
                    if let Some(fp) = file_path {
                        let path = self.config.cache_dir.join("blobs").join(&fp);
                        let data = fs::read(&path)?;

                        let img = image::load_from_memory(&data)?;
                        let rgba = img.to_rgba8();
                        let (width, height) = (rgba.width() as usize, rgba.height() as usize);

                        let img_data = ImageData {
                            width,
                            height,
                            bytes: rgba.into_raw().into(),
                        };

                        self.with_clipboard_async(move |clipboard| {
                            clipboard.set_image(img_data)?;
                            Ok(())
                        }).await?;
                    }
                }
                _ => anyhow::bail!("Unknown content type"),
            }
        } else {
            anyhow::bail!("Entry not found for hash: {}", hash_prefix);
        }

        Ok(())
    }

    pub async fn clear(&mut self) -> Result<()> {
        self.last_clipboard_hash = None;
        self.cache.clear();

        let _ = self.with_clipboard_async(|clipboard| {
            clipboard.set_text("".to_string())?;
            Ok(())
        }).await;

        // Bulk delete from database for efficiency
        self.db.execute("DELETE FROM entries", [])?;
        self.db.execute("VACUUM", [])?;

        // Remove all blob files
        let blobs_dir = self.config.cache_dir.join("blobs");
        if blobs_dir.exists() {
            for entry in fs::read_dir(&blobs_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_file() {
                    let _ = fs::remove_file(path);
                }
            }
        }

        Ok(())
    }

    pub async fn purge_all(&mut self) -> Result<()> {
        self.clear().await?;

        if self.config.cache_dir.exists() {
            fs::remove_dir_all(&self.config.cache_dir)?;
        }

        Ok(())
    }

    pub fn prune(&mut self, hashes: &[String]) -> Result<()> {
        for hash in hashes {
            self.delete_entry(hash)?;
        }
        Ok(())
    }

    pub fn get_history(&self, limit: usize) -> Result<Vec<Entry>> {
        let mut stmt = self.db.prepare(
            "SELECT id, hash, timestamp, app_name, content_type, inline_text, file_path, mime_type, size_bytes, compressed, width, height
             FROM entries ORDER BY timestamp DESC LIMIT ?1"
        )?;

        let entries = stmt
            .query_map([limit as i64], |row| {
                let id: i64 = row.get(0)?;
                let hash: String = row.get(1)?;
                let timestamp: i64 = row.get(2)?;
                let app_name: String = row.get(3)?;
                let content_type: String = row.get(4)?;
                let inline_text: Option<String> = row.get(5)?;
                let _file_path: Option<String> = row.get(6)?;
                let mime_type: Option<String> = row.get(7)?;
                let size_bytes: i64 = row.get(8)?;
                let compressed: Option<bool> = row.get::<_, Option<i64>>(9)?.map(|v| v != 0);
                let width: Option<i64> = row.get(10)?;
                let height: Option<i64> = row.get(11)?;

                let content = match content_type.as_str() {
                    "text" => ContentType::Text(inline_text.unwrap_or_default()),
                    "text_file" => ContentType::TextFile {
                        hash: hash.clone(),
                        compressed: compressed.unwrap_or(false),
                    },
                    "image" => ContentType::Image {
                        mime: mime_type.unwrap_or_else(|| "image/unknown".to_string()),
                        hash: hash.clone(),
                        width: width.unwrap_or(0) as u32,
                        height: height.unwrap_or(0) as u32,
                    },
                    _ => ContentType::Text("Unknown".to_string()),
                };

                Ok(Entry {
                    id: Some(id),
                    hash,
                    timestamp: timestamp as u64,
                    app_name,
                    content,
                    size_bytes: size_bytes as usize,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(entries)
    }

    pub fn get_entries(&self, limit: usize) -> Result<Vec<(String, String, String, i64, i64)>> {
        let mut stmt = self.db.prepare(
            "SELECT hash, content_type, inline_text, file_path, size_bytes, timestamp
             FROM entries ORDER BY timestamp DESC LIMIT ?1",
        )?;

        let rows = stmt
            .query_map([limit as i64], |row| {
                let hash: String = row.get(0)?;
                let content_type: String = row.get(1)?;
                let inline_text: Option<String> = row.get(2)?;
                let file_path: Option<String> = row.get(3)?;
                let size_bytes: i64 = row.get(4)?;
                let timestamp: i64 = row.get(5)?;

                let content = match content_type.as_str() {
                    "text" => inline_text.unwrap_or_else(|| "[Empty]".to_string()),
                    "text_file" => {
                        if let Some(fp) = file_path {
                            self.read_text_preview(&fp, self.config.max_display_length)
                                .unwrap_or_else(|| {
                                    format!("[Text: {}]", self.format_size(size_bytes))
                                })
                        } else {
                            format!("[Text: {}]", self.format_size(size_bytes))
                        }
                    }
                    "image" => format!("[Image: {}]", self.format_size(size_bytes)),
                    _ => "[Unknown]".to_string(),
                };

                Ok((hash, content, content_type, size_bytes, timestamp))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    pub fn search_entries(&self, query: Option<&str>, full_content: bool) -> Result<()> {
        let sql = "SELECT hash, content_type, inline_text, file_path, size_bytes, timestamp, compressed, mime_type, width, height
                   FROM entries
                   ORDER BY timestamp DESC";

        let mut stmt = self.db.prepare(sql)?;

        let row_mapper = |row: &rusqlite::Row| -> Result<(String, String, Option<String>, Option<String>, i64, i64, Option<bool>, Option<String>, Option<i64>, Option<i64>), rusqlite::Error> {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<i64>>(6)?.map(|v| v != 0),
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<i64>>(8)?,
                row.get::<_, Option<i64>>(9)?,
            ))
        };

        let rows = stmt.query_map([], row_mapper)?;

        for row in rows {
            let (hash, content_type, inline_text, file_path, size_bytes, timestamp, compressed, mime_type, width, height) = row?;

            let time_str = self.format_time_ago(timestamp);
            let size_str = self.format_size(size_bytes);
            let hash_prefix = &hash[..HASH_PREFIX_LEN.min(hash.len())];

            // Handle query filtering
            let mut matches_query = query.is_none();
            if let Some(q) = query {
                let q_lower = q.to_lowercase();

                // Check inline text
                if let Some(ref text) = inline_text {
                    if text.to_lowercase().contains(&q_lower) {
                        matches_query = true;
                    }
                }

                // Check file content for text_file entries
                if !matches_query && content_type == "text_file" {
                    if let Some(ref fp) = file_path {
                        let content = if compressed.unwrap_or(false) {
                            self.read_compressed_preview(&self.config.cache_dir.join("blobs").join(fp), usize::MAX)
                        } else {
                            self.read_plain_preview(&self.config.cache_dir.join("blobs").join(fp), usize::MAX)
                        };

                        if let Some(content) = content {
                            if content.to_lowercase().contains(&q_lower) {
                                matches_query = true;
                            }
                        }
                    }
                }
            }

            if !matches_query {
                continue;
            }

            match content_type.as_str() {
                "text" => {
                    if let Some(text) = inline_text {
                        if full_content {
                            println!("HASH:{}|TYPE:text|SIZE:{}|TIME:{}|CONTENT:{}",
                                hash_prefix, size_str, time_str, text.replace('\n', "\\n"));
                        } else {
                            let display = self.truncate_to_fit(&text, 60);
                            println!("HASH:{}|TYPE:text|SIZE:{}|TIME:{}|CONTENT:{}",
                                hash_prefix, size_str, time_str, display);
                        }
                    }
                }
                "text_file" => {
                    if let Some(fp) = file_path {
                        let content = if full_content {
                            // Read full content for search
                            if compressed.unwrap_or(false) {
                                self.read_compressed_preview(&self.config.cache_dir.join("blobs").join(&fp), usize::MAX)
                                    .unwrap_or_else(|| format!("[Text: {}]", size_str))
                            } else {
                                self.read_plain_preview(&self.config.cache_dir.join("blobs").join(&fp), usize::MAX)
                                    .unwrap_or_else(|| format!("[Text: {}]", size_str))
                            }
                        } else {
                            self.read_text_preview(&fp, 60)
                                .unwrap_or_else(|| format!("[Text: {}]", size_str))
                        };

                        let display_content = if full_content {
                            content.replace('\n', "\\n")
                        } else {
                            self.truncate_to_fit(&content, 60)
                        };

                        println!("HASH:{}|TYPE:text_file|SIZE:{}|TIME:{}|CONTENT:{}",
                            hash_prefix, size_str, time_str, display_content);
                    }
                }
                "image" => {
                    let mime_short = mime_type
                        .as_ref()
                        .map(|m| m.split('/').last().unwrap_or("?"))
                        .unwrap_or("?");

                    let content = if let (Some(w), Some(h)) = (width, height) {
                        format!("[IMG:{}x{}px {} {}]", w, h, mime_short, size_str)
                    } else {
                        format!("[IMG:{} {}]", mime_short, size_str)
                    };

                    println!("HASH:{}|TYPE:image|SIZE:{}|TIME:{}|CONTENT:{}",
                        hash_prefix, size_str, time_str, content);
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Fast FTS5 search for completion - returns matches ranked by relevance + recency
    pub fn complete(&self, query: &str, limit: usize) -> Result<()> {
        if query.trim().is_empty() {
            // No query, just return recent entries
            return self.print_recent(limit);
        }

        // Use FTS5 MATCH with trigram tokenizer for fuzzy matching
        // Join with entries to get timestamp for ranking
        let sql = "SELECT e.hash, e.inline_text, e.timestamp, e.size_bytes,
                          bm25(entries_fts) as rank
                   FROM entries_fts f
                   JOIN entries e ON e.hash = f.hash
                   WHERE entries_fts MATCH ?1
                   ORDER BY rank, e.timestamp DESC
                   LIMIT ?2";

        let mut stmt = self.db.prepare(sql)?;

        // Escape special FTS5 characters and prepare query
        let fts_query = self.prepare_fts_query(query);

        let rows = stmt.query_map(params![fts_query, limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,      // hash
                row.get::<_, String>(1)?,      // content
                row.get::<_, i64>(2)?,         // timestamp
                row.get::<_, i64>(3)?,         // size_bytes
            ))
        })?;

        for row in rows {
            let (hash, content, timestamp, size_bytes) = row?;
            let time_str = self.format_time_ago(timestamp);
            let size_str = self.format_size(size_bytes);
            let hash_prefix = &hash[..HASH_PREFIX_LEN.min(hash.len())];
            let display = self.truncate_to_fit(&content, 60);

            // Simple format for rofi/fzf piping
            println!("{} {} [{}] #{}", time_str, display, size_str, hash_prefix);
        }

        Ok(())
    }

    fn print_recent(&self, limit: usize) -> Result<()> {
        let sql = "SELECT hash, inline_text, timestamp, size_bytes
                   FROM entries
                   WHERE inline_text IS NOT NULL
                   ORDER BY timestamp DESC
                   LIMIT ?1";

        let mut stmt = self.db.prepare(sql)?;

        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;

        for row in rows {
            let (hash, content, timestamp, size_bytes) = row?;
            let time_str = self.format_time_ago(timestamp);
            let size_str = self.format_size(size_bytes);
            let hash_prefix = &hash[..HASH_PREFIX_LEN.min(hash.len())];
            let display = self.truncate_to_fit(&content, 60);

            println!("{} {} [{}] #{}", time_str, display, size_str, hash_prefix);
        }

        Ok(())
    }

    fn prepare_fts_query(&self, query: &str) -> String {
        // For trigram tokenizer, we can search substrings directly
        // Escape quotes and wrap in quotes for phrase matching
        let escaped = query.replace('"', "\"\"");
        format!("\"{}\"", escaped)
    }
}
