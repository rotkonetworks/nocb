# nocb

nearly optimal clipboard manager - fast, compressed, hash-based storage
<img width="804" height="561" alt="image" src="https://github.com/user-attachments/assets/4562f64d-cf15-4153-8248-3d42720048a7" />

## features

- **hash-based storage** - blake3 content addressing, instant deduplication
- **compression** - automatic zstd compression for text >4KB
- **images** - full support with dimensions (png/jpeg/gif/bmp/webp)
- **performance** - sqlite with WAL, LRU cache, 100MB entry support
- **security** - unix socket with uid verification, memory zeroization
- **rofi integration** - 80-char entries with hash-based retrieval

## install

```bash
cargo install --path core
# or
cargo build --release
cp target/release/nocb ~/.local/bin/
```

## usage

### daemon setup

```bash
# manual start
nocb daemon

# systemd service
cat > ~/.config/systemd/user/nocb.service << EOF
[Unit]
Description=NOCB Clipboard Manager
After=graphical-session.target

[Service]
Type=simple
ExecStart=$HOME/.local/bin/nocb daemon
Restart=on-failure
Environment="DISPLAY=:0"

[Install]
WantedBy=default.target
EOF

systemctl --user enable --now nocb
```

### usage

```bash
# minimalistic use with fzf
nocb print | fzf | nocb copy
```

### keybindings

```bash
# sxhkd with rofi example
echo 'super + b
    ~/.local/bin/nocb print | rofi -dmenu -i -p "clipboard" | ~/.local/bin/nocb copy' >> ~/.config/sxhkd/sxhkdrc
```

### commands

```bash
nocb print                      # list history (newest first)
nocb copy <selection>           # copy by selection or hash
nocb clear                      # wipe all history
nocb prune <hash1> <hash2>      # remove specific entries
```

## display format

```
3m use anyhow::{Context, Result}; use arboard… #b9ef2033
27s Preview of large text file… [38K] #a7c4f892
1d [IMG:1920x1080px png 256K] #d8e9f023
```

- **time** - relative timestamp (s/m/h/d)
- **content** - truncated to fit 80 chars
- **size** - file size for large entries
- **hash** - 8-char prefix for retrieval

## configuration

`~/.config/nocb/config.toml`:

```toml
cache_dir = "~/.cache/nocb"
max_entries = 10000
max_display_length = 200
max_print_entries = 1000
blacklist = ["KeePassXC", "1Password"]
trim_whitespace = true
compress_threshold = 4096
static_entries = []  # pinned entries
```

## storage

- **database**: `~/.cache/nocb/index.db` - metadata, hashes, timestamps
- **blobs**: `~/.cache/nocb/blobs/` - compressed text, images
- **socket**: `/tmp/nocb.sock` - ipc with uid verification

## implementation

- rust with tokio async runtime
- arboard for cross-platform clipboard
- sqlite with prepared statements
- zstd compression level 3
- lru cache for recent entries
- non-blocking clipboard polling

## requirements

- rust 1.70+
- x11 or wayland (linux)
- systemd (optional)

## license

MIT
