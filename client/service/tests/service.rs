//! The service as the UI sees it: the protocol, the settings players may
//! change, and users of one PC kept from each other's sessions. The engine
//! runs on the in-memory network; connections are in-process streams, so
//! tests can play two different users.

use std::sync::Arc;
use std::time::Duration;

use pingpath_client_core::config::ClientConfig;
use pingpath_client_core::ipc::{MAX_LINE, PROTOCOL_VERSION};
use pingpath_client_core::measure::{Fixed, ProbePlan};
use pingpath_client_core::net::fake::FakeNetwork;
use pingpath_client_core::select::Leg;
use pingpath_service::paths::Paths;
use pingpath_service::run::{self, Options};
use pingpath_service::server::{Caller, Limits, Service};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::watch;

const ALICE: Caller = Caller::WindowsSession(1);
const BOB: Caller = Caller::WindowsSession(2);

struct World {
    service: Arc<Service>,
    paths: Paths,
    _stop: watch::Sender<bool>,
    stop: watch::Receiver<bool>,
    _dir: tempfile::TempDir,
}

fn world_with(limits: Limits, config: ClientConfig) -> World {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path().join("PingPath"));
    std::fs::create_dir_all(&paths.data).unwrap();
    config.save(&paths.config()).unwrap();
    let service = run::start(Options {
        paths: paths.clone(),
        platform: Arc::new(FakeNetwork::new()),
        direct: Arc::new(Fixed(Some(Leg {
            rtt_ms: 120.0,
            loss_pct: 0.0,
        }))),
        probes: ProbePlan::default(),
        log_level: pingpath_diagnostics::init_in(
            "test",
            "0",
            None,
            pingpath_diagnostics::Console::Quiet,
        )
        .level(),
        limits,
    });
    let (stop_tx, stop) = watch::channel(false);
    World {
        service,
        paths,
        _stop: stop_tx,
        stop,
        _dir: dir,
    }
}

fn world() -> World {
    world_with(Limits::default(), ClientConfig::default())
}

struct Client {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
    next_id: u64,
}

impl World {
    /// A UI connection from `caller`, or the error the service refused it with.
    fn connect(&self, caller: Caller) -> Result<Client, String> {
        let permit = self.service.try_slot().ok_or("full")?;
        let slot = self
            .service
            .claim(caller, permit)
            .map_err(|e| e.code.clone())?;
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        tokio::spawn(self.service.clone().serve(theirs, slot, self.stop.clone()));
        let (reader, writer) = tokio::io::split(ours);
        Ok(Client {
            reader: BufReader::new(reader),
            writer,
            next_id: 1,
        })
    }

    async fn greeted(&self, caller: Caller) -> Client {
        let mut client = self.connect(caller).unwrap();
        let welcome = client
            .call(
                "hello",
                json!({"version": PROTOCOL_VERSION, "client": "test"}),
            )
            .await
            .unwrap();
        assert_eq!(welcome["version"], PROTOCOL_VERSION);
        client
    }
}

impl Client {
    async fn send_line(&mut self, line: &str) {
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.write_all(b"\n").await.unwrap();
    }

    /// The next line, or `None` once the service closed the connection.
    async fn line(&mut self) -> Option<Value> {
        let mut line = String::new();
        // Longer than the service's own hello timeout, which tests wait for.
        let read = tokio::time::timeout(Duration::from_secs(60), self.reader.read_line(&mut line))
            .await
            .expect("the service answers")
            .unwrap();
        (read > 0).then(|| serde_json::from_str(&line).unwrap())
    }

    /// Sends a request and waits for its answer, skipping events.
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.next_id;
        self.next_id += 1;
        let request = if params.is_null() {
            json!({"id": id, "method": method})
        } else {
            json!({"id": id, "method": method, "params": params})
        };
        self.send_line(&request.to_string()).await;
        loop {
            let message = self.line().await.expect("connection open");
            if message["id"] == id {
                return match message.get("error") {
                    Some(error) => Err(error.clone()),
                    None => Ok(message["result"].clone()),
                };
            }
        }
    }
}

fn code(error: Result<Value, Value>) -> String {
    error.unwrap_err()["code"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn the_ui_must_say_hello_first_in_the_same_protocol_version() {
    let world = world();
    let mut client = world.connect(ALICE).unwrap();
    assert_eq!(
        code(client.call("status", Value::Null).await),
        "hello_required"
    );
    client
        .call(
            "hello",
            json!({"version": PROTOCOL_VERSION, "client": "test"}),
        )
        .await
        .unwrap();
    let status = client.call("status", Value::Null).await.unwrap();
    assert_eq!(status["mode"], "idle");
    assert_eq!(status["owner"], Value::Null);

    // An old UI is told why, then disconnected.
    let mut old = world.connect(ALICE).unwrap();
    let error = old
        .call(
            "hello",
            json!({"version": PROTOCOL_VERSION + 1, "client": "old"}),
        )
        .await;
    assert_eq!(code(error), "version_mismatch");
    assert!(old.line().await.is_none());
}

#[tokio::test]
async fn players_see_the_games_and_change_only_their_own_settings() {
    let world = world();
    let mut ui = world.greeted(ALICE).await;

    let games = ui.call("games", Value::Null).await.unwrap();
    let games = games.as_array().unwrap();
    assert!(!games.is_empty());
    for game in games {
        // Built-in games still being verified are never routed (rule 7).
        let routable = game["routable"].as_bool().unwrap();
        let status = game["status"].as_str().unwrap();
        assert_eq!(
            routable,
            matches!(status, "supported" | "testing"),
            "{game}"
        );
        assert_eq!(game["test_definition"], false);
    }

    let settings = ui
        .call(
            "set_settings",
            json!({"verbose_logging": true, "min_improvement_ms": 15.0}),
        )
        .await
        .unwrap();
    assert_eq!(settings["verbose_logging"], true);
    assert_eq!(settings["min_improvement_ms"], 15.0);
    let saved = ClientConfig::load(&world.paths.config()).unwrap();
    assert!(saved.verbose_logging);
    assert_eq!(saved.min_improvement_ms, 15.0);

    // The control plane and game definitions are an administrator's.
    let error = ui
        .call(
            "set_settings",
            json!({"control_url": "https://evil.example"}),
        )
        .await;
    assert_eq!(code(error), "bad_request");
    let error = ui
        .call("set_settings", json!({"min_improvement_pct": 500.0}))
        .await;
    assert_eq!(code(error), "invalid_settings");
    assert_eq!(
        ClientConfig::load(&world.paths.config())
            .unwrap()
            .min_improvement_pct,
        10.0
    );

    let logs = ui.call("log_paths", Value::Null).await.unwrap();
    assert_eq!(
        logs["service_logs"],
        world.paths.logs().display().to_string()
    );
}

#[tokio::test]
async fn games_that_are_never_routed_are_refused() {
    let world = world();
    let mut ui = world.greeted(ALICE).await;
    let games = ui.call("games", Value::Null).await.unwrap();
    let refused = games
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["routable"] == false)
        .expect("a built-in game that is never routed");
    let region = refused["regions"][0]["id"].clone();
    let error = ui
        .call("measure", json!({"game": refused["id"], "region": region}))
        .await;
    assert_eq!(code(error), "refused");
}

#[tokio::test]
async fn one_user_cannot_touch_another_users_session() {
    let world = world();
    let mut alice = world.greeted(ALICE).await;
    let mut bob = world.greeted(BOB).await;

    // Alice tries to turn Optimize on (nothing is measured, so it fails);
    // the attempt and its notices are hers.
    let error = alice.call("enable", json!({})).await;
    assert_eq!(code(error), "not_measured");
    let status = alice.call("status", Value::Null).await.unwrap();
    assert_eq!(status["owner"], "you");
    let status = bob.call("status", Value::Null).await.unwrap();
    assert_eq!(status["owner"], "another_user");
    assert_eq!(
        code(bob.call("dismiss", json!({"id": 1})).await),
        "owned_by_another_user"
    );
    alice.call("dismiss", json!({"id": 1})).await.unwrap();

    // With no session running, Bob may still measure and turn it on.
    let error = bob.call("enable", json!({})).await;
    assert_eq!(code(error), "not_measured");
    let status = bob.call("status", Value::Null).await.unwrap();
    assert_eq!(status["owner"], "you");

    // Once Bob signs out, nobody owns it.
    world.service.signed_out(BOB).await;
    let status = alice.call("status", Value::Null).await.unwrap();
    assert_eq!(status["owner"], Value::Null);
}

#[tokio::test]
async fn connections_and_requests_are_limited() {
    let limits = Limits {
        connections: 3,
        per_user: 2,
        burst: 5.0,
        rate: 1.0,
        ..Limits::default()
    };
    let world = world_with(limits, ClientConfig::default());
    let mut first = world.greeted(ALICE).await;
    let _second = world.connect(ALICE).unwrap();
    assert_eq!(world.connect(ALICE).err().unwrap(), "too_many_connections");
    let _bob = world.connect(BOB).unwrap();
    assert_eq!(world.connect(Caller::Uid(7)).err().unwrap(), "full");

    // The burst is spent (hello took one); then requests are refused.
    let mut codes = Vec::new();
    for _ in 0..6 {
        codes.push(match first.call("status", Value::Null).await {
            Ok(_) => "ok".to_string(),
            Err(error) => error["code"].as_str().unwrap().to_string(),
        });
    }
    assert_eq!(codes[..4], ["ok", "ok", "ok", "ok"]);
    assert!(codes[4..].iter().all(|c| c == "rate_limited"), "{codes:?}");

    // Closing a connection frees its slot.
    drop(first);
    tokio::time::sleep(Duration::from_millis(100)).await;
    world.connect(ALICE).unwrap();
}

#[tokio::test]
async fn an_oversized_request_closes_the_connection() {
    let world = world();
    let mut ui = world.greeted(ALICE).await;
    let long = format!(
        r#"{{"id":9,"method":"status","pad":"{}"}}"#,
        "x".repeat(MAX_LINE)
    );
    ui.send_line(&long).await;
    let error = ui.line().await.unwrap();
    assert_eq!(error["error"]["code"], "bad_request");
    assert!(ui.line().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn a_connection_that_never_says_hello_is_closed() {
    let world = world();
    let mut silent = world.connect(ALICE).unwrap();
    // Paused time jumps to the service's hello deadline.
    let started = tokio::time::Instant::now();
    assert!(silent.line().await.is_none());
    assert!(started.elapsed() >= Limits::default().hello_timeout);
}

#[cfg(unix)]
#[tokio::test]
async fn the_ui_connects_over_the_unix_socket() {
    use tokio::net::UnixStream;

    let world = world();
    let socket = world.paths.data.join("service.sock");
    let listener = tokio::spawn(pingpath_service::unix::listen(
        socket.clone(),
        world.service.clone(),
        world.stop.clone(),
    ));
    let mut stream = loop {
        match UnixStream::connect(&socket).await {
            Ok(stream) => break stream,
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    };
    let hello = json!({"id": 1, "method": "hello", "params": {"version": PROTOCOL_VERSION, "client": "test"}});
    stream
        .write_all(format!("{hello}\n").as_bytes())
        .await
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let welcome: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(welcome["result"]["version"], PROTOCOL_VERSION);
    listener.abort();
}
