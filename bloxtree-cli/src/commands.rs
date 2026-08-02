use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use crate::{Cli, Command::*};
use bloxtree_core::{Bloxtree, CommonPrefixEntry, Hash, error::Error as CoreError};

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

pub async fn run_command(bt: &mut Bloxtree, cli: &Cli, store: &Path) -> Result<(), CliError> {
    match &cli.command {
        Add { paths, file } => cmd_add(bt, paths, file.as_ref(), cli.quiet).await,
        Get { path, out, hash } => {
            cmd_get(bt, path, out.as_ref(), hash.as_deref(), cli.quiet).await
        }
        Remove { path, hash } => cmd_remove(bt, path, hash.as_deref()).await,
        Trim { path, max_versions } => cmd_trim(bt, path, *max_versions).await,
        List { prefix } => cmd_list(bt, prefix.as_deref().unwrap_or("")).await,
        Paths => cmd_paths(bt).await,
        Versions { path } => cmd_versions(bt, path).await,
        Info => cmd_info(bt, store).await,
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

pub(crate) enum CliError {
    Core(CoreError),
    Io(io::Error),
    Usage(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Core(e) => write!(f, "{e}"),
            CliError::Io(e) => write!(f, "{e}"),
            CliError::Usage(msg) => write!(f, "{msg}"),
        }
    }
}

impl From<CoreError> for CliError {
    fn from(e: CoreError) -> Self {
        CliError::Core(e)
    }
}

impl From<io::Error> for CliError {
    fn from(e: io::Error) -> Self {
        CliError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

fn stream_out(reader: &mut impl io::Read, out: Option<&PathBuf>) -> Result<u64, CliError> {
    match out {
        Some(path) => {
            let mut fh = fs::File::create(path)?;
            Ok(io::copy(reader, &mut fh)?)
        }
        None => {
            let mut stdout = io::stdout().lock();
            Ok(io::copy(reader, &mut stdout)?)
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn cmd_add(
    bt: &mut Bloxtree,
    paths: &[String],
    file: Option<&PathBuf>,
    quiet: bool,
) -> Result<(), CliError> {
    let path_strs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
    let r = match file {
        Some(f) => {
            let mut fh = fs::File::open(f)
                .map_err(|e| CliError::Usage(format!("cannot open {}: {e}", f.display())))?;
            bt.add_reader_to(&path_strs, &mut fh).await?
        }
        None => {
            bt.add_reader_to(&path_strs, &mut io::stdin().lock())
                .await?
        }
    };
    if !quiet {
        println!("{}  {}", r.hash, r.uuid);
    }
    Ok(())
}

async fn cmd_get(
    bt: &Bloxtree,
    path: &str,
    out: Option<&PathBuf>,
    hash: Option<&str>,
    quiet: bool,
) -> Result<(), CliError> {
    let mut reader: Box<dyn io::Read> = match hash {
        Some(hex) => {
            let h = parse_hash(hex)?;
            Box::new(
                bt.get_reader_by_hash(h)
                    .await?
                    .ok_or_else(|| CliError::Usage(format!("hash not found: {hex}")))?,
            )
        }
        None => Box::new(
            bt.get_reader(path)
                .await?
                .ok_or_else(|| CliError::Usage(format!("path not found: {path}")))?,
        ),
    };
    let n = stream_out(&mut reader, out)?;
    if !quiet {
        eprintln!("{n} bytes");
    }
    Ok(())
}

async fn cmd_remove(bt: &mut Bloxtree, path: &str, hash: Option<&str>) -> Result<(), CliError> {
    let h = hash.map(parse_hash).transpose()?;
    bt.remove_path(path, h).await?;
    Ok(())
}

async fn cmd_trim(bt: &mut Bloxtree, path: &str, max_versions: u8) -> Result<(), CliError> {
    bt.trim_path(path, max_versions).await?;
    Ok(())
}

async fn cmd_list(bt: &Bloxtree, prefix: &str) -> Result<(), CliError> {
    for entry in &bt.list_folder(prefix).await? {
        match entry {
            CommonPrefixEntry::CommonPrefix { path } => println!("d  {path}/"),
            CommonPrefixEntry::Path(pe) => {
                println!("f  {}  {}", pe.path, pe.hash)
            }
        }
    }
    Ok(())
}

async fn cmd_paths(bt: &Bloxtree) -> Result<(), CliError> {
    for pe in &bt.list_paths().await? {
        println!("{}  {}  {}", pe.path, pe.hash, pe.uuid);
    }
    Ok(())
}

async fn cmd_versions(bt: &Bloxtree, path: &str) -> Result<(), CliError> {
    let versions = bt.versions(path).await?;
    for (i, v) in versions.iter().enumerate() {
        println!("{}  {}  {}", i + 1, v.hash, v.uuid);
    }
    Ok(())
}

async fn cmd_info(bt: &Bloxtree, store: &Path) -> Result<(), CliError> {
    let paths = bt.list_paths().await?;
    println!("store:    {}", store.display());
    println!("paths:    {}", paths.len());
    let mut total_versions: usize = 0;
    for p in &paths {
        total_versions += bt.versions(&p.path).await.map(|v| v.len()).unwrap_or(0);
    }
    println!("versions: {}", total_versions);
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_hash(hex: &str) -> Result<Hash, CliError> {
    Hash::from_hex(hex).map_err(|_| CliError::Usage(format!("invalid hex hash: {hex}")))
}
