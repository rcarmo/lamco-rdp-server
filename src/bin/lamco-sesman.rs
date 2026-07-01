//! `lamco-sesman` command-line frontend.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use lamco_rdp_server::sesman::{EnsureOptions, SesmanConfig, SessionManager, SessionSize};
use tracing::{Level, info};
use tracing_subscriber::FmtSubscriber;

#[derive(Debug, Parser)]
#[command(name = "lamco-sesman")]
#[command(version, about = "Lamco RDP session manager", long_about = None)]
struct Args {
    /// Optional sesman TOML config path.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Emit JSON output for automation.
    #[arg(long)]
    json: bool,

    /// Verbose logging.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start a session unless a healthy one already exists.
    Ensure {
        /// Force teardown/restart even if a healthy session exists.
        #[arg(long)]
        restart: bool,
        /// Requested client desktop width.
        #[arg(long)]
        width: Option<u32>,
        /// Requested client desktop height.
        #[arg(long)]
        height: Option<u32>,
        /// Client peer/address to record for reconnect auditing.
        #[arg(long)]
        client: Option<String>,
    },
    /// Alias for `ensure --restart`.
    Start {
        /// Requested client desktop width.
        #[arg(long)]
        width: Option<u32>,
        /// Requested client desktop height.
        #[arg(long)]
        height: Option<u32>,
        /// Client peer/address to record for reconnect auditing.
        #[arg(long)]
        client: Option<String>,
    },
    /// Record reconnect metadata and reuse the existing healthy session.
    Reconnect {
        /// Requested client desktop width.
        #[arg(long)]
        width: Option<u32>,
        /// Requested client desktop height.
        #[arg(long)]
        height: Option<u32>,
        /// Client peer/address to record for reconnect auditing.
        #[arg(long)]
        client: Option<String>,
    },
    /// Print current session status.
    Status,
    /// Stop the managed session.
    Stop,
    /// Stop and then start the managed session.
    Restart {
        /// Requested client desktop width.
        #[arg(long)]
        width: Option<u32>,
        /// Requested client desktop height.
        #[arg(long)]
        height: Option<u32>,
        /// Client peer/address to record for reconnect auditing.
        #[arg(long)]
        client: Option<String>,
    },
    /// Ensure the session exists, then stay in the foreground and monitor it.
    Run {
        /// Force teardown/restart before entering the monitor loop.
        #[arg(long)]
        restart: bool,
        /// Poll interval in seconds.
        #[arg(long, default_value_t = 1)]
        poll_seconds: u64,
        /// Requested client desktop width.
        #[arg(long)]
        width: Option<u32>,
        /// Requested client desktop height.
        #[arg(long)]
        height: Option<u32>,
        /// Client peer/address to record for reconnect auditing.
        #[arg(long)]
        client: Option<String>,
    },
    /// Print the effective default TOML config.
    GenerateConfig,
}

fn main() -> Result<()> {
    let args = Args::parse();
    init_logging(args.verbose);

    if matches!(args.command, Command::GenerateConfig) {
        let toml = toml::to_string_pretty(&SesmanConfig::default())?;
        print!("{toml}");
        return Ok(());
    }

    let config = SesmanConfig::load(args.config.as_deref())?;
    let manager = SessionManager::new(config);

    match args.command {
        Command::Ensure {
            restart,
            width,
            height,
            client,
        } => {
            let result = manager.ensure(EnsureOptions {
                force_restart: restart,
                requested_size: requested_size(width, height),
                client_peer: client,
            })?;
            print_output(args.json, &result)?;
        }
        Command::Start {
            width,
            height,
            client,
        }
        | Command::Restart {
            width,
            height,
            client,
        } => {
            let result = manager.ensure(EnsureOptions {
                force_restart: true,
                requested_size: requested_size(width, height),
                client_peer: client,
            })?;
            print_output(args.json, &result)?;
        }
        Command::Reconnect {
            width,
            height,
            client,
        } => {
            let result = manager.ensure(EnsureOptions {
                force_restart: false,
                requested_size: requested_size(width, height),
                client_peer: client,
            })?;
            print_output(args.json, &result)?;
        }
        Command::Status => {
            let status = manager.status()?;
            print_output(args.json, &status)?;
        }
        Command::Stop => {
            let status = manager.stop()?;
            print_output(args.json, &status)?;
        }
        Command::Run {
            restart,
            poll_seconds,
            width,
            height,
            client,
        } => {
            let result = manager.ensure(EnsureOptions {
                force_restart: restart,
                requested_size: requested_size(width, height),
                client_peer: client,
            })?;
            print_output(args.json, &result)?;
            monitor_foreground(&manager, poll_seconds)?;
        }
        Command::GenerateConfig => unreachable!("handled before config load"),
    }

    Ok(())
}

fn requested_size(width: Option<u32>, height: Option<u32>) -> Option<SessionSize> {
    match (width, height) {
        (Some(width), Some(height)) => Some(SessionSize { width, height }),
        _ => None,
    }
}

fn monitor_foreground(manager: &SessionManager, poll_seconds: u64) -> Result<()> {
    let interval = std::time::Duration::from_secs(poll_seconds.max(1));
    loop {
        std::thread::sleep(interval);
        let status = manager.status()?;
        if status.health != lamco_rdp_server::sesman::SessionHealth::Healthy {
            anyhow::bail!(
                "managed session is no longer healthy: {:?} dead={:?}",
                status.health,
                status.dead_components
            );
        }
    }
}

fn print_output<T>(json: bool, value: &T) -> Result<()>
where
    T: serde::Serialize + std::fmt::Debug,
{
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{value:#?}");
    }
    Ok(())
}

fn init_logging(verbose: u8) {
    let level = match verbose {
        0 => Level::WARN,
        1 => Level::INFO,
        _ => Level::DEBUG,
    };
    let subscriber = FmtSubscriber::builder()
        .with_max_level(level)
        .with_target(false)
        .finish();
    if tracing::subscriber::set_global_default(subscriber).is_ok() {
        info!("lamco-sesman logging initialized");
    }
}
