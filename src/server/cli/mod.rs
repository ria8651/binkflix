//! `clap` subcommand dispatch for the server binary. With no subcommand
//! `binkflix` runs the server (today's behaviour); other subcommands are
//! one-shot tools that piggy-back on the same SQLite database.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

pub mod cleanup;
pub mod import_jellyfin;

#[derive(Parser, Debug)]
#[command(name = "binkflix", version, about = "Personal media server", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the HTTP server (default when no subcommand is given).
    Serve,

    /// List or purge soft-deleted rows. By default this is a dry run that
    /// only prints what would be removed; pass `--apply` to actually delete.
    /// Hard deletion cascades through FK-linked rows (watch_progress,
    /// subtitles, thumbnails, etc.) so it is irreversible.
    Cleanup {
        #[arg(long)]
        apply: bool,
    },

    /// Import watch history from a Jellyfin SQLite database into binkflix's
    /// `watch_progress` table. Interactive — prompts for source user and
    /// target user_sub.
    ImportJellyfin {
        /// Path to the Jellyfin `library.db` (or equivalent) to read from.
        path: PathBuf,
    },
}

pub fn run() {
    let cli = Cli::parse();
    match cli.command {
        None | Some(Command::Serve) => super::run(),
        Some(Command::Cleanup { apply }) => run_one_shot(cleanup::run(apply)),
        Some(Command::ImportJellyfin { path }) => run_one_shot(import_jellyfin::run(path)),
    }
}

/// Build a small multi-thread runtime for one-shot commands. The server has
/// its own runtime in `super::run`; tools share none of that machinery so we
/// stand up the simplest possible runtime here.
fn run_one_shot(fut: impl std::future::Future<Output = anyhow::Result<()>>) {
    let _ = dotenvy::dotenv();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    if let Err(e) = rt.block_on(fut) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
