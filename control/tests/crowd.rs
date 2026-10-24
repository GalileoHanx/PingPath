//! PLAN.md rule 9 for crowd data (docs/decisions/012-crowd-data-and-dashboard.md):
//! reports sent from known addresses through a real control plane leave
//! no address, no /24 or /48, and no contributor ID in the database (its
//! write-ahead log and shared-memory files included) or in the logs, read
//! at TRACE level. And the records are there, with the place the address
//! files gave, so the check isn't vacuous.

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pingpath_control::config::{Config, CrowdConfig, CrowdLimits, RelayEntry, Role};
use pingpath_control::{Control, ControlOptions};
use pingpath_games::{AntiCheat, GameDefinition, PortRange, Ports, Region, SupportStatus};
use pingpath_tunnel::keys;
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(5);

/// The addresses reports come from, as the proxy header gives them.
const ADDRESSES: [&str; 4] = [
    "198.51.100.77",
    "203.0.113.9",
    "192.0.2.10",
    "2001:db8:1234:5678::9",
];

/// Everything that would mean an address or a network was kept, as text
/// and as bytes.
fn address_needles() -> Vec<(String, Vec<u8>)> {
    let text = |s: &str| (s.to_string(), s.as_bytes().to_vec());
    let mut needles = vec![];
    for address in ADDRESSES {
        needles.push(text(address));
        match address.parse::<std::net::IpAddr>().unwrap() {
            std::net::IpAddr::V4(v4) => {
                let [a, b, c, d] = v4.octets();
                needles.push((format!("{address} as bytes"), vec![a, b, c, d]));
                needles.push((format!("{address}'s network as bytes"), vec![a, b, c, 0]));
                needles.push(text(&format!("{a}.{b}.{c}.")));
                needles.push(text(&format!("{a}.{b}.{c}.0")));
            }
            std::net::IpAddr::V6(v6) => {
                let octets = v6.octets();
                needles.push((format!("{address} as bytes"), octets.to_vec()));
                needles.push((format!("{address}'s /48 as bytes"), octets[..6].to_vec()));
                needles.push(text("2001:db8:1234"));
                needles.push(text("2001:0db8:1234"));
            }
        }
    }
    needles
}

fn contributor(n: u8) -> [u8; 16] {
    // Not a repeated byte, so it can't turn up by chance.
    std::array::from_fn(|i| n.wrapping_mul(31).wrapping_add(i as u8 * 17) ^ 0x5a)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name)
}

fn game() -> GameDefinition {
    GameDefinition {
        id: "cs2".into(),
        name: "Counter-Strike 2".into(),
        status: SupportStatus::Testing,
        status_reason: None,
        report: None,
        anti_cheat: vec![AntiCheat::None],
        ports: Ports {
            udp: vec![PortRange {
                first: 27015,
                last: 27015,
            }],
            tcp: vec![],
        },
        sdr_app_id: None,
        regions: vec![Region {
            id: "eu".into(),
            name: "Europe".into(),
            server_ranges: vec!["127.0.0.0/8".parse().unwrap()],
            ping_targets: vec![],
            sdr_pops: vec![],
            location: None,
        }],
        notes: None,
    }
}

fn config(state_dir: &Path) -> Config {
    Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        admin_listen: Some("127.0.0.1:0".parse().unwrap()),
        issuer_seed_file: "unused".into(),
        games_dir: None,
        client_ip_header: Some("X-Forwarded-For".into()),
        sessions: Default::default(),
        doctor: Default::default(),
        crowd: Some(CrowdConfig {
            state_dir: state_dir.to_path_buf(),
            city_db: Some(fixture("geo-city.mmdb")),
            asn_db: Some(fixture("geo-asn.mmdb")),
            excluded_asns: vec![],
            retention_days: 30,
            limits: CrowdLimits::default(),
        }),
        relays: vec![RelayEntry {
            id: 1,
            name: "fra-1".into(),
            address: "127.0.0.1:9".parse().unwrap(),
            roles: vec![Role::Exit, Role::Entry],
            regions: vec!["eu".into()],
            public_key: Some(keys::encode(&[1; 32])),
            api_key_sha256: None,
            provider: None,
            location: None,
            doctor: None,
        }],
    }
}

/// Collects every log line, at every level.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn send(method: &'static str, url: String, from: &'static str, body: Value) -> (u16, Value) {
    tokio::task::spawn_blocking(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(TIMEOUT))
            .build()
            .into();
        let request = match method {
            "POST" => agent.post(&url),
            _ => agent.delete(&url).force_send_body(),
        };
        let mut response = request
            .header("X-Forwarded-For", from)
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .expect("the control plane answers");
        let status = response.status().as_u16();
        let text = response.body_mut().read_to_string().unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    })
    .await
    .unwrap()
}

fn report(who: u8) -> Value {
    json!({
        "contributor": hex(&contributor(who)),
        "client": {"source": "app", "version": "0.1.0"},
        "records": [
            {"kind": "measurement", "game": "cs2",
             "regions": [{"region": "eu", "rtt_ms": 95.0, "loss_pct": 0.0, "jitter_ms": 1.5}],
             "chosen": {"region": "eu", "exit": 1, "entry": 1}},
            {"kind": "session", "game": "cs2", "region": "eu", "exit": 1, "entry": 1,
             "direct_ms": 95.0, "ping_ms": 70.0, "loss_pct": 0.1, "jitter_ms": 1.0}
        ]
    })
}

/// Every file of the database: the file, its write-ahead log and its
/// shared memory, whichever exist.
fn database_bytes(dir: &Path) -> Vec<(String, Vec<u8>)> {
    ["crowd.sqlite", "crowd.sqlite-wal", "crowd.sqlite-shm"]
        .iter()
        .filter_map(|name| {
            std::fs::read(dir.join(name))
                .ok()
                .map(|bytes| (name.to_string(), bytes))
        })
        .collect()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn assert_clean(what: &str, bytes: &[u8], contributors: &[[u8; 16]]) {
    for (name, needle) in address_needles() {
        assert!(!contains(bytes, &needle), "{what} holds {name}");
    }
    for id in contributors {
        assert!(!contains(bytes, id), "{what} holds a contributor ID");
        assert!(
            !contains(bytes, hex(id).as_bytes()),
            "{what} holds a contributor ID in hex"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_that_identifies_a_player_is_written() {
    let logs = Logs::default();
    let writer = logs.clone();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .init();

    let dir = tempfile::tempdir().unwrap();
    let control = Control::start(ControlOptions {
        config: config(dir.path()),
        games: vec![game()],
        issuer_seed: [9; 32],
    })
    .await
    .unwrap();
    let api = format!("http://{}", control.api_addr());
    let admin: SocketAddr = control.admin_addr().unwrap();

    let contributors: Vec<[u8; 16]> = (1..=ADDRESSES.len() as u8).map(contributor).collect();
    for (who, from) in ADDRESSES.iter().enumerate() {
        let (status, body) = send(
            "POST",
            format!("{api}/v1/crowd/reports"),
            from,
            report(who as u8 + 1),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["stored"], 2, "{body}");
    }
    // The last one changes their mind.
    let (status, body) = send(
        "DELETE",
        format!("{api}/v1/crowd/contributor"),
        ADDRESSES[3],
        json!({"contributor": hex(&contributors[3])}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["deleted"], 2);
    // A report that isn't JSON is refused, so a web page can't send one.
    let refused = tokio::task::spawn_blocking({
        let url = format!("{api}/v1/crowd/reports");
        move || {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .http_status_as_error(false)
                .build()
                .into();
            agent
                .post(&url)
                .header("Content-Type", "text/plain")
                .send(report(9).to_string())
                .unwrap()
                .status()
                .as_u16()
        }
    })
    .await
    .unwrap();
    assert_eq!(refused, 415);

    // The records are there, with the places the address files gave.
    let state_dir = dir.path().to_path_buf();
    let places: Vec<(String, Option<String>, Option<i64>, i64)> = tokio::task::spawn_blocking({
        let path = state_dir.join("crowd.sqlite");
        move || {
            // The writer works through its queue: wait for the rows.
            for _ in 0..50 {
                let connection = rusqlite::Connection::open(&path).unwrap();
                let rows: Vec<(String, Option<String>, Option<i64>, i64)> = connection
                    .prepare(
                        "SELECT country, city, asn, COUNT(*) FROM records
                         GROUP BY country, city, asn ORDER BY country, city",
                    )
                    .unwrap()
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect();
                if rows.iter().map(|r| r.3).sum::<i64>() == 6 {
                    return rows;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            panic!("the records never arrived");
        }
    })
    .await
    .unwrap();
    assert_eq!(
        places,
        [
            ("KW".into(), None, Some(64502), 2),
            ("SA".into(), Some("Jeddah".into()), Some(64501), 2),
            ("SA".into(), Some("Riyadh".into()), Some(64500), 2),
        ]
    );

    let metrics = tokio::task::spawn_blocking(move || {
        ureq::get(&format!("http://{admin}/metrics"))
            .call()
            .unwrap()
            .body_mut()
            .read_to_string()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        metrics.contains("pingpath_control_crowd_reports_total 4"),
        "{metrics}"
    );
    assert!(
        metrics.contains("pingpath_control_crowd_forgets_total 1"),
        "{metrics}"
    );

    // While it runs, and after it stops (its last checkpoint written).
    for (name, bytes) in database_bytes(&state_dir) {
        assert_clean(&format!("{name}, running"), &bytes, &contributors);
    }
    let state = control.state().clone();
    control.shutdown();
    drop(state);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let files = database_bytes(&state_dir);
    assert!(!files.is_empty());
    for (name, bytes) in files {
        assert_clean(&format!("{name}, stopped"), &bytes, &contributors);
    }

    let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        logged.matches("crowd report").count() >= 4,
        "the reports weren't logged at all: {logged}"
    );
    assert!(
        logged.contains("a contributor's records deleted"),
        "{logged}"
    );
    assert_clean("the log", logged.as_bytes(), &contributors);
}
