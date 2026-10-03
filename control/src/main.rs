//! `pingpath-control`: run the control plane, create its signing key or a
//! relay's API key, or check its config.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use pingpath_control::config::Config;
use pingpath_control::state::{api_key_hash, hex};
use pingpath_control::{Control, ControlOptions};
use pingpath_tunnel::keys;
use pingpath_tunnel::token::Issuer;

#[derive(Parser)]
#[command(
    name = "pingpath-control",
    version,
    about = "PingPath control plane: session tokens, relay registry and latency (see docs/relay-operator.md)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the control plane
    Run {
        #[arg(long, default_value = "/etc/pingpath/control.toml")]
        config: PathBuf,
    },
    /// Create the Ed25519 key that signs session tokens
    Keygen {
        /// Write the private seed here (mode 0600) instead of printing it
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Create an API key for a relay to report latency with
    ApiKey {
        /// Write the key here (mode 0600) instead of printing it
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Check a config file, its key and game definitions, then exit
    Check {
        #[arg(long, default_value = "/etc/pingpath/control.toml")]
        config: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let _logs = matches!(cli.command, Command::Run { .. })
        .then(|| pingpath_diagnostics::init("control", env!("CARGO_PKG_VERSION")));
    match cli.command {
        Command::Run { config } => run(&config),
        Command::Keygen { out } => keygen(out.as_deref()),
        Command::ApiKey { out } => api_key(out.as_deref()),
        Command::Check { config } => match prepare(&config) {
            Ok(options) => {
                let issuer = Issuer::from_seed(&options.issuer_seed).expect("checked in prepare");
                println!(
                    "ok: {} relays, {} games; relays must trust issuer key {}",
                    options.config.relays.len(),
                    options.games.len(),
                    keys::encode(&issuer.public_key())
                );
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e),
        },
    }
}

/// Loads the config and everything it points to.
fn prepare(path: &Path) -> Result<ControlOptions, String> {
    let config = Config::load(path)?;
    let file = &config.issuer_seed_file;
    let text =
        fs::read_to_string(file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let issuer_seed = keys::decode(&text).map_err(|e| format!("{}: {e}", file.display()))?;
    Issuer::from_seed(&issuer_seed).map_err(|e| format!("{}: {e}", file.display()))?;
    let games = pingpath_games::load(config.games_dir.as_deref())?;
    Ok(ControlOptions {
        config,
        games,
        issuer_seed,
    })
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
        let control = match Control::start(options).await {
            Ok(control) => control,
            Err(e) => return fail(&format!("cannot start: {e}")),
        };
        shutdown_signal().await;
        control.shutdown();
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
    let seed = match keys::random_32() {
        Ok(seed) => seed,
        Err(e) => return fail(&e.to_string()),
    };
    let issuer = Issuer::from_seed(&seed).expect("any 32 bytes are a valid seed");
    if let Err(code) = emit_secret(out, "signing seed", &keys::encode(&seed)) {
        return code;
    }
    println!("public key:   {}", keys::encode(&issuer.public_key()));
    println!("Put the public key in every relay's issuer_public_keys.");
    ExitCode::SUCCESS
}

fn api_key(out: Option<&Path>) -> ExitCode {
    let key = match keys::random_32() {
        Ok(bytes) => hex(&bytes),
        Err(e) => return fail(&e.to_string()),
    };
    if let Err(code) = emit_secret(out, "API key", &key) {
        return code;
    }
    println!("api_key_sha256 = \"{}\"", api_key_hash(&key));
    println!(
        "Give the key to the relay (its control.api_key_file); put the hash in its [[relays]] entry here."
    );
    ExitCode::SUCCESS
}

fn emit_secret(out: Option<&Path>, what: &str, secret: &str) -> Result<(), ExitCode> {
    match out {
        Some(path) => {
            write_private(path, secret)
                .map_err(|e| fail(&format!("cannot write {}: {e}", path.display())))?;
            println!("{what} written to {}", path.display());
        }
        None => println!("{what}: {secret}"),
    }
    Ok(())
}

fn write_private(path: &Path, secret: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        writeln!(file, "{secret}")
    }
    #[cfg(not(unix))]
    {
        fs::write(path, format!("{secret}\n"))
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    tracing::error!("{message}");
    ExitCode::FAILURE
}
