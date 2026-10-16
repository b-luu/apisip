use std::path::PathBuf;
use std::process::ExitCode;

use apisip::cmd;
use apisip::workspace::Workspace;
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
    init_tracing(cli.verbose);
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn exit(ok: bool) -> ExitCode {
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn init_tracing(verbose: u8) {
    let default = match verbose {
        0 => "warn",
        1 => "apisip=info,warn",
        2 => "apisip=debug,info",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_target(false)
        .init();
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let ws = Workspace::new(cli.dir)?;
    // Load .env before the runtime starts any thread. Variables already set
    // in the process environment win over the file.
    let env = ws.env_path();
    if env.exists() {
        dotenvy::from_path(&env).map_err(|e| {
            anyhow::anyhow!(
                "reading {}: {}",
                env.display(),
                apisip::envfile::describe(&e)
            )
        })?;
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        match cli.command {
            Command::Init(a) => {
                cmd::init::run(
                    &ws,
                    cmd::init::InitOptions {
                        spec: a.spec,
                        force: a.force,
                        yes: a.yes,
                        destination: a.destination,
                        format: a.format,
                        auth: a.auth,
                        plan: a.plan,
                        streams: a.streams,
                        base_url: a.base_url,
                        s3_endpoint: a.s3_endpoint,
                        s3_region: a.s3_region,
                    },
                )
                .await?;
                Ok(ExitCode::SUCCESS)
            }
            Command::List => {
                cmd::list::run(&ws).await?;
                Ok(ExitCode::SUCCESS)
            }
            Command::Check => Ok(exit(cmd::check::run(&ws).await?)),
            Command::Run(a) => Ok(exit(
                cmd::run::run(
                    &ws,
                    cmd::run::RunOptions {
                        streams: a.stream,
                        full_refresh: a.full_refresh,
                        strict: a.strict,
                    },
                )
                .await?,
            )),
            Command::Update(a) => Ok(exit(
                cmd::update::run(
                    &ws,
                    cmd::update::UpdateOptions {
                        spec: a.spec,
                        check: a.check,
                        yes: a.yes,
                    },
                )
                .await?,
            )),
        }
    })
}
