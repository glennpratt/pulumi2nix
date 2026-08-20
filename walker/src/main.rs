use clap::{Parser, Subcommand};
use pulumi2nix_index::{cmd_verify, cmd_walk, default_base, VerifyOpts, WalkOpts};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "pulumi2nix-index",
    about = "Breadth-first backfill walker for a pulumi-nix-index repo"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fill in missing hashes, breadth-first, within a budget
    Walk {
        /// Index repo root (shards live under index/)
        #[arg(long, default_value = ".")]
        index_dir: PathBuf,
        /// Provider name (repeatable); name@version pins one exact version
        /// and jumps the breadth-first queue
        #[arg(long = "provider", value_name = "NAME[@VERSION]")]
        providers: Vec<String>,
        /// File with one provider name per line (# comments ok)
        #[arg(long)]
        providers_file: Option<PathBuf>,
        /// Platforms to index (default: linux-amd64 linux-arm64 darwin-amd64 darwin-arm64)
        #[arg(long = "platform", value_name = "TARGET")]
        platforms: Vec<String>,
        /// Stop after hashing this many tarballs
        #[arg(long, default_value_t = 200)]
        max_artifacts: usize,
        /// Stop after this much wall clock (seconds)
        #[arg(long, default_value_t = 2400.0)]
        max_seconds: f64,
        /// Concurrent tarball streams
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
    },
    /// Re-hash a random sample of existing entries (drift detection)
    Verify {
        #[arg(long, default_value = ".")]
        index_dir: PathBuf,
        /// Number of entries to re-verify
        #[arg(long, default_value_t = 50)]
        sample: usize,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Walk {
            index_dir,
            providers,
            providers_file,
            platforms,
            max_artifacts,
            max_seconds,
            concurrency,
        } => {
            cmd_walk(WalkOpts {
                index_dir,
                providers,
                providers_file,
                platforms,
                max_artifacts,
                max_seconds,
                concurrency,
                base: default_base(),
            })
            .await
        }
        Command::Verify { index_dir, sample } => {
            cmd_verify(VerifyOpts {
                index_dir,
                sample,
                base: default_base(),
            })
            .await
        }
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}
