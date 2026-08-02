use std::{path::PathBuf, process::ExitCode};

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "bloxtree-fuse",
    version,
    about = "Mount a bloxtree store as a FUSE filesystem"
)]
struct Cli {
    #[arg(
        long,
        help = "Path to the bloxtree store root. Falls back to BLOXTREE_STORE env var, then $XDG_DATA_HOME/bloxtree"
    )]
    store: Option<PathBuf>,

    /// Directory to mount the filesystem on (must be empty)
    mountpoint: PathBuf,
}

fn resolve_store(cli_store: &Option<PathBuf>) -> PathBuf {
    if let Some(s) = cli_store {
        return s.clone();
    }
    if let Ok(s) = std::env::var("BLOXTREE_STORE") {
        return PathBuf::from(s);
    }
    dirs::data_dir()
        .expect("could not determine XDG data directory")
        .join("bloxtree")
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let store = resolve_store(&cli.store);

    eprintln!("mounting bloxtree at {}", cli.mountpoint.display());

    match bloxtree_fuse::mount(&store, &cli.mountpoint) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mount error: {e}");
            ExitCode::from(1)
        }
    }
}
