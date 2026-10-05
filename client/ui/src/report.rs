//! "Copy diagnostic report" (PLAN.md section 9): one Markdown report from
//! the service's logs and the window's, built with the same code as
//! `pingpath-report`, so a tester shares one file for both.
//!
//! The two log folders stay separate (the service's is in ProgramData, the
//! window's in the user's profile), so the report has a part for each:
//! `pingpath-report`'s report for each folder, under its own heading.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use pingpath_diagnostics::report::{ReportOptions, build_report};
use time::OffsetDateTime;

/// Where the service logs when it can't be asked: `%ProgramData%\PingPath\logs`.
pub fn default_service_logs() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("ProgramData")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
            .join("PingPath")
            .join("logs")
    } else {
        PathBuf::from("/var/lib/pingpath/logs")
    }
}

/// The report: the service's part, then the window's. A part that can't
/// be read says why instead of failing the whole report.
pub fn build(service_logs: &Path, ui_logs: Option<&Path>, app_version: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# PingPath diagnostic report\n");
    let _ = writeln!(
        out,
        "From the PingPath app {app_version}. Two parts: the PingPath service, which measures \
         and turns Optimize on and off, and the PingPath app, the window. Public IP addresses \
         are shortened and your home folder is shown as `~`. Read the report before sharing it.\n"
    );
    part(&mut out, "Service", Some(service_logs));
    part(&mut out, "App", ui_logs);
    out
}

fn part(out: &mut String, title: &str, dir: Option<&Path>) {
    let _ = writeln!(out, "## {title}\n");
    let Some(dir) = dir else {
        let _ = writeln!(out, "No log folder is known for this part.\n");
        return;
    };
    let options = ReportOptions::new(dir.to_path_buf());
    match build_report(&options) {
        Ok(report) => {
            out.push_str(&nest(&report));
            out.push('\n');
        }
        Err(error) => {
            // Shown the way the report shows paths: home folder as `~`.
            let mut shown = dir.display().to_string();
            if let Some(home) = &options.home {
                shown = shown.replace(home.as_str(), "~");
            }
            let shown = pingpath_diagnostics::redact_text(&shown);
            tracing::warn!(%error, dir = shown, "cannot read logs for the diagnostic report");
            let _ = writeln!(out, "Cannot read the logs in `{shown}`: {error}\n");
        }
    }
}

/// Puts a report under a level-2 heading: drops its title and moves every
/// heading down a level, leaving code blocks (log lines) untouched.
fn nest(report: &str) -> String {
    let mut out = String::new();
    let mut in_code = false;
    for (i, line) in report.lines().enumerate() {
        if i == 0 && line.starts_with("# ") {
            continue;
        }
        if line.starts_with("```") {
            in_code = !in_code;
        }
        if !in_code && line.starts_with('#') {
            out.push('#');
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Saves the report as `<dir>/pingpath-report-<UTC time>.md`.
pub fn save(report: &str, dir: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let stamp = OffsetDateTime::now_utc()
        .format(time::macros::format_description!(
            "[year][month][day]-[hour][minute][second]"
        ))
        .map_err(io::Error::other)?;
    let path = dir.join(format!("pingpath-report-{stamp}.md"));
    fs::write(&path, report)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nests_headings_outside_code_blocks() {
        let report = "# PingPath diagnostic report\n\n## Components\n\n```\n# not a heading\n```\n### x (last 1 lines)\n";
        assert_eq!(
            nest(report),
            "\n### Components\n\n```\n# not a heading\n```\n#### x (last 1 lines)\n"
        );
    }

    #[test]
    fn builds_both_parts_even_when_one_folder_is_missing() {
        let root = std::env::temp_dir().join(format!("pingpath-ui-report-{}", std::process::id()));
        let ui = root.join("ui");
        fs::create_dir_all(&ui).unwrap();
        fs::write(
            ui.join("ui.2026-10-05.log"),
            r#"{"timestamp":"2026-10-05T10:00:00Z","level":"WARN","fields":{"message":"the PingPath service is unavailable; retrying"},"target":"pingpath_client_ui"}"#,
        )
        .unwrap();
        let report = build(&root.join("missing"), Some(&ui), "0.1.0");
        assert!(report.starts_with("# PingPath diagnostic report\n"));
        assert_eq!(report.matches("\n# ").count(), 0, "one title only");
        let (service, app) = report.split_once("## App").unwrap();
        assert!(service.contains("## Service") && service.contains("No log files found"));
        assert!(app.contains("the PingPath service is unavailable"));

        let saved = save(&report, &root.join("Documents").join("PingPath")).unwrap();
        assert_eq!(fs::read_to_string(&saved).unwrap(), report);
        let _ = fs::remove_dir_all(root);
    }
}
