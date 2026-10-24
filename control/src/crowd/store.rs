//! The crowd store (docs/decisions/012, "Storage"): one SQLite file, written
//! by one thread that takes rows from a bounded queue, so a slow disk turns
//! reports away (`crowd_busy`) rather than holding them, and never touches
//! sessions.
//!
//! What a row holds: the person's keyed hash, the network's keyed tag, the
//! hour it arrived (the day, after 24 hours), the ISP, country and city the
//! address files gave, and the record's game, region, relays and figures.
//! Never an address or a prefix. Deletes overwrite (`secure_delete`), and
//! the write-ahead log is checkpointed after them, so deleted rows don't
//! linger in free pages.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use pingpath_crowd::{Method, Source};
use rusqlite::{Connection, params};
use tokio::sync::oneshot;

use super::geo::Place;
use super::key::{ContributorHash, NetworkTag};

/// How often old rows are deleted and day-old hours rounded down.
const RETAIN_EVERY: Duration = Duration::from_secs(3600);
/// Hours are kept this long, for the status page's last day.
const HOURS_KEPT_SECS: u64 = 86400;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS records (
    contributor BLOB NOT NULL,
    network BLOB NOT NULL,
    received INTEGER NOT NULL,
    source TEXT NOT NULL,
    kind TEXT NOT NULL,
    asn INTEGER,
    isp TEXT,
    country TEXT,
    city TEXT,
    game TEXT NOT NULL,
    region TEXT NOT NULL,
    direct_ms REAL,
    ping_ms REAL,
    loss_pct REAL NOT NULL,
    jitter_ms REAL NOT NULL,
    entry INTEGER,
    exit INTEGER,
    method TEXT
);
CREATE INDEX IF NOT EXISTS records_contributor ON records (contributor);
CREATE INDEX IF NOT EXISTS records_received ON records (received);
CREATE INDEX IF NOT EXISTS records_game ON records (game, region);
CREATE TABLE IF NOT EXISTS contributor_days (
    contributor BLOB NOT NULL,
    day INTEGER NOT NULL,
    PRIMARY KEY (contributor, day)
) WITHOUT ROWID;
";

/// What a stored row measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Direct figures, from a measurement or the probe.
    Direct,
    /// An optimized session.
    Session,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Session => "session",
        }
    }
}

/// One row, from one record (a measurement or probe record makes one per
/// region).
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub kind: Kind,
    pub game: String,
    pub region: String,
    pub direct_ms: Option<f64>,
    pub ping_ms: Option<f64>,
    pub loss_pct: f64,
    pub jitter_ms: f64,
    pub entry: Option<u32>,
    pub exit: Option<u32>,
    pub method: Option<Method>,
}

/// One report's rows, as queued.
#[derive(Debug)]
pub struct Insert {
    pub contributor: ContributorHash,
    pub network: NetworkTag,
    /// The hour it arrived (Unix seconds, rounded down).
    pub received: u64,
    pub source: Source,
    pub place: Place,
    pub rows: Vec<Row>,
}

enum Job {
    Insert(Insert),
    Forget(ContributorHash, oneshot::Sender<Result<u64, String>>),
    Stop,
}

/// Counted by the writer, for `/metrics` and the log.
#[derive(Debug, Default)]
pub struct StoreStats {
    pub rows_written: AtomicU64,
    pub rows_expired: AtomicU64,
    pub rows_forgotten: AtomicU64,
    pub errors: AtomicU64,
}

/// What the store held when it opened, for the limits.
#[derive(Debug, Default)]
pub struct Opened {
    /// Everyone with records.
    pub known: HashSet<ContributorHash>,
    /// Rows that arrived today, per ASN (0 for unknown), and in all.
    pub rows_today_by_asn: HashMap<u32, u64>,
    pub rows_today: u64,
}

/// The queue was full: the report is turned away.
#[derive(Debug)]
pub struct Full;

pub struct Store {
    jobs: SyncSender<Job>,
    writer: Option<JoinHandle<()>>,
    pub stats: Arc<StoreStats>,
    pub path: PathBuf,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn connect(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "secure_delete", "ON")?;
    connection.busy_timeout(Duration::from_secs(5))?;
    Ok(connection)
}

impl Store {
    /// Opens (or makes) the database at `path` and starts its writer.
    pub fn open(path: &Path, retention_days: u32, queue: usize) -> Result<(Self, Opened), String> {
        let fail = |e: rusqlite::Error| format!("crowd database {}: {e}", path.display());
        let connection = connect(path).map_err(fail)?;
        connection.execute_batch(SCHEMA).map_err(fail)?;
        let opened = read_opened(&connection, now_unix()).map_err(fail)?;
        let stats = Arc::new(StoreStats::default());
        let (jobs, queued) = mpsc::sync_channel(queue);
        let writer = {
            let stats = stats.clone();
            let retention = u64::from(retention_days) * 86400;
            std::thread::Builder::new()
                .name("crowd-store".into())
                .spawn(move || write_loop(connection, queued, stats, retention))
                .map_err(|e| format!("cannot start the crowd store: {e}"))?
        };
        Ok((
            Self {
                jobs,
                writer: Some(writer),
                stats,
                path: path.to_path_buf(),
            },
            opened,
        ))
    }

    /// Queues a report's rows; a full queue turns them away at once.
    pub fn insert(&self, insert: Insert) -> Result<(), Full> {
        match self.jobs.try_send(Job::Insert(insert)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => Err(Full),
        }
    }

    /// Deletes everything one person shared; the number of rows.
    pub async fn forget(&self, contributor: ContributorHash) -> Result<u64, String> {
        let (reply, answer) = oneshot::channel();
        self.jobs
            .try_send(Job::Forget(contributor, reply))
            .map_err(|_| "the crowd store is busy".to_string())?;
        answer
            .await
            .map_err(|_| "the crowd store stopped".to_string())?
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // Blocks until the queue has room: everything queued is written.
        let _ = self.jobs.send(Job::Stop);
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

fn read_opened(connection: &Connection, now: u64) -> rusqlite::Result<Opened> {
    let mut opened = Opened::default();
    let mut known = connection.prepare("SELECT DISTINCT contributor FROM contributor_days")?;
    for hash in known.query_map([], |row| row.get::<_, Vec<u8>>(0))? {
        if let Ok(hash) = <ContributorHash>::try_from(hash?.as_slice()) {
            opened.known.insert(hash);
        }
    }
    let today = now - now % 86400;
    let mut rows = connection.prepare(
        "SELECT COALESCE(asn, 0), COUNT(*) FROM records WHERE received >= ?1 GROUP BY 1",
    )?;
    for counted in rows.query_map([today as i64], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })? {
        let (asn, count) = counted?;
        let count = count.max(0) as u64;
        opened
            .rows_today_by_asn
            .insert(u32::try_from(asn).unwrap_or(0), count);
        opened.rows_today += count;
    }
    Ok(opened)
}

fn write_loop(
    connection: Connection,
    jobs: mpsc::Receiver<Job>,
    stats: Arc<StoreStats>,
    retention_secs: u64,
) {
    let mut connection = connection;
    let mut next_retain = Instant::now();
    loop {
        let wait = next_retain.saturating_duration_since(Instant::now());
        match jobs.recv_timeout(wait) {
            Ok(Job::Insert(insert)) => match write(&mut connection, &insert) {
                Ok(()) => {
                    stats
                        .rows_written
                        .fetch_add(insert.rows.len() as u64, Ordering::Relaxed);
                }
                Err(e) => {
                    stats.errors.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(error = %e, rows = insert.rows.len(), "crowd rows not written");
                }
            },
            Ok(Job::Forget(contributor, reply)) => {
                let result = forget(&mut connection, &contributor).map_err(|e| e.to_string());
                match &result {
                    Ok(rows) => {
                        stats.rows_forgotten.fetch_add(*rows, Ordering::Relaxed);
                        tracing::info!(rows, "a contributor's records deleted");
                    }
                    Err(e) => {
                        stats.errors.fetch_add(1, Ordering::Relaxed);
                        tracing::error!(error = %e, "a contributor's records not deleted");
                    }
                }
                let _ = reply.send(result);
            }
            Ok(Job::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                match retain(&mut connection, now_unix(), retention_secs) {
                    Ok(expired) => {
                        stats.rows_expired.fetch_add(expired, Ordering::Relaxed);
                        if expired > 0 {
                            tracing::info!(rows = expired, "expired crowd records deleted");
                        }
                    }
                    Err(e) => {
                        stats.errors.fetch_add(1, Ordering::Relaxed);
                        tracing::error!(error = %e, "expired crowd records not deleted");
                    }
                }
                next_retain = Instant::now() + RETAIN_EVERY;
            }
        }
    }
    if let Err(e) = connection.pragma_update(None, "wal_checkpoint", "TRUNCATE") {
        tracing::warn!(error = %e, "crowd database not checkpointed at stop");
    }
}

fn write(connection: &mut Connection, insert: &Insert) -> rusqlite::Result<()> {
    let tx = connection.transaction()?;
    {
        let mut statement = tx.prepare_cached(
            "INSERT INTO records (contributor, network, received, source, kind, asn, isp,
                country, city, game, region, direct_ms, ping_ms, loss_pct, jitter_ms, entry,
                exit, method)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                ?17, ?18)",
        )?;
        let place = &insert.place;
        for row in &insert.rows {
            statement.execute(params![
                &insert.contributor[..],
                &insert.network[..],
                insert.received as i64,
                insert.source.as_str(),
                row.kind.as_str(),
                place.asn,
                place.isp,
                place.country,
                place.city,
                row.game,
                row.region,
                row.direct_ms,
                row.ping_ms,
                row.loss_pct,
                row.jitter_ms,
                row.entry,
                row.exit,
                row.method.map(Method::as_str),
            ])?;
        }
        tx.execute(
            "INSERT OR IGNORE INTO contributor_days (contributor, day) VALUES (?1, ?2)",
            params![
                &insert.contributor[..],
                (insert.received - insert.received % 86400) as i64
            ],
        )?;
    }
    tx.commit()
}

fn forget(connection: &mut Connection, contributor: &ContributorHash) -> rusqlite::Result<u64> {
    let tx = connection.transaction()?;
    let rows = tx.execute(
        "DELETE FROM records WHERE contributor = ?1",
        [&contributor[..]],
    )?;
    tx.execute(
        "DELETE FROM contributor_days WHERE contributor = ?1",
        [&contributor[..]],
    )?;
    tx.commit()?;
    connection.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
    Ok(rows as u64)
}

/// Deletes rows past retention, rounds hours over a day old down to the
/// day, and checkpoints; the number of rows deleted.
fn retain(connection: &mut Connection, now: u64, retention_secs: u64) -> rusqlite::Result<u64> {
    let cutoff = now.saturating_sub(retention_secs) as i64;
    let day_old = now.saturating_sub(HOURS_KEPT_SECS) as i64;
    let tx = connection.transaction()?;
    let expired = tx.execute("DELETE FROM records WHERE received < ?1", [cutoff])?;
    tx.execute("DELETE FROM contributor_days WHERE day < ?1", [cutoff])?;
    tx.execute(
        "UPDATE records SET received = received - received % 86400
         WHERE received < ?1 AND received % 86400 != 0",
        [day_old],
    )?;
    tx.commit()?;
    if expired > 0 {
        connection.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
    }
    Ok(expired as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(region: &str) -> Row {
        Row {
            kind: Kind::Direct,
            game: "cs2".into(),
            region: region.into(),
            direct_ms: Some(80.0),
            ping_ms: None,
            loss_pct: 0.0,
            jitter_ms: 1.0,
            entry: None,
            exit: None,
            method: None,
        }
    }

    fn insert(contributor: u8, received: u64, rows: Vec<Row>) -> Insert {
        Insert {
            contributor: [contributor; 16],
            network: [9; 8],
            received,
            source: Source::App,
            place: Place {
                asn: Some(64500),
                isp: Some("Example Telecom".into()),
                country: Some("SA".into()),
                city: Some("Riyadh".into()),
            },
            rows,
        }
    }

    fn count(path: &Path, sql: &str) -> i64 {
        Connection::open(path)
            .unwrap()
            .query_row(sql, [], |r| r.get(0))
            .unwrap()
    }

    #[tokio::test]
    async fn rows_are_written_forgotten_and_counted_when_reopened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crowd.sqlite");
        let now = now_unix();
        let hour = now - now % 3600;
        {
            let (store, opened) = Store::open(&path, 30, 16).unwrap();
            assert!(opened.known.is_empty());
            store
                .insert(insert(1, hour, vec![row("eu"), row("me")]))
                .unwrap();
            store.insert(insert(2, hour, vec![row("eu")])).unwrap();
            assert_eq!(store.forget([2; 16]).await.unwrap(), 1);
            assert_eq!(store.forget([3; 16]).await.unwrap(), 0);
        }
        assert_eq!(count(&path, "SELECT COUNT(*) FROM records"), 2);
        assert_eq!(count(&path, "SELECT COUNT(*) FROM contributor_days"), 1);
        let (_store, opened) = Store::open(&path, 30, 16).unwrap();
        assert_eq!(opened.known, HashSet::from([[1u8; 16]]));
        assert_eq!(opened.rows_today, 2);
        assert_eq!(opened.rows_today_by_asn.get(&64500), Some(&2));
    }

    #[test]
    fn old_rows_expire_and_day_old_hours_become_days() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crowd.sqlite");
        let mut connection = connect(&path).unwrap();
        connection.execute_batch(SCHEMA).unwrap();
        let day = 86400;
        let now = 100 * day + 5 * 3600;
        write(&mut connection, &insert(1, now - 31 * day, vec![row("eu")])).unwrap();
        write(
            &mut connection,
            &insert(1, now - 2 * day + 7200, vec![row("eu")]),
        )
        .unwrap();
        write(&mut connection, &insert(1, now - 3600, vec![row("eu")])).unwrap();
        assert_eq!(retain(&mut connection, now, 30 * day).unwrap(), 1);
        let times: Vec<i64> = connection
            .prepare("SELECT received FROM records ORDER BY received")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            times,
            [
                (now - 2 * day) as i64 - (now % day) as i64,
                (now - 3600) as i64
            ]
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM contributor_days", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn a_full_queue_turns_reports_away() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crowd.sqlite");
        let (store, _) = Store::open(&path, 30, 1).unwrap();
        // Another connection holds the database, so the writer stalls on
        // its first job and the queue of one fills.
        let blocker = Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let turned_away = (0..50).any(|_| store.insert(insert(1, 0, vec![row("eu")])).is_err());
        blocker.execute_batch("COMMIT").unwrap();
        assert!(turned_away, "a queue of one held 50 reports");
    }
}
