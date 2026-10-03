//! End to end: a component logs through `init_in`, and the report built from
//! its log folder shows what a tester needs, with identifying data removed.

use pingpath_diagnostics::report::{ReportOptions, build_report, write_report};
use pingpath_diagnostics::{Console, Redacted, init_in};

#[test]
fn report_summarises_component_logs_without_identifying_data() {
    let logs = tempfile::tempdir().unwrap();
    let guard = init_in(
        "relay",
        "9.9.9",
        Some(logs.path().to_path_buf()),
        Console::Full,
    );

    let peer: std::net::SocketAddr = "8.8.4.4:51820".parse().unwrap();
    tracing::info!(peer = %Redacted(peer), "session started");
    tracing::warn!(relay = "fra-1", "keepalive timeout from 1.1.1.1");
    tracing::info!(config = "/home/tester/pingpath/relay.toml", "config loaded");
    drop(guard); // flushes the log file

    let probe_dir = logs.path().join(pingpath_diagnostics::report::PROBE_DIR);
    std::fs::create_dir_all(&probe_dir).unwrap();
    std::fs::write(
        probe_dir.join("pingpath-probe-dota-2-1.md"),
        "# PingPath probe: Dota 2\n\nhop 8.8.8.8\n",
    )
    .unwrap();

    let mut options = ReportOptions::new(logs.path().to_path_buf());
    options.home = Some("/home/tester".into());
    let report = build_report(&options).unwrap();

    assert!(report.contains("| relay | 9.9.9 |"), "{report}");
    assert!(
        report.contains("keepalive timeout from 1.1.1.x"),
        "{report}"
    );
    assert!(report.contains("peer=8.8.4.x:51820"), "{report}");
    assert!(report.contains("config=~/pingpath/relay.toml"), "{report}");
    assert!(report.contains("#### PingPath probe: Dota 2"), "{report}");
    assert!(report.contains("hop 8.8.8.x"), "{report}");
    for leaked in ["8.8.4.4", "8.8.8.8", "1.1.1.1", "/home/tester"] {
        assert!(!report.contains(leaked), "report leaks {leaked}:\n{report}");
    }

    let out = tempfile::tempdir().unwrap();
    let path = write_report(&options, out.path()).unwrap();
    assert!(
        path.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("pingpath-report-")
    );
}

#[test]
fn report_explains_when_there_are_no_logs() {
    let empty = tempfile::tempdir().unwrap();
    let report = build_report(&ReportOptions::new(empty.path().join("missing"))).unwrap();
    assert!(report.contains("No log files found"), "{report}");
}
