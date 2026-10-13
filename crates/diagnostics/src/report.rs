//! The diagnostic report: one Markdown file with everything needed to
//! understand a problem a tester hit, safe to share once they've read it.

use std::collections::{BTreeMap, VecDeque};
use std::env;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::{LOG_DIR_ENV, LOG_ENV, STARTED, redact_text};

pub struct ReportOptions {
    pub log_dir: PathBuf,
    /// Most recent log lines included per component.
    pub lines_per_component: usize,
    /// Most recent warnings and errors listed in the summary.
    pub max_problems: usize,
    /// Home folder to hide as `~`; defaults to the current user's.
    pub home: Option<String>,
    /// Most recent probe reports (`<log dir>/probe/*.md`) included in full.
    pub probe_reports: usize,
}

impl ReportOptions {
    pub fn new(log_dir: PathBuf) -> Self {
        Self {
            log_dir,
            lines_per_component: 300,
            max_problems: 50,
            home: home_dir(),
            probe_reports: 4,
        }
    }
}

#[derive(Default)]
struct Component {
    files: Vec<String>,
    version: Option<String>,
    last_start: Option<String>,
    lines: VecDeque<String>,
    warnings: usize,
    errors: usize,
}

/// Builds the report from every `*.log` file in the log directory.
pub fn build_report(options: &ReportOptions) -> io::Result<String> {
    let scrub = |text: &str| {
        let text = match &options.home {
            Some(home) => text.replace(home.as_str(), "~"),
            None => text.to_string(),
        };
        redact_text(&text)
    };

    let mut components: BTreeMap<String, Component> = BTreeMap::new();
    let mut problems: Vec<(String, String)> = Vec::new();
    for path in log_files(&options.log_dir)? {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let component_name = name.split('.').next().unwrap_or_default().to_string();
        let component = components.entry(component_name).or_default();
        component.files.push(name);

        for line in BufReader::new(File::open(&path)?).lines() {
            let line = line?;
            let event = Event::parse(&line);
            if event.message == STARTED {
                component.version = event.field("version");
                component.last_start = Some(event.timestamp.clone());
            }
            let rendered = scrub(&event.render());
            match event.level.as_str() {
                "WARN" => component.warnings += 1,
                "ERROR" => component.errors += 1,
                _ => {}
            }
            if matches!(event.level.as_str(), "WARN" | "ERROR") {
                problems.push((event.timestamp.clone(), rendered.clone()));
            }
            component.lines.push_back(rendered);
            if component.lines.len() > options.lines_per_component {
                component.lines.pop_front();
            }
        }
    }
    problems.sort_by(|a, b| a.0.cmp(&b.0));
    let skipped = problems.len().saturating_sub(options.max_problems);

    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default();
    let mut out = String::new();
    // Writing to a String cannot fail.
    let _ = writeln!(out, "# PingPath diagnostic report\n");
    let _ = writeln!(out, "- Generated: {now}");
    let _ = writeln!(
        out,
        "- Report tool: pingpath-report {}",
        env!("CARGO_PKG_VERSION")
    );
    let _ = writeln!(
        out,
        "- System: {} ({})",
        os_description(),
        env::consts::ARCH
    );
    let _ = writeln!(
        out,
        "- Log directory: {}",
        scrub(&options.log_dir.display().to_string())
    );
    let settings: Vec<String> = env::vars()
        .filter(|(k, _)| k.starts_with("PINGPATH_"))
        .map(|(k, v)| format!("`{k}={}`", scrub(&v)))
        .collect();
    let _ = writeln!(
        out,
        "- Settings: {}",
        if settings.is_empty() {
            "defaults".to_string()
        } else {
            settings.join(", ")
        }
    );
    let _ = writeln!(
        out,
        "\n> Public IP addresses are shortened (e.g. `203.0.113.x`) and your home folder is \
         shown as `~`. Read the report before sharing it.\n"
    );

    let _ = writeln!(out, "## Components\n");
    if components.is_empty() {
        let _ = writeln!(
            out,
            "No log files found. Run the component you were testing, then run pingpath-report \
             again. If the logs are elsewhere, set `{LOG_DIR_ENV}` to that folder.\n"
        );
    } else {
        let _ = writeln!(
            out,
            "| Component | Version | Last started | Warnings | Errors | Log files |"
        );
        let _ = writeln!(out, "|---|---|---|---|---|---|");
        for (name, c) in &components {
            let _ = writeln!(
                out,
                "| {name} | {} | {} | {} | {} | {} |",
                c.version.as_deref().unwrap_or("?"),
                c.last_start.as_deref().unwrap_or("?"),
                c.warnings,
                c.errors,
                c.files.join(", ")
            );
        }
        out.push('\n');
    }

    let _ = writeln!(out, "## Warnings and errors\n");
    if problems.is_empty() {
        let _ = writeln!(out, "None.\n");
    } else {
        if skipped > 0 {
            let _ = writeln!(
                out,
                "Showing the latest {} of {}.\n",
                options.max_problems,
                problems.len()
            );
        }
        let _ = writeln!(out, "```");
        for (_, line) in &problems[skipped..] {
            let _ = writeln!(out, "{line}");
        }
        let _ = writeln!(out, "```\n");
    }

    let _ = writeln!(out, "## Latest probe results\n");
    let probes = latest_files(
        &options.log_dir.join(PROBE_DIR),
        "md",
        options.probe_reports,
    )?;
    if probes.is_empty() {
        let _ = writeln!(
            out,
            "None. Run pingpath-probe first if you were testing it.\n"
        );
    }
    for path in probes {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let _ = writeln!(out, "<details><summary>{name}</summary>\n");
        // Demote the probe report's headings so they nest under this section.
        for line in fs::read_to_string(&path)?.lines() {
            let line = if line.starts_with('#') {
                format!("###{line}")
            } else {
                line.to_string()
            };
            let _ = writeln!(out, "{}", scrub(&line));
        }
        let _ = writeln!(out, "\n</details>\n");
    }

    let _ = writeln!(out, "## Recent log lines\n");
    for (name, c) in &components {
        let _ = writeln!(out, "### {name} (last {} lines)\n", c.lines.len());
        let _ = writeln!(out, "```");
        for line in &c.lines {
            let _ = writeln!(out, "{line}");
        }
        let _ = writeln!(out, "```\n");
    }
    if components.is_empty() {
        let _ = writeln!(out, "None.\n");
    }
    let _ = writeln!(
        out,
        "_Log level for more detail: set `{LOG_ENV}=debug` and reproduce the problem._"
    );
    Ok(out)
}

/// Writes the report to `out_dir/pingpath-report-<UTC time>.md` and returns
/// its path.
pub fn write_report(options: &ReportOptions, out_dir: &Path) -> io::Result<PathBuf> {
    let report = build_report(options)?;
    let stamp = OffsetDateTime::now_utc()
        .format(time::macros::format_description!(
            "[year][month][day]-[hour][minute][second]"
        ))
        .map_err(io::Error::other)?;
    let path = out_dir.join(format!("pingpath-report-{stamp}.md"));
    fs::write(&path, report)?;
    Ok(path)
}

/// Where pingpath-probe saves its reports, inside the log directory.
pub const PROBE_DIR: &str = "probe";

/// The `limit` newest files with extension `ext` in `dir`, newest first. A
/// missing directory has none.
fn latest_files(dir: &Path, ext: &str, limit: usize) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|e| e == ext) {
            files.push((entry.metadata()?.modified()?, path));
        }
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    Ok(files.into_iter().take(limit).map(|(_, p)| p).collect())
}

/// `*.log` files in `dir`, sorted so each component's files run oldest to
/// newest. A missing directory has no log files.
fn log_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "log") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// One log line. Lines that aren't JSON (e.g. written by hand or by an
/// older version) are kept verbatim.
struct Event {
    timestamp: String,
    level: String,
    target: String,
    message: String,
    fields: serde_json::Map<String, Value>,
    raw: Option<String>,
}

impl Event {
    fn parse(line: &str) -> Self {
        let Ok(Value::Object(mut obj)) = serde_json::from_str::<Value>(line) else {
            return Self {
                timestamp: String::new(),
                level: String::new(),
                target: String::new(),
                message: String::new(),
                fields: serde_json::Map::new(),
                raw: Some(line.to_string()),
            };
        };
        let mut text = |key: &str| match obj.remove(key) {
            Some(Value::String(s)) => s,
            _ => String::new(),
        };
        let (timestamp, level, target) = (text("timestamp"), text("level"), text("target"));
        let mut fields = match obj.remove("fields") {
            Some(Value::Object(fields)) => fields,
            _ => serde_json::Map::new(),
        };
        let message = match fields.remove("message") {
            Some(Value::String(s)) => s,
            _ => String::new(),
        };
        Self {
            timestamp,
            level,
            target,
            message,
            fields,
            raw: None,
        }
    }

    fn field(&self, key: &str) -> Option<String> {
        self.fields.get(key).map(value_text)
    }

    fn render(&self) -> String {
        if let Some(raw) = &self.raw {
            return raw.clone();
        }
        let mut line = format!(
            "{} {:<5} {}: {}",
            self.timestamp, self.level, self.target, self.message
        );
        for (key, value) in &self.fields {
            let _ = write!(line, " {key}={}", value_text(value));
        }
        line
    }
}

/// A field's value as one line of text: a value holding a line break
/// (some come from other users, through the service's pipe) can't start a
/// line of its own in the report, or close its code block.
fn value_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.replace('\r', "\\r").replace('\n', "\\n"),
        other => other.to_string(),
    }
}

fn home_dir() -> Option<String> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    env::var(var).ok().filter(|h| h.len() > 1)
}

fn os_description() -> String {
    let from_command = |program: &str, args: &[&str]| {
        std::process::Command::new(program)
            .args(args)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let description = if cfg!(windows) {
        from_command("cmd", &["/C", "ver"])
    } else if cfg!(target_os = "macos") {
        from_command("sw_vers", &["-productVersion"]).map(|v| format!("macOS {v}"))
    } else {
        let name = fs::read_to_string("/etc/os-release").ok().and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                .map(|v| v.trim_matches('"').to_string())
        });
        let kernel = fs::read_to_string("/proc/sys/kernel/osrelease").ok();
        match (name, kernel) {
            (Some(n), Some(k)) => Some(format!("{n}, kernel {}", k.trim())),
            (n, k) => n.or(k.map(|k| format!("kernel {}", k.trim()))),
        }
    };
    description.unwrap_or_else(|| env::consts::OS.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_json_events_as_one_line() {
        let line = r#"{"timestamp":"2026-10-03T16:20:00.000001Z","level":"WARN","fields":{"message":"relay timeout","relay":"fra-1","ms":1000},"target":"pingpath_client"}"#;
        let event = Event::parse(line);
        assert_eq!(event.level, "WARN");
        assert_eq!(
            event.render(),
            "2026-10-03T16:20:00.000001Z WARN  pingpath_client: relay timeout ms=1000 relay=fra-1"
        );
    }

    #[test]
    fn keeps_non_json_lines_verbatim() {
        assert_eq!(
            Event::parse("plain text 1.2.3").render(),
            "plain text 1.2.3"
        );
    }
}
