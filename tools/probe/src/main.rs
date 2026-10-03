//! `pingpath-probe`: measures the connection from this PC to a game's server
//! regions and says where the problem is. Run without arguments (or
//! double-click it) to probe every known game.

use std::fs;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use pingpath_diagnostics::report::PROBE_DIR;
use pingpath_games::GameDefinition;
use pingpath_probe::measure::SeriesOptions;
use pingpath_probe::probe::{self, ProbeOptions};
use pingpath_probe::report::Report;
use pingpath_probe::{catalog, geo, sdr, udp};
use time::OffsetDateTime;

#[derive(Parser)]
#[command(
    name = "pingpath-probe",
    version,
    about = "Measures latency, jitter, loss and the route from this PC to a game's server regions, \
             and says in plain words where the problem is.",
    after_help = "Run without a command to probe every known game. Reports are saved in the \
                  PingPath log folder; run pingpath-report afterwards to bundle everything into \
                  one file to share."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Measure a game's server regions (all games if none is given)
    Run(RunArgs),
    /// List the games the probe knows
    List {
        #[arg(long)]
        games_dir: Option<PathBuf>,
    },
    /// Print a game's server ranges from Valve's relay list, to paste into its definition
    SdrImport {
        game: String,
        #[arg(long)]
        games_dir: Option<PathBuf>,
    },
    /// Run a UDP echo server for the UDP test (--udp-echo)
    EchoServer {
        #[arg(long, default_value = "0.0.0.0:7777")]
        bind: SocketAddr,
    },
    /// List the cities --from understands
    Cities,
}

#[derive(Args)]
struct RunArgs {
    /// Game ID, as shown by `list`; omit to probe every game
    game: Option<String>,
    /// Only this region (repeatable)
    #[arg(long = "region", value_name = "ID")]
    regions: Vec<String>,
    /// Probes per target
    #[arg(long, default_value_t = 20)]
    count: u32,
    /// Milliseconds between probes
    #[arg(long, default_value_t = 200)]
    interval_ms: u64,
    /// Milliseconds to wait for each reply
    #[arg(long, default_value_t = 1000)]
    timeout_ms: u64,
    /// Don't trace the route
    #[arg(long)]
    no_trace: bool,
    /// Probes per hop when tracing the route
    #[arg(long, default_value_t = 10)]
    trace_probes: u32,
    #[arg(long, default_value_t = 30)]
    max_hops: u8,
    /// Your city, for the distance floor (see `cities`)
    #[arg(long, value_name = "CITY")]
    from: Option<String>,
    /// Your location as LAT,LON, instead of --from
    #[arg(long, value_name = "LAT,LON", conflicts_with = "from")]
    from_coords: Option<String>,
    /// A PingPath echo server (HOST:PORT) for the UDP and WireGuard-throttling test
    #[arg(long, value_name = "HOST:PORT")]
    udp_echo: Option<String>,
    /// Skip the public IP, ISP and per-hop network lookups
    #[arg(long)]
    no_lookups: bool,
    /// Read game definitions from this folder instead of the built-in ones
    #[arg(long)]
    games_dir: Option<PathBuf>,
    /// Save reports here instead of the PingPath log folder
    #[arg(long)]
    out: Option<PathBuf>,
}

fn main() -> ExitCode {
    let _logs = pingpath_diagnostics::init_cli("probe", env!("CARGO_PKG_VERSION"));
    let command = Cli::parse().command.unwrap_or_else(|| {
        Cli::parse_from(["pingpath-probe", "run"])
            .command
            .expect("run parses")
    });
    let code = match command {
        Command::Run(args) => run(args),
        Command::List { games_dir } => list(games_dir.as_deref()),
        Command::SdrImport { game, games_dir } => sdr_import(&game, games_dir.as_deref()),
        Command::EchoServer { bind } => echo_server(bind),
        Command::Cities => {
            for (name, lat, lon) in geo::CITIES {
                println!("{name:<12} {lat:>6.2}, {lon:>6.2}");
            }
            ExitCode::SUCCESS
        }
    };
    pingpath_diagnostics::pause_if_double_clicked();
    code
}

fn run(args: RunArgs) -> ExitCode {
    let games = match catalog::load(args.games_dir.as_deref()) {
        Ok(games) => games,
        Err(e) => return fail(&e),
    };
    let selected: Vec<&GameDefinition> = match &args.game {
        Some(id) => match catalog::find(&games, id) {
            Ok(game) => vec![game],
            Err(e) => return fail(&e),
        },
        None => games.iter().collect(),
    };

    let origin = match (&args.from, &args.from_coords) {
        (Some(city), _) => match geo::city(city) {
            Some(origin) => Some(origin),
            None => {
                return fail(&format!(
                    "unknown city \"{city}\"; run `pingpath-probe cities`, or use --from-coords LAT,LON"
                ));
            }
        },
        (None, Some(coords)) => match geo::coordinates(coords) {
            Some(origin) => Some(origin),
            None => return fail(&format!("\"{coords}\" is not LAT,LON (e.g. 24.71,46.68)")),
        },
        (None, None) => None,
    };
    let udp_echo = match &args.udp_echo {
        Some(endpoint) => match endpoint.to_socket_addrs().ok().and_then(|mut a| a.next()) {
            Some(addr) => Some(addr),
            None => return fail(&format!("cannot resolve echo server \"{endpoint}\"")),
        },
        None => None,
    };
    let options = ProbeOptions {
        series: SeriesOptions {
            count: args.count.max(1),
            interval: Duration::from_millis(args.interval_ms),
            timeout: Duration::from_millis(args.timeout_ms.max(1)),
        },
        trace: !args.no_trace,
        trace_probes: args.trace_probes.max(1),
        max_hops: args.max_hops.clamp(1, 64),
        regions: args.regions,
        origin,
        udp_echo,
        lookups: !args.no_lookups,
        ..ProbeOptions::default()
    };

    let out_dir = args
        .out
        .or_else(|| pingpath_diagnostics::log_dir().map(|d| d.join(PROBE_DIR)))
        .unwrap_or_else(|| PathBuf::from("."));
    for game in selected {
        eprintln!("Probing {} (about half a minute)...", game.name);
        let report = probe::run(game, &options);
        println!("{}", report.to_markdown());
        match save(&report, &out_dir) {
            Ok((json, md)) => eprintln!("Saved {} and {}\n", json.display(), md.display()),
            Err(e) => {
                tracing::error!(error = %e, dir = %out_dir.display(), "cannot save the probe report");
                eprintln!("Could not save the report in {}: {e}\n", out_dir.display());
            }
        }
    }
    eprintln!("Done. Run pingpath-report to bundle the results and logs into one file to share.");
    ExitCode::SUCCESS
}

fn save(report: &Report, dir: &Path) -> std::io::Result<(PathBuf, PathBuf)> {
    fs::create_dir_all(dir)?;
    let stamp = OffsetDateTime::now_utc()
        .format(time::macros::format_description!(
            "[year][month][day]-[hour][minute][second]"
        ))
        .map_err(std::io::Error::other)?;
    let base = dir.join(format!("pingpath-probe-{}-{stamp}", report.game.id));
    let (json, md) = (base.with_extension("json"), base.with_extension("md"));
    fs::write(&json, report.to_json())?;
    fs::write(&md, report.to_markdown())?;
    let canonical = |p: PathBuf| fs::canonicalize(&p).unwrap_or(p);
    Ok((canonical(json), canonical(md)))
}

fn list(games_dir: Option<&Path>) -> ExitCode {
    let games = match catalog::load(games_dir) {
        Ok(games) => games,
        Err(e) => return fail(&e),
    };
    for game in games {
        println!("{} ({}, {})", game.id, game.name, game.status);
        for region in &game.regions {
            let mut sources = Vec::new();
            if !region.sdr_pops.is_empty() {
                sources.push(format!("Valve relays {}", region.sdr_pops.join(", ")));
            }
            if !region.ping_targets.is_empty() {
                sources.push(format!("{} ping target(s)", region.ping_targets.len()));
            }
            let sources = if sources.is_empty() {
                "nothing to measure yet".to_string()
            } else {
                sources.join(", ")
            };
            println!("  {:<12} {:<24} {sources}", region.id, region.name);
        }
    }
    ExitCode::SUCCESS
}

fn sdr_import(id: &str, games_dir: Option<&Path>) -> ExitCode {
    let games = match catalog::load(games_dir) {
        Ok(games) => games,
        Err(e) => return fail(&e),
    };
    let game = match catalog::find(&games, id) {
        Ok(game) => game,
        Err(e) => return fail(&e),
    };
    let Some(app_id) = game.sdr_app_id else {
        return fail(&format!("{} has no sdr_app_id", game.name));
    };
    let config = match sdr::fetch(app_id) {
        Ok(config) => config,
        Err(e) => return fail(&e),
    };

    println!(
        "# {} relay list, revision {:?}, from {}",
        game.name,
        config.revision,
        sdr::url(app_id)
    );
    println!(
        "# Paste the server_ranges into games/{}.yaml.\nregions:",
        game.id
    );
    for region in &game.regions {
        let relays: Vec<sdr::Relay> = region
            .sdr_pops
            .iter()
            .filter_map(|pop| config.pops.get(pop))
            .flat_map(|p| p.relays.iter().copied())
            .collect();
        let ranges = sdr::prefixes(&relays)
            .iter()
            .map(|r| format!("\"{r}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let ports: Vec<String> = relays
            .iter()
            .filter_map(|r| r.ports)
            .map(|(a, b)| format!("{a}-{b}"))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        println!(
            "  - id: {}\n    server_ranges: [{ranges}]\n    # relay ports: {}",
            region.id,
            ports.join(", ")
        );
        tracing::info!(region = %region.id, ranges = %ranges, ports = %ports.join(", "), "imported ranges");
    }
    println!("\n# All points of presence in the list:");
    for (code, pop) in &config.pops {
        let line = format!(
            "{code:<5} {:<32} {} relays",
            pop.description.as_deref().unwrap_or("?"),
            pop.relays.len()
        );
        println!("#   {line}");
        tracing::info!(pop = %code, description = ?pop.description, relays = pop.relays.len(), "relay pop");
    }
    ExitCode::SUCCESS
}

fn echo_server(bind: SocketAddr) -> ExitCode {
    eprintln!("UDP echo server on {bind}. Stop it with Ctrl+C.");
    match udp::serve(bind) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&format!("echo server stopped: {e}")),
    }
}

/// Logs the error, which also prints it on the console.
fn fail(message: &str) -> ExitCode {
    tracing::error!("{message}");
    ExitCode::FAILURE
}
