use std::{path::PathBuf, process::ExitCode};

use bloxtree_core::Bloxtree;
use clap::{Parser, Subcommand};

mod commands;
use commands::run_command;

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "bloxtree",
    version,
    about = "Content-addressable store with versioning"
)]
struct Cli {
    #[arg(
        long,
        help = "Path to the bloxtree store root. Falls back to BLOXTREE_STORE env var, then $XDG_DATA_HOME/bloxtree"
    )]
    store: Option<PathBuf>,

    #[arg(long, default_value_t = false, help = "Suppress informational output")]
    quiet: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Store a blob under one or more paths (reads from --file or stdin)
    Add {
        #[arg(help = "Paths to store the blob under")]
        paths: Vec<String>,

        #[arg(long, help = "Read content from this file instead of stdin")]
        file: Option<PathBuf>,
    },
    /// Fetch the latest version of a path (or a specific version by hash)
    Get {
        path: String,

        #[arg(long, help = "Write output to this file instead of stdout")]
        out: Option<PathBuf>,

        #[arg(long, help = "Retrieve a specific version by BLAKE3 hash (hex)")]
        hash: Option<String>,
    },
    /// Delete a path or a specific version of it
    Remove {
        path: String,

        #[arg(long, help = "Remove only this version (hex BLAKE3 hash)")]
        hash: Option<String>,
    },
    /// Keep only the newest N versions of a path
    Trim {
        path: String,

        /// Number of versions to keep; 0 removes all
        max_versions: u8,
    },
    /// List immediate children under a prefix (default: root)
    List { prefix: Option<String> },
    /// List all paths with their latest version
    Paths,
    /// Show version history of a path (newest first)
    Versions { path: String },
    /// Print store metadata
    Info,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn resolve_store(cli_store: &Option<PathBuf>) -> PathBuf {
    if let Some(s) = cli_store {
        return s.clone();
    }
    if let Ok(s) = std::env::var("BLOXTREE_STORE") {
        return PathBuf::from(s);
    }
    dirs::data_dir()
        .expect("could not determine XDG data directory as fallback")
        .join("bloxtree")
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let store = resolve_store(&cli.store);

    let mut bt = match Bloxtree::open(&store).await {
        Ok(bt) => bt,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };

    match run_command(&mut bt, &cli, &store).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
