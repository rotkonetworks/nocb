use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nocb::{ClipboardManager, Config};
use std::path::PathBuf;
use tokio::signal;

#[derive(Parser)]
#[command(name = "nocb")]
#[command(version = "1.1.7")]
#[command(about = "nearly optimal clipboard manager")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run clipboard daemon
    Daemon,
    /// Serve the clipboard to Claude Code over MCP (JSON-RPC on stdio)
    Mcp,
    /// Print clipboard history for rofi
    Print,
    /// Search clipboard history with full content
    Search {
        /// Search query pattern (optional - if not provided, shows all entries)
        query: Option<String>,
        /// Output full content instead of truncated previews
        #[arg(long)]
        full: bool,
    },
    /// Fast FTS completion search for rofi/fzf (use: nocb complete | fzf | nocb copy)
    Complete {
        /// Search query for fuzzy matching
        query: Option<String>,
        /// Max results to return
        #[arg(short, long, default_value = "50")]
        limit: usize,
    },
    /// Copy selection to clipboard (reads from stdin if no args)
    Copy {
        /// Selection text or image reference to copy
        #[arg(trailing_var_arg = true)]
        selection: Vec<String>,
    },
    /// Clear clipboard history
    Clear,
    /// Remove entries by hash list
    Prune {
        /// File containing hash list or direct hashes
        #[arg(value_name = "HASHES")]
        input: Vec<String>,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // No up-front display check: the daemon discovers and waits for a display at
    // runtime (see ClipboardManager::try_reconnect), and the read-only commands
    // (print/search/complete) only touch the local DB, so they work headless.

    let config = Config::load().context("Failed to load configuration")?;

    match cli.command {
        Commands::Daemon => {
            let manager = ClipboardManager::new(config).await?;

            tokio::select! {
                result = manager.run_daemon() => {
                    if let Err(e) = result {
                        eprintln!("Daemon error: {}", e);
                        std::process::exit(1);
                    }
                }
                _ = signal::ctrl_c() => {
                    println!("\nShutting down...");
                }
            }
        }
        Commands::Mcp => {
            let manager = ClipboardManager::new(config).await?;
            manager.run_mcp()?;
        }
        Commands::Print => {
            let manager = ClipboardManager::new(config).await?;
            manager.print_history()?;
        }
        Commands::Search { query, full } => {
            let manager = ClipboardManager::new(config).await?;
            manager.search_entries(query.as_deref(), full)?;
        }
        Commands::Complete { query, limit } => {
            let manager = ClipboardManager::new(config).await?;
            manager.complete(query.as_deref().unwrap_or(""), limit)?;
        }
        Commands::Copy { selection } => {
            let selection = if selection.is_empty() {
                use std::io::{self, Read};
                let mut buffer = String::new();
                io::stdin().read_to_string(&mut buffer)?;
                buffer.trim().to_string()
            } else {
                selection.join(" ")
            };

            if selection.is_empty() {
                return Ok(());
            }

            ClipboardManager::send_copy_command(&selection).await?;
        }
        Commands::Clear => {
            nocb::ClipboardManager::send_command("CLEAR").await?;

            println!("Clear command sent to daemon");
        }
        Commands::Prune { input } => {
            let hashes = if input.len() == 1 && PathBuf::from(&input[0]).exists() {
                let content =
                    std::fs::read_to_string(&input[0]).context("Failed to read hash file")?;
                content.lines().map(|s| s.trim().to_string()).collect()
            } else {
                input
            };

            nocb::ClipboardManager::send_command(&format!("PRUNE:{}", hashes.join(","))).await?;
            println!("Prune command sent to daemon for {} entries", hashes.len());
        }
    }

    Ok(())
}
