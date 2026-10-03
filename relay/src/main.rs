//! `pingpath-relay`: run a relay, create its keys, or check its config.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use pingpath_relay::config::{Config, Role};
use pingpath_relay::{Relay, RelayOptions};
use pingpath_tunnel::keys::{self, WireGuardKeys};

#[derive(Parser)]
#[command(
    name = "pingpath-relay",
    version,
    about = "PingPath relay: entry and exit roles (see docs/relay-operator.md)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the relay
    Run {
        #[arg(long, default_value = "/etc/pingpath/relay.toml")]
        config: PathBuf,
    },
    /// Create a WireGuard key pair for the exit role
    Keygen {
        /// Write the private key here (mode 0600) instead of printing it
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Check a config file, its keys and game definitions, then exit
    Check {
        #[arg(long, default_value = "/etc/pingpath/relay.toml")]
        config: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let _logs = matches!(cli.command, Command::Run { .. })
        .then(|| pingpath_diagnostics::init("relay", env!("CARGO_PKG_VERSION")));
    match cli.command {
        Command::Run { config } => run(&config),
        Command::Keygen { out } => keygen(out.as_deref()),
        Command::Check { config } => match prepare(&config) {
            Ok(options) => {
                println!(
                    "ok: relay {} \"{}\", roles {:?}, {} games, {} other relays",
                    options.config.id,
                    options.config.name,
                    options.config.roles,
                    options.games.len(),
                    options.config.relays.len()
                );
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e),
        },
    }
}

/// Loads the config and everything it points to.
fn prepare(path: &Path) -> Result<RelayOptions, String> {
    let config = Config::load(path)?;
    let exit_secret = match (&config.private_key_file, config.has_role(Role::Exit)) {
        (Some(file), true) => Some(read_key(file)?),
        _ => None,
    };
    let control_api_key = match &config.control {
        Some(control) => Some(
            fs::read_to_string(&control.api_key_file)
                .map_err(|e| format!("cannot read {}: {e}", control.api_key_file.display()))?
                .trim()
                .to_string(),
        ),
        None => None,
    };
    let games = pingpath_games::load(config.games_dir.as_deref())?;
    Ok(RelayOptions {
        config,
        games,
        exit_secret,
        control_api_key,
    })
}

fn read_key(path: &Path) -> Result<[u8; 32], String> {
    let text =
        fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    keys::decode(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn run(path: &Path) -> ExitCode {
    let options = match prepare(path) {
        Ok(options) => options,
        Err(e) => return fail(&e),
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => return fail(&format!("cannot start the async runtime: {e}")),
    };
    runtime.block_on(async {
        let relay = match Relay::start(options).await {
            Ok(relay) => relay,
            Err(e) => return fail(&format!("cannot start: {e}")),
        };
        if let Some(key) = relay.exit_public_key() {
            tracing::info!(public_key = %keys::encode(key.as_bytes()), "exit WireGuard public key");
        }
        shutdown_signal().await;
        relay.shutdown();
        ExitCode::SUCCESS
    })
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(term) => term,
                Err(_) => return std::future::pending().await,
            };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn keygen(out: Option<&Path>) -> ExitCode {
    let keys = match WireGuardKeys::generate() {
        Ok(keys) => keys,
        Err(e) => return fail(&e.to_string()),
    };
    match out {
        Some(path) => {
            if let Err(e) = write_private(path, &keys.secret_base64()) {
                return fail(&format!("cannot write {}: {e}", path.display()));
            }
            println!("private key written to {}", path.display());
        }
        None => println!("private key: {}", keys.secret_base64()),
    }
    println!("public key:  {}", keys.public_base64());
    ExitCode::SUCCESS
}

fn write_private(path: &Path, key: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        writeln!(file, "{key}")
    }
    #[cfg(not(unix))]
    {
        fs::write(path, format!("{key}\n"))
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    tracing::error!("{message}");
    ExitCode::FAILURE
}
