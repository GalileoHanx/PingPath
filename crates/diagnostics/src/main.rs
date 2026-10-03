//! `pingpath-report`: bundles PingPath's logs into one Markdown file to share
//! when something goes wrong.

use std::path::PathBuf;
use std::process::ExitCode;

use pingpath_diagnostics::log_dir;
use pingpath_diagnostics::report::{ReportOptions, write_report};

const USAGE: &str = "\
Usage: pingpath-report [--out <dir>] [--log-dir <dir>]

Collects PingPath's recent logs, warnings and system details into
pingpath-report-<time>.md. Public IP addresses are shortened and your home
folder is hidden; read the file before sharing it.

  --out <dir>      Where to write the report (default: current folder)
  --log-dir <dir>  Where to read logs from (default: PingPath's log folder)";

fn main() -> ExitCode {
    let mut out_dir = PathBuf::from(".");
    let mut logs = log_dir();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.next()) {
            ("--out", Some(dir)) => out_dir = dir.into(),
            ("--log-dir", Some(dir)) => logs = Some(dir.into()),
            ("-h" | "--help", _) => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            _ => {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            }
        }
    }

    let Some(logs) = logs else {
        eprintln!("error: cannot find the log folder; pass --log-dir <dir>");
        return ExitCode::FAILURE;
    };
    match write_report(&ReportOptions::new(logs), &out_dir) {
        Ok(path) => {
            let path = std::fs::canonicalize(&path).unwrap_or(path);
            println!("Diagnostic report written to {}", path.display());
            println!(
                "Read it, then upload it in the chat, attach it to a GitHub issue, \
                 or copy it into logs/ in the repository (see logs/README.md)."
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: cannot write the report: {e}");
            ExitCode::FAILURE
        }
    }
}
