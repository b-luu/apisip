use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

/// Point apisip at an OpenAPI spec, answer a few prompts, and get every data
/// endpoint as JSONL or Parquet files, locally or in any S3-compatible bucket.
///
/// apisip works on one folder per API: every command reads `apisip.toml` in the
/// current directory (or the one given with `-C`).
#[derive(Parser, Debug)]
#[command(name = "apisip", version, about, long_about)]
struct Cli {
    /// Run as if apisip was started in this directory.
    #[arg(short = 'C', global = true, value_name = "DIR")]
    dir: Option<PathBuf>,

    /// More logging (-v info, -vv debug, -vvv trace). RUST_LOG overrides it.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Read a spec, ask a few questions, and write apisip.toml and .env here.
    Init(InitArgs),
    /// List the streams configured in this folder.
    List,
    /// Dry run: validate config and credentials with one request per stream. Writes nothing.
    Check,
    /// Download every enabled stream.
    Run(RunArgs),
    /// Diff a new version of the spec against the config, and merge it.
    Update(UpdateArgs),
}

#[derive(Args, Debug)]
struct InitArgs {
    /// URL or file path of the OpenAPI (3.x) or Swagger (2.0) spec, JSON or YAML.
    spec: String,
    /// Overwrite an existing apisip.toml.
    #[arg(long)]
    force: bool,
    /// Accept every default and never prompt. Use with the flags below.
    #[arg(short, long)]
    yes: bool,
    /// Where to write data: a directory, file:// URL, or s3://bucket/prefix.
    #[arg(long)]
    destination: Option<String>,
    /// Output format.
    #[arg(long, value_parser = ["jsonl", "parquet"])]
    format: Option<String>,
    /// Name of the security scheme to use, as declared in the spec.
    #[arg(long)]
    auth: Option<String>,
    /// Rate-limit plan to use when the spec documents several.
    #[arg(long)]
    plan: Option<String>,
    /// Only enable these streams (comma-separated). Default: all detected streams.
    #[arg(long, value_delimiter = ',')]
    streams: Option<Vec<String>>,
    /// Override the base URL from the spec's servers list.
    #[arg(long)]
    base_url: Option<String>,
    /// S3 endpoint URL for S3-compatible stores (MinIO, R2, ...).
    #[arg(long)]
    s3_endpoint: Option<String>,
    /// S3 region.
    #[arg(long)]
    s3_region: Option<String>,
}

#[derive(Args, Debug)]
struct RunArgs {
    /// Only run these streams (repeatable or comma-separated).
    #[arg(long, value_delimiter = ',')]
    stream: Vec<String>,
    /// Ignore stored watermarks and fetch everything again.
    #[arg(long)]
    full_refresh: bool,
    /// Fail on records that do not match the spec instead of quarantining them.
    #[arg(long)]
    strict: bool,
}

#[derive(Args, Debug)]
struct UpdateArgs {
    /// New spec URL or file. Default: the spec in apisip.toml.
    spec: Option<String>,
    /// Only print the diff. Exits with code 1 when something changed.
    #[arg(long)]
    check: bool,
    /// Never prompt: keep the current auth and your edited values.
    #[arg(short, long)]
    yes: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    eprintln!("apisip {:?}: not implemented yet", cli.command);
    ExitCode::FAILURE
}
