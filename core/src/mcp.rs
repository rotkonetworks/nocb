//! Minimal Model Context Protocol (MCP) server for nocb.
//!
//! Speaks JSON-RPC 2.0 over stdio — the transport Claude Code uses for local
//! MCP servers. It is deliberately hand-rolled (no SDK dependency) and read-only:
//! it exposes the clipboard history to an AI client but never mutates it.
//!
//! Register with Claude Code:
//!     claude mcp add nocb -- /home/you/.local/bin/nocb mcp

use crate::ClipboardManager;
use anyhow::Result;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use std::io::{BufRead, Write};

/// MCP revision we implement. Clients negotiate against this.
const PROTOCOL_VERSION: &str = "2024-11-05";
/// How many recent entries a text search scans (keeps latency bounded).
const SEARCH_SCAN_LIMIT: usize = 500;
/// Long edge above which an inlined image is thumbnailed before encoding.
const MAX_IMAGE_EDGE: u32 = 1568;
/// Hard ceiling on the base64 payload; beyond this we describe rather than inline.
const MAX_B64_BYTES: usize = 3 * 1024 * 1024;

/// Run the stdio server loop until stdin closes (client disconnects).
pub fn serve(manager: &ClipboardManager) -> Result<()> {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break; // EOF: client went away
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("nocb mcp: ignoring malformed JSON-RPC message: {e}");
                continue;
            }
        };

        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");

        let response = match method {
            "initialize" => Some(reply(id, json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "nocb", "version": env!("CARGO_PKG_VERSION") },
            }))),
            "tools/list" => Some(reply(id, json!({ "tools": tool_defs() }))),
            "tools/call" => Some(handle_call(manager, id, &req)),
            "ping" => Some(reply(id, json!({}))),
            // Notifications (no id) — e.g. notifications/initialized — need no reply.
            _ if id.is_none() => None,
            _ => Some(error(id, -32601, &format!("method not found: {method}"))),
        };

        if let Some(resp) = response {
            writeln!(out, "{}", serde_json::to_string(&resp)?)?;
            out.flush()?;
        }
    }

    Ok(())
}

fn tool_defs() -> Value {
    json!([
        {
            "name": "get_clipboard",
            "description": "Return the most recent clipboard entry. Images are returned as an image content block the model can see directly (PNG, downscaled if very large), alongside a text block giving dimensions and hash.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "search_clipboard",
            "description": "Search the clipboard history for entries whose text contains a query string. Returns matching entries with their full content.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Case-insensitive substring to match." },
                    "limit": { "type": "integer", "description": "Max results to return (default 10).", "minimum": 1 }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        },
        {
            "name": "list_clipboard",
            "description": "List the most recent clipboard history entries as one-line previews (hash, type, age, snippet). Use get_clipboard_entry with a hash to fetch full content.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "description": "Number of entries to list (default 20).", "minimum": 1 }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "get_clipboard_entry",
            "description": "Fetch one clipboard entry by hash prefix. Images are returned as a viewable image content block; text entries as text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "hash": { "type": "string", "description": "Hash prefix (>= 8 chars)." }
                },
                "required": ["hash"],
                "additionalProperties": false
            }
        }
    ])
}

fn handle_call(manager: &ClipboardManager, id: Option<Value>, req: &Value) -> Value {
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    let result: Result<Vec<Value>> = match name {
        "get_clipboard" => tool_get_clipboard(manager),
        "search_clipboard" => tool_search(manager, &args).map(|t| vec![text_block(&t)]),
        "list_clipboard" => tool_list(manager, &args).map(|t| vec![text_block(&t)]),
        "get_clipboard_entry" => tool_get_entry(manager, &args),
        other => Err(anyhow::anyhow!("unknown tool: {other}")),
    };

    match result {
        Ok(content) => reply(id, json!({ "content": content })),
        // Tool-level failures are reported inside the result with isError, per MCP.
        Err(e) => reply(id, json!({
            "content": [text_block(&format!("Error: {e}"))],
            "isError": true,
        })),
    }
}

fn tool_get_clipboard(m: &ClipboardManager) -> Result<Vec<Value>> {
    let entries = m.get_history(1)?;
    let Some(entry) = entries.into_iter().next() else {
        return Ok(vec![text_block("(clipboard history is empty)")]);
    };
    entry_blocks(m, &entry.hash, entry.size_bytes)
}

/// Build MCP content blocks for one entry: an image block when the entry is an
/// image, otherwise its text. Image blocks are always paired with a short text
/// block so the model has referable metadata (dimensions, hash) alongside the
/// picture.
fn entry_blocks(m: &ClipboardManager, hash: &str, size_bytes: usize) -> Result<Vec<Value>> {
    if let Some((bytes, mime, w, h)) = m.get_image_blob(hash)? {
        let (bytes, w, h) = downscale_if_needed(bytes, &mime, w, h);
        let encoded = STANDARD.encode(&bytes);
        if encoded.len() > MAX_B64_BYTES {
            return Ok(vec![text_block(&format!(
                "(image {w}x{h} {mime}, {} — too large to inline; hash {})",
                human_size(bytes.len()),
                short_hash(hash)
            ))]);
        }
        return Ok(vec![
            text_block(&format!(
                "image {w}x{h} {mime}, {}, hash {}",
                human_size(bytes.len()),
                short_hash(hash)
            )),
            json!({ "type": "image", "data": encoded, "mimeType": mime }),
        ]);
    }

    match m.get_full_text(hash)? {
        Some(text) => Ok(vec![text_block(&text)]),
        None => Ok(vec![text_block(&format!(
            "(entry is non-text and non-image: {size_bytes} bytes, hash {})",
            short_hash(hash)
        ))]),
    }
}

fn tool_search(m: &ClipboardManager, args: &Value) -> Result<String> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    if query.is_empty() {
        anyhow::bail!("`query` is required");
    }
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;

    let mut hits = Vec::new();
    for entry in m.get_history(SEARCH_SCAN_LIMIT)? {
        let Some(text) = m.get_full_text(&entry.hash)? else {
            continue;
        };
        if text.to_lowercase().contains(&query) {
            hits.push(format!(
                "### {} — {} ago, from {}\n{}",
                short_hash(&entry.hash),
                m.format_time_ago(entry.timestamp as i64),
                entry.app_name,
                text
            ));
            if hits.len() >= limit {
                break;
            }
        }
    }

    if hits.is_empty() {
        Ok(format!("No clipboard entries matching '{query}'."))
    } else {
        Ok(hits.join("\n\n"))
    }
}

fn tool_list(m: &ClipboardManager, args: &Value) -> Result<String> {
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
    let rows = m.get_entries(limit)?;
    if rows.is_empty() {
        return Ok("(clipboard history is empty)".to_string());
    }

    let lines: Vec<String> = rows
        .into_iter()
        .map(|(hash, preview, content_type, _size, timestamp)| {
            let snippet = truncate_chars(&preview.replace('\n', " "), 80);
            format!(
                "{}  [{}]  {} ago  {}",
                short_hash(&hash),
                content_type,
                m.format_time_ago(timestamp),
                snippet
            )
        })
        .collect();

    Ok(lines.join("\n"))
}

fn tool_get_entry(m: &ClipboardManager, args: &Value) -> Result<Vec<Value>> {
    let hash = args.get("hash").and_then(Value::as_str).unwrap_or("");
    if hash.len() < 8 {
        anyhow::bail!("`hash` must be a prefix of at least 8 characters");
    }
    entry_blocks(m, hash, 0)
}

/// Keep inlined images within a sane token budget. Long edge above
/// MAX_IMAGE_EDGE is thumbnailed and re-encoded as PNG.
fn downscale_if_needed(bytes: Vec<u8>, mime: &str, w: u32, h: u32) -> (Vec<u8>, u32, u32) {
    if w.max(h) <= MAX_IMAGE_EDGE {
        return (bytes, w, h);
    }
    let Ok(img) = image::load_from_memory(&bytes) else {
        return (bytes, w, h);
    };
    let small = img.thumbnail(MAX_IMAGE_EDGE, MAX_IMAGE_EDGE);
    let (nw, nh) = (small.width(), small.height());
    // Drop the alpha channel when the source has none: re-encoding a thumbnail
    // as RGBA can otherwise produce a *larger* payload than the full-size RGB
    // original, which defeats the point of downscaling.
    let small = if small.color().has_alpha() {
        small
    } else {
        image::DynamicImage::ImageRgb8(small.to_rgb8())
    };
    let mut out = Vec::new();
    if small
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .is_err()
    {
        return (bytes, w, h);
    }
    let _ = mime;
    (out, nw, nh)
}

fn human_size(n: usize) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1}MB", n as f64 / (1024.0 * 1024.0))
    } else {
        format!("{}KB", n / 1024)
    }
}

// --- helpers ---------------------------------------------------------------

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn reply(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result })
}

fn error(id: Option<Value>, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "error": { "code": code, "message": message } })
}

fn short_hash(hash: &str) -> &str {
    &hash[..hash.len().min(8)]
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}
