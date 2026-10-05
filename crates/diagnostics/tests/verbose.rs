//! "Verbose logging" changes what reaches the log file while running, with
//! no restart (its own test binary: logging is set up once per process).

use pingpath_diagnostics::{Console, init_in};

#[test]
fn verbose_logging_switches_on_and_off_without_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let guard = init_in("verbose", "0", Some(dir.path().into()), Console::Quiet);
    let level = guard.level();

    tracing::debug!("hidden before");
    level.set_verbose(true);
    tracing::debug!("shown while verbose");
    level.set_verbose(false);
    tracing::debug!("hidden after");
    drop(guard);

    let mut text = String::new();
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        text += &std::fs::read_to_string(entry.unwrap().path()).unwrap();
    }
    assert!(text.contains("shown while verbose"), "{text}");
    assert!(!text.contains("hidden before"));
    assert!(!text.contains("hidden after"));
}
