---
name: nocb-clipboard
description: Read the user's clipboard and clipboard history (text and screenshots) through the nocb MCP server. Use when the user refers to something they copied, says "check my clipboard", "look at the screenshot I just took", "what did I copy", "the error I copied", or pastes nothing but expects you to see it. Also use when they mention nocb history or a Flameshot/screenshot capture.
---

# nocb clipboard

nocb records every clipboard change (text and images) into `~/.cache/nocb`. The
`nocb mcp` server exposes that history read-only as `mcp__nocb__*` tools, so you
can look at what the user copied without them pasting or saving files.

## Tools

| Tool | Use it for |
|---|---|
| `mcp__nocb__get_clipboard` | The most recent entry. Images come back as a viewable image block. |
| `mcp__nocb__list_clipboard` (`limit`) | One-line previews: `hash  [type]  age  snippet`. Start here when the user says "earlier" or "a few copies ago". |
| `mcp__nocb__get_clipboard_entry` (`hash`, ≥ 8 chars) | Full text, or the image, of one entry from the list. |
| `mcp__nocb__search_clipboard` (`query`, `limit`) | Case-insensitive substring search over text entries. |

If the tools are deferred, load all four in one `ToolSearch` call:
`select:mcp__nocb__get_clipboard,mcp__nocb__list_clipboard,mcp__nocb__get_clipboard_entry,mcp__nocb__search_clipboard`.

## Workflow

- **"Look at my screenshot"**: call `list_clipboard` with `limit` 5–10 and take the newest
  `[image]` entry. Don't just call `get_clipboard`, because the user often copies text after
  the screenshot. Fetch the image with `get_clipboard_entry`, then describe it so the user
  can confirm it's the right one.
- **"What I copied"**: `get_clipboard`. If that isn't plausibly it, list recent entries.
- **"That error/URL/command from before"**: `search_clipboard` with a distinctive substring.
- Selecting text in a terminal creates many small partial entries (`the MCP`, `restarted`,
  …). Skip them and look for the complete one.

## Notes

- The history is personal data. Read only what the task needs, and don't quote unrelated
  entries back to the user.
- The server reads the SQLite DB directly, so it works even when the daemon is down. But
  then nothing *new* is captured. If the newest entry is older than something the user
  says they just copied, the daemon is broken. Hand off to the `nocb-doctor` agent rather
  than guessing.
- Registration (user scope): `claude mcp add nocb --scope user -- nocb mcp`.
