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

# systemd service (ships with the repo; packages install it for you)
cp nocb.service ~/.config/systemd/user/
sed -i "s|/usr/bin/nocb|$HOME/.local/bin/nocb|" ~/.config/systemd/user/nocb.service
systemctl --user enable --now nocb
```

do not set `DISPLAY`/`XAUTHORITY` in the unit — the daemon finds the live X
display and auth cookie itself and reconnects across X restarts.

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

## claude code (mcp)

`nocb mcp` is a read-only MCP server over the history, so Claude Code can see
what you copied — including screenshots (e.g. flameshot ctrl+c) — without you
pasting or saving files:

```bash
claude mcp add nocb --scope user -- nocb mcp
```

tools: `get_clipboard`, `list_clipboard`, `get_clipboard_entry`, `search_clipboard`.
images come back as viewable image blocks. it reads the database directly, so it
works even when the daemon is down (but then nothing new is captured).

the repo ships claude code helpers in `.claude/`:

- `skills/nocb-clipboard` — when and how to read clipboard/screenshots via mcp
- `agents/nocb-doctor` — troubleshooting runbook for "nocb isn't working"

they load automatically inside this repo; to use them everywhere, link them into
`~/.claude/skills/` and `~/.claude/agents/`.

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
- **socket**: `$XDG_RUNTIME_DIR/nocb.sock` - ipc with uid verification (self-heals if removed)

## reliability

the daemon is built to never sit "running but deaf":

- **watchdog** — the unit is `Type=notify` with `WatchdogSec=90`; the daemon only
  pings while clipboard reads succeed (or it's actively waiting for X) and its
  socket exists, so systemd restarts it if it silently stops working
- **dead x connection** — detected and the process exits for a clean restart
  (arboard can't reconnect in-process once its connection dies)
- **socket** — lives in `$XDG_RUNTIME_DIR` (not `/tmp`, which tmpfiles ages out)
  and is rebound automatically if removed
- **no display** — waits with backoff instead of exiting, so it survives logout/X restarts

quick health check:

```bash
echo "probe-$(date +%s)" | xclip -selection clipboard -i; sleep 1.5; nocb print | head -1
```

if that probe doesn't show up, see `.claude/agents/nocb-doctor.md`. one known
non-nocb cause: claude code's copy-on-select writes clipboard *and* primary via
`xclip`, racing your terminal's ctrl+shift+c — set `CLAUDE_CODE_DISABLE_MOUSE=1`
or turn off copy-on-select in `/config`.

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
