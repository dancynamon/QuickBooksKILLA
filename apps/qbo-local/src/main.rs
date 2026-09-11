//! `qbo-local` — the subcommand binary. `HANDOFF.md` §2.5, `ROADMAP.md` §A.
//!
//! Wires the pieces built so far — [`Store`], [`Daemon`], [`Reconciler`] — into
//! something runnable against a file database, ahead of `HttpQboClient`
//! (`HANDOFF.md` §2.3) landing. Every subcommand that would need Intuit
//! credentials accepts `--mock` instead, backed by [`MockQbo`], so the loop is
//! exercisable end to end today; without it, each refuses with the same exact
//! message rather than pretending to reach a client that does not exist yet.
//!
//! No `clap`: `std::env::args` and a hand-rolled [`parse`], matching the rest
//! of this crate's no-new-dependencies discipline.

use std::env;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use qbo_local::client::MockQbo;
use qbo_local::clock::{Clock, SystemClock};
use qbo_local::daemon::{Cadence, Daemon, DaemonOptions, Tick};
use qbo_local::domain::{EntityType, RealmId, SyncTier};
use qbo_local::driver::{SyncOptions, SyncPath, SyncReport};
use qbo_local::ratelimit::RealmLimits;
use qbo_local::reconcile::{ReconcileOptions, ReconcileReport, Reconciler};
use qbo_local::store::backup::SnapshotPolicy;
use qbo_local::store::{latest_version, ProjectedTable, RealmSummary, Store};
use qbo_local::sync::{CDC_LOOKBACK_DAYS, DEFAULT_CDC_MAX_AGE_DAYS};

const PROJECTED_TABLES: &[ProjectedTable] = &[
    ProjectedTable::Contacts,
    ProjectedTable::Items,
    ProjectedTable::Accounts,
    ProjectedTable::Classes,
    ProjectedTable::Documents,
    ProjectedTable::DocumentLines,
    ProjectedTable::DocumentLinks,
];

/// A snapshot rotation depth used when a caller gives `--snapshot-dir` /
/// `--dir` without an explicit `--keep`. Not specified anywhere as a fixed
/// value (`DESIGN.md` §8 says only "N-deep"); a week of nightly snapshots is a
/// reasonable default that costs little disk and covers "someone doesn't
/// notice for a few days".
const DEFAULT_SNAPSHOT_KEEP: usize = 7;

/// `HANDOFF.md` §2.3: the HTTP transport is not built yet. Every subcommand
/// that would otherwise need it prints exactly this and exits 2 unless told
/// to run against [`MockQbo`] instead.
const HTTP_CLIENT_MISSING: &str =
    "HttpQboClient is not built yet (HANDOFF.md §2.3); run with --mock to exercise the loop";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let command = match parse(&args) {
        Ok(command) => command,
        Err(usage_error) => {
            eprint!("{usage_error}");
            return ExitCode::from(2);
        }
    };

    match execute(command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(CommandError::MissingHttpClient) => {
            eprintln!("{HTTP_CLIENT_MISSING}");
            ExitCode::from(2)
        }
        Err(CommandError::Runtime(message)) => {
            eprintln!("{message}");
            ExitCode::from(1)
        }
    }
}

// ---------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Command {
    Status {
        db: Option<PathBuf>,
    },
    Init {
        db: PathBuf,
        realm: RealmId,
        name: String,
    },
    Daemon {
        db: PathBuf,
        realm: RealmId,
        focused_secs: Option<u64>,
        idle_secs: Option<u64>,
        snapshot_dir: Option<PathBuf>,
        keep: Option<usize>,
        snapshot_hour_utc: Option<u32>,
        once: bool,
        mock: bool,
    },
    Sweep {
        db: PathBuf,
        realm: RealmId,
        mock: bool,
    },
    Snapshot {
        db: PathBuf,
        dir: PathBuf,
        keep: Option<usize>,
    },
}

const USAGE: &str = "\
qbo-local — local SQLite replica of QuickBooks Online

USAGE:
    qbo-local status [--db PATH]
    qbo-local init --db PATH --realm ID --name \"Display Name\"
    qbo-local daemon --db PATH --realm ID [--focused-secs N] [--idle-secs N]
                      [--snapshot-dir DIR --keep N] [--snapshot-hour-utc H]
                      [--once] [--mock]
    qbo-local sweep --db PATH --realm ID [--mock]
    qbo-local snapshot --db PATH --dir DIR [--keep N]
    qbo-local --help

SUBCOMMANDS:
    status      Print the M0 foundations, and sync status per realm with --db.
    init        Open (creating if absent) a file store and register a realm.
                Idempotent — registering an existing realm is not an error.
    daemon      Run the CDC poll loop against a realm until stopped.
    sweep       Run one reconciliation sweep against QBO's index.
    snapshot    Take one nightly-style backup of the replica, with rotation.

FLAGS:
    --db PATH               Path to the SQLite replica file.
    --realm ID              A QBO realm id (digits only).
    --name \"Display Name\"   The realm's display name (init only).
    --focused-secs N        Focused-cadence poll interval, in seconds (daemon).
    --idle-secs N           Idle-cadence poll interval, in seconds (daemon).
    --snapshot-dir DIR      Where nightly snapshots go (daemon; pair with --keep).
    --keep N                Snapshots to retain (daemon, snapshot).
    --snapshot-hour-utc H   UTC hour at or after which a snapshot may fire (daemon).
    --dir DIR               Where a snapshot is written (snapshot).
    --once                  Run a single tick and exit (daemon).
    --mock                  Use an empty in-memory MockQbo instead of live QBO.

Without --mock, daemon and sweep exit 2: HttpQboClient is not built yet.
Ctrl-C is safe to stop `daemon` with — every write is transactional — or type
\"stop\" and press Enter on its stdin for a clean exit.

EXIT CODES:
    0   ok
    1   runtime error
    2   usage error (this message), or --mock not given (daemon, sweep)
";

#[derive(Clone, Debug, PartialEq, Eq)]
struct UsageError {
    reason: Option<String>,
}

impl UsageError {
    fn usage() -> Self {
        UsageError { reason: None }
    }

    fn new(reason: impl Into<String>) -> Self {
        UsageError {
            reason: Some(reason.into()),
        }
    }

    fn missing_flag(flag: &str) -> Self {
        Self::new(format!("missing required flag {flag}"))
    }

    fn missing_value(flag: &str) -> Self {
        Self::new(format!("{flag} requires a value"))
    }

    fn unknown_flag(flag: &str) -> Self {
        Self::new(format!("unknown flag {flag}"))
    }

    fn unknown_subcommand(name: &str) -> Self {
        Self::new(format!("unknown subcommand {name:?}"))
    }

    fn invalid_value(flag: &str, value: &str) -> Self {
        Self::new(format!("invalid value for {flag}: {value:?}"))
    }
}

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(reason) = &self.reason {
            writeln!(f, "error: {reason}")?;
            writeln!(f)?;
        }
        write!(f, "{USAGE}")
    }
}

/// Parse `args` — the process arguments with the program name already
/// stripped — into a [`Command`].
fn parse(args: &[String]) -> Result<Command, UsageError> {
    let mut args = args.iter();
    let Some(subcommand) = args.next() else {
        return Err(UsageError::usage());
    };

    match subcommand.as_str() {
        "--help" | "-h" => Err(UsageError::usage()),
        "status" => parse_status(args),
        "init" => parse_init(args),
        "daemon" => parse_daemon(args),
        "sweep" => parse_sweep(args),
        "snapshot" => parse_snapshot(args),
        other => Err(UsageError::unknown_subcommand(other)),
    }
}

type Args<'a> = std::slice::Iter<'a, String>;

fn next_value(args: &mut Args<'_>, flag: &str) -> Result<String, UsageError> {
    args.next()
        .cloned()
        .ok_or_else(|| UsageError::missing_value(flag))
}

fn parse_realm(raw: &str) -> Result<RealmId, UsageError> {
    RealmId::parse(raw).map_err(|_| UsageError::invalid_value("--realm", raw))
}

fn parse_u32(raw: &str, flag: &str) -> Result<u32, UsageError> {
    raw.parse()
        .map_err(|_| UsageError::invalid_value(flag, raw))
}

fn parse_u64(raw: &str, flag: &str) -> Result<u64, UsageError> {
    raw.parse()
        .map_err(|_| UsageError::invalid_value(flag, raw))
}

fn parse_usize(raw: &str, flag: &str) -> Result<usize, UsageError> {
    raw.parse()
        .map_err(|_| UsageError::invalid_value(flag, raw))
}

fn parse_status(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(Command::Status { db })
}

fn parse_init(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut realm = None;
    let mut name = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--realm" => realm = Some(parse_realm(&next_value(&mut args, "--realm")?)?),
            "--name" => name = Some(next_value(&mut args, "--name")?),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(Command::Init {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        realm: realm.ok_or_else(|| UsageError::missing_flag("--realm"))?,
        name: name.ok_or_else(|| UsageError::missing_flag("--name"))?,
    })
}

fn parse_daemon(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut realm = None;
    let mut focused_secs = None;
    let mut idle_secs = None;
    let mut snapshot_dir = None;
    let mut keep = None;
    let mut snapshot_hour_utc = None;
    let mut once = false;
    let mut mock = false;

    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--realm" => realm = Some(parse_realm(&next_value(&mut args, "--realm")?)?),
            "--focused-secs" => {
                focused_secs = Some(parse_u64(
                    &next_value(&mut args, "--focused-secs")?,
                    "--focused-secs",
                )?)
            }
            "--idle-secs" => {
                idle_secs = Some(parse_u64(
                    &next_value(&mut args, "--idle-secs")?,
                    "--idle-secs",
                )?)
            }
            "--snapshot-dir" => {
                snapshot_dir = Some(PathBuf::from(next_value(&mut args, "--snapshot-dir")?))
            }
            "--keep" => keep = Some(parse_usize(&next_value(&mut args, "--keep")?, "--keep")?),
            "--snapshot-hour-utc" => {
                snapshot_hour_utc = Some(parse_u32(
                    &next_value(&mut args, "--snapshot-hour-utc")?,
                    "--snapshot-hour-utc",
                )?)
            }
            "--once" => once = true,
            "--mock" => mock = true,
            other => return Err(UsageError::unknown_flag(other)),
        }
    }

    if snapshot_dir.is_some() != keep.is_some() {
        return Err(UsageError::new(
            "--snapshot-dir and --keep must be given together",
        ));
    }

    Ok(Command::Daemon {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        realm: realm.ok_or_else(|| UsageError::missing_flag("--realm"))?,
        focused_secs,
        idle_secs,
        snapshot_dir,
        keep,
        snapshot_hour_utc,
        once,
        mock,
    })
}

fn parse_sweep(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut realm = None;
    let mut mock = false;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--realm" => realm = Some(parse_realm(&next_value(&mut args, "--realm")?)?),
            "--mock" => mock = true,
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(Command::Sweep {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        realm: realm.ok_or_else(|| UsageError::missing_flag("--realm"))?,
        mock,
    })
}

fn parse_snapshot(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut dir = None;
    let mut keep = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--dir" => dir = Some(PathBuf::from(next_value(&mut args, "--dir")?)),
            "--keep" => keep = Some(parse_usize(&next_value(&mut args, "--keep")?, "--keep")?),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(Command::Snapshot {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        dir: dir.ok_or_else(|| UsageError::missing_flag("--dir"))?,
        keep,
    })
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

enum CommandError {
    /// `--mock` was not given to a subcommand that would otherwise need
    /// `HttpQboClient`.
    MissingHttpClient,
    Runtime(String),
}

fn runtime<E: std::fmt::Display>(error: E) -> CommandError {
    CommandError::Runtime(error.to_string())
}

fn execute(command: Command) -> Result<(), CommandError> {
    match command {
        Command::Status { db } => run_status(db),
        Command::Init { db, realm, name } => run_init(&db, &realm, &name),
        Command::Daemon {
            db,
            realm,
            focused_secs,
            idle_secs,
            snapshot_dir,
            keep,
            snapshot_hour_utc,
            once,
            mock,
        } => run_daemon(
            &db,
            &realm,
            focused_secs,
            idle_secs,
            snapshot_dir,
            keep,
            snapshot_hour_utc,
            once,
            mock,
        ),
        Command::Sweep { db, realm, mock } => run_sweep(&db, &realm, mock),
        Command::Snapshot { db, dir, keep } => run_snapshot(&db, &dir, keep),
    }
}

// -- status -------------------------------------------------------------

fn run_status(db: Option<PathBuf>) -> Result<(), CommandError> {
    println!("qbo-local — M0 foundations");
    println!();

    match db {
        None => {
            // Nothing is mirrored yet; register a throwaway realm so the
            // projection counts below have something to be zero for, rather
            // than being printed as an unexplained blank.
            let store = Store::open_in_memory().map_err(runtime)?;
            let realm = RealmId::parse("0000000000000000").map_err(runtime)?;
            store
                .register_realm(&realm, "empty replica", Utc::now())
                .map_err(runtime)?;

            print_foundations(store.schema_version().map_err(runtime)?);

            let projected: i64 = PROJECTED_TABLES
                .iter()
                .map(|table| store.count_projected(&realm, *table).unwrap_or(0))
                .sum();
            println!("  projection (§3.2)");
            println!("    tables       : {}", PROJECTED_TABLES.len());
            println!("    rows         : {projected}");
            println!("    rebuilt from : entities.raw_json, no Intuit round-trip");
            println!();
            println!("  replica: in-memory (no --db given)");
        }
        Some(path) => {
            let store = Store::open(&path).map_err(runtime)?;
            print_foundations(store.schema_version().map_err(runtime)?);

            let realms = store.list_realms().map_err(runtime)?;
            println!("  replica           : {}", path.display());
            println!("  realms registered : {}", realms.len());
            println!();

            if realms.is_empty() {
                println!(
                    "  (none — run `qbo-local init --db {} --realm ID --name \"Display Name\"`)",
                    path.display()
                );
                println!();
            }
            for realm in &realms {
                print_realm_sync_status(&store, realm)?;
            }
        }
    }

    println!("  writes: disabled for every realm until explicitly enabled");
    println!("  generated at {}", Utc::now().to_rfc3339());
    Ok(())
}

fn print_foundations(schema_version: i64) {
    println!("  replica schema version : {schema_version}");
    println!("  latest migration       : {}", latest_version());
    println!();

    let masters = EntityType::ALL
        .iter()
        .filter(|e| e.tier() == SyncTier::Masters)
        .count();
    let documents = EntityType::ALL
        .iter()
        .filter(|e| e.tier() == SyncTier::Documents)
        .count();
    let peripheral = EntityType::ALL
        .iter()
        .filter(|e| e.tier() == SyncTier::Peripheral)
        .count();

    println!("  entity types           : {} total", EntityType::ALL.len());
    println!("    masters (M0)         : {masters}");
    println!("    documents (M0)       : {documents}");
    println!("    peripheral (later)   : {peripheral}");
    println!();

    let limits = RealmLimits::default();
    println!("  rate budgets per realm");
    println!(
        "    general              : {}/min",
        limits.general.refill_per_minute
    );
    println!(
        "    batch                : {}/min",
        limits.batch.refill_per_minute
    );
    println!(
        "    reports              : {}/min",
        limits.reports.refill_per_minute
    );
    println!("    max concurrent       : {}", limits.max_concurrent);
    println!();

    println!("  cdc lookback (documented) : {CDC_LOOKBACK_DAYS} days");
    println!("  cdc cursor max age (ours) : {DEFAULT_CDC_MAX_AGE_DAYS} days");
    println!();
}

/// [`qbo_local::store::query::SyncStatus`] for one realm, as a small table —
/// `DESIGN.md` §6.6's "last successful sync per realm, always visible", on a
/// terminal instead of in the chrome.
fn print_realm_sync_status(store: &Store, realm: &RealmSummary) -> Result<(), CommandError> {
    let status = store.sync_status(&realm.realm_id).map_err(runtime)?;

    println!("  realm {} — {}", realm.realm_id, realm.display_name);
    println!("    write enabled     : {}", status.write_enabled);
    println!("    quarantined total : {}", status.quarantined_total);
    println!(
        "    {:<14} {:>9} {:>26} {:>26}",
        "entity", "mirrored", "last cdc cursor", "last full sweep"
    );
    for entity in &status.entities {
        println!(
            "    {:<14} {:>9} {:>26} {:>26}",
            entity.entity_type.as_str(),
            entity.mirrored,
            entity
                .last_cdc_cursor
                .map(|at| at.to_rfc3339())
                .unwrap_or_else(|| "-".into()),
            entity
                .last_full_sweep
                .map(|at| at.to_rfc3339())
                .unwrap_or_else(|| "-".into()),
        );
    }
    println!();
    Ok(())
}

// -- init -----------------------------------------------------------------

fn run_init(db: &Path, realm: &RealmId, name: &str) -> Result<(), CommandError> {
    let store = Store::open(db).map_err(runtime)?;
    // `register_realm` is `ON CONFLICT ... DO UPDATE`, so registering an
    // existing realm is not an error — it just refreshes the display name.
    store
        .register_realm(realm, name, Utc::now())
        .map_err(runtime)?;
    println!("registered realm {realm} ({name:?}) in {}", db.display());
    Ok(())
}

// -- sweep ------------------------------------------------------------------

fn run_sweep(db: &Path, realm: &RealmId, mock: bool) -> Result<(), CommandError> {
    if !mock {
        return Err(CommandError::MissingHttpClient);
    }

    let store = Store::open(db).map_err(runtime)?;
    let now = Utc::now();
    let client = MockQbo::new(now);
    let mut reconciler = Reconciler::new(client, ReconcileOptions::default(), now);
    let report = reconciler
        .sweep_realm(&store, realm, EntityType::m0_scope(), now)
        .map_err(runtime)?;

    print_reconcile_report(&report);
    Ok(())
}

fn print_reconcile_report(report: &ReconcileReport) {
    println!(
        "{:<14} {:>8} {:>8} {:>9} {:>7} {:>7} {:>8} {:>12} {:>9}",
        "entity",
        "remote",
        "local",
        "missing",
        "stale",
        "extra",
        "healed",
        "quarantined",
        "requests"
    );
    for entity in &report.entities {
        println!(
            "{:<14} {:>8} {:>8} {:>9} {:>7} {:>7} {:>8} {:>12} {:>9}",
            entity.entity_type.as_str(),
            entity.checked_remote,
            entity.checked_local,
            entity.missing.len(),
            entity.stale.len(),
            entity.extra.len(),
            entity.healed,
            entity.quarantined,
            entity.requests,
        );
    }
    println!();
    println!("orphaned document lines : {}", report.orphaned_lines.len());
    println!("clean                   : {}", report.is_clean());
}

// -- snapshot -----------------------------------------------------------

fn run_snapshot(db: &Path, dir: &Path, keep: Option<usize>) -> Result<(), CommandError> {
    let store = Store::open(db).map_err(runtime)?;
    let policy = SnapshotPolicy {
        directory: dir.to_path_buf(),
        keep: keep.unwrap_or(DEFAULT_SNAPSHOT_KEEP),
    };
    let report = store.take_snapshot(&policy, Utc::now()).map_err(runtime)?;

    println!("snapshot : {}", report.path.display());
    println!("pruned   : {}", report.pruned.len());
    for path in &report.pruned {
        println!("  {}", path.display());
    }
    Ok(())
}

// -- daemon -----------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn run_daemon(
    db: &Path,
    realm: &RealmId,
    focused_secs: Option<u64>,
    idle_secs: Option<u64>,
    snapshot_dir: Option<PathBuf>,
    keep: Option<usize>,
    snapshot_hour_utc: Option<u32>,
    once: bool,
    mock: bool,
) -> Result<(), CommandError> {
    if !mock {
        return Err(CommandError::MissingHttpClient);
    }

    let store = Store::open(db).map_err(runtime)?;
    let clock = SystemClock;
    let default_cadence = Cadence::default();
    let cadence = Cadence {
        focused: focused_secs
            .map(Duration::from_secs)
            .unwrap_or(default_cadence.focused),
        idle: idle_secs
            .map(Duration::from_secs)
            .unwrap_or(default_cadence.idle),
    };
    let snapshot = match (snapshot_dir, keep) {
        (Some(directory), Some(keep)) => Some(SnapshotPolicy { directory, keep }),
        _ => None,
    };
    let options = DaemonOptions {
        cadence,
        sync: SyncOptions::default(),
        snapshot,
        snapshot_hour_utc: snapshot_hour_utc.unwrap_or(DaemonOptions::default().snapshot_hour_utc),
    };

    let client = MockQbo::new(clock.now());
    let mut daemon = Daemon::new(client, clock, options);

    if once {
        let tick = daemon.tick(&store, realm);
        print_tick_line(&tick);
        if let Ok(report) = &tick.report {
            print_sync_report_table(report);
        }
        println!("next delay: {}s", tick.next_delay.as_secs());
        return Ok(());
    }

    println!(
        "daemon running for realm {realm} against {} — Ctrl-C is safe (every write is \
         transactional); type \"stop\" and press Enter on stdin for a clean exit",
        db.display()
    );

    let stop = Arc::new(AtomicBool::new(false));
    spawn_stdin_stop_watcher(Arc::clone(&stop));
    daemon.run(&store, realm, &stop, print_tick_line);

    Ok(())
}

/// One line per tick: timestamp, path per entity summarised, next delay, and
/// the snapshot path when one was taken.
fn print_tick_line(tick: &Tick) {
    let path_summary = match &tick.report {
        Ok(report) => report
            .entities
            .iter()
            .map(|entity| {
                format!(
                    "{}={}",
                    entity.entity_type.as_str(),
                    sync_path_label(&entity.path)
                )
            })
            .collect::<Vec<_>>()
            .join(" "),
        Err(error) => format!("error={error}"),
    };
    let snapshot = match &tick.snapshot {
        None => "-".to_string(),
        Some(Ok(report)) => report.path.display().to_string(),
        Some(Err(error)) => format!("snapshot-error={error}"),
    };

    println!(
        "{} next_delay={}s snapshot={snapshot} {path_summary}",
        Utc::now().to_rfc3339(),
        tick.next_delay.as_secs(),
    );
}

fn sync_path_label(path: &SyncPath) -> &'static str {
    match path {
        SyncPath::FullSweep { .. } => "sweep",
        SyncPath::Cdc { .. } => "cdc",
        SyncPath::CdcThenBackfill { .. } => "backfill",
    }
}

fn print_sync_report_table(report: &SyncReport) {
    println!(
        "{:<14} {:<10} {:>9} {:>10} {:>12} {:>9}",
        "entity", "path", "mirrored", "projected", "quarantined", "requests"
    );
    for entity in &report.entities {
        println!(
            "{:<14} {:<10} {:>9} {:>10} {:>12} {:>9}",
            entity.entity_type.as_str(),
            sync_path_label(&entity.path),
            entity.mirrored,
            entity.projected,
            entity.quarantined,
            entity.requests,
        );
    }
}

/// Watches stdin for EOF or a line reading "stop" and sets `stop` when either
/// happens. Ctrl-C on the foreground process is the other, simpler way out —
/// safe because every write this daemon makes is transactional, so the worst
/// a kill loses is the in-flight tick, never a half-written one.
fn spawn_stdin_stop_watcher(stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.lock().read_line(&mut line) {
                Ok(0) => break, // EOF
                Ok(_) if line.trim() == "stop" => break,
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        stop.store(true, Ordering::Relaxed);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_arguments_is_a_usage_error() {
        assert_eq!(parse(&[]), Err(UsageError::usage()));
    }

    #[test]
    fn help_is_a_usage_error_with_no_reason() {
        assert_eq!(parse(&args(&["--help"])), Err(UsageError::usage()));
        assert_eq!(parse(&args(&["-h"])), Err(UsageError::usage()));
    }

    #[test]
    fn an_unknown_subcommand_is_refused() {
        let result = parse(&args(&["frobnicate"]));
        assert_eq!(result, Err(UsageError::unknown_subcommand("frobnicate")));
    }

    #[test]
    fn status_defaults_to_no_db() {
        assert_eq!(parse(&args(&["status"])), Ok(Command::Status { db: None }));
    }

    #[test]
    fn status_takes_an_optional_db() {
        assert_eq!(
            parse(&args(&["status", "--db", "replica.db"])),
            Ok(Command::Status {
                db: Some(PathBuf::from("replica.db"))
            })
        );
    }

    #[test]
    fn status_rejects_an_unknown_flag() {
        assert_eq!(
            parse(&args(&["status", "--bogus"])),
            Err(UsageError::unknown_flag("--bogus"))
        );
    }

    #[test]
    fn init_requires_db_realm_and_name() {
        assert_eq!(
            parse(&args(&[
                "init",
                "--realm",
                "1234567890123456",
                "--name",
                "Aquamentor"
            ])),
            Err(UsageError::missing_flag("--db"))
        );
        assert_eq!(
            parse(&args(&["init", "--db", "r.db", "--name", "Aquamentor"])),
            Err(UsageError::missing_flag("--realm"))
        );
        assert_eq!(
            parse(&args(&[
                "init",
                "--db",
                "r.db",
                "--realm",
                "1234567890123456"
            ])),
            Err(UsageError::missing_flag("--name"))
        );
    }

    #[test]
    fn init_parses_a_complete_invocation() {
        let parsed = parse(&args(&[
            "init",
            "--db",
            "replica.db",
            "--realm",
            "1234567890123456",
            "--name",
            "Aquamentor, Inc.",
        ]))
        .unwrap();
        assert_eq!(
            parsed,
            Command::Init {
                db: PathBuf::from("replica.db"),
                realm: RealmId::parse("1234567890123456").unwrap(),
                name: "Aquamentor, Inc.".to_string(),
            }
        );
    }

    #[test]
    fn init_rejects_a_non_numeric_realm() {
        assert_eq!(
            parse(&args(&[
                "init",
                "--db",
                "r.db",
                "--realm",
                "not-a-realm",
                "--name",
                "x"
            ])),
            Err(UsageError::invalid_value("--realm", "not-a-realm"))
        );
    }

    #[test]
    fn daemon_requires_db_and_realm() {
        assert_eq!(
            parse(&args(&["daemon", "--realm", "1234567890123456"])),
            Err(UsageError::missing_flag("--db"))
        );
        assert_eq!(
            parse(&args(&["daemon", "--db", "r.db"])),
            Err(UsageError::missing_flag("--realm"))
        );
    }

    #[test]
    fn daemon_defaults_once_and_mock_to_false() {
        let parsed = parse(&args(&[
            "daemon",
            "--db",
            "r.db",
            "--realm",
            "1234567890123456",
        ]))
        .unwrap();
        match parsed {
            Command::Daemon { once, mock, .. } => {
                assert!(!once);
                assert!(!mock);
            }
            other => panic!("expected Command::Daemon, got {other:?}"),
        }
    }

    #[test]
    fn daemon_toggles_once_and_mock() {
        let parsed = parse(&args(&[
            "daemon",
            "--db",
            "r.db",
            "--realm",
            "1234567890123456",
            "--once",
            "--mock",
        ]))
        .unwrap();
        match parsed {
            Command::Daemon { once, mock, .. } => {
                assert!(once);
                assert!(mock);
            }
            other => panic!("expected Command::Daemon, got {other:?}"),
        }
    }

    #[test]
    fn daemon_parses_cadence_and_snapshot_flags() {
        let parsed = parse(&args(&[
            "daemon",
            "--db",
            "r.db",
            "--realm",
            "1234567890123456",
            "--focused-secs",
            "5",
            "--idle-secs",
            "120",
            "--snapshot-dir",
            "snaps",
            "--keep",
            "3",
            "--snapshot-hour-utc",
            "9",
        ]))
        .unwrap();
        assert_eq!(
            parsed,
            Command::Daemon {
                db: PathBuf::from("r.db"),
                realm: RealmId::parse("1234567890123456").unwrap(),
                focused_secs: Some(5),
                idle_secs: Some(120),
                snapshot_dir: Some(PathBuf::from("snaps")),
                keep: Some(3),
                snapshot_hour_utc: Some(9),
                once: false,
                mock: false,
            }
        );
    }

    #[test]
    fn daemon_snapshot_dir_and_keep_must_be_given_together() {
        assert!(parse(&args(&[
            "daemon",
            "--db",
            "r.db",
            "--realm",
            "1234567890123456",
            "--snapshot-dir",
            "snaps",
        ]))
        .is_err());
        assert!(parse(&args(&[
            "daemon",
            "--db",
            "r.db",
            "--realm",
            "1234567890123456",
            "--keep",
            "3",
        ]))
        .is_err());
    }

    #[test]
    fn daemon_rejects_an_unknown_flag() {
        assert_eq!(
            parse(&args(&[
                "daemon",
                "--db",
                "r.db",
                "--realm",
                "1234567890123456",
                "--bogus"
            ])),
            Err(UsageError::unknown_flag("--bogus"))
        );
    }

    #[test]
    fn sweep_requires_db_and_realm() {
        assert_eq!(
            parse(&args(&["sweep", "--realm", "1234567890123456"])),
            Err(UsageError::missing_flag("--db"))
        );
        assert_eq!(
            parse(&args(&["sweep", "--db", "r.db"])),
            Err(UsageError::missing_flag("--realm"))
        );
    }

    #[test]
    fn sweep_parses_with_mock() {
        assert_eq!(
            parse(&args(&[
                "sweep",
                "--db",
                "r.db",
                "--realm",
                "1234567890123456",
                "--mock"
            ])),
            Ok(Command::Sweep {
                db: PathBuf::from("r.db"),
                realm: RealmId::parse("1234567890123456").unwrap(),
                mock: true,
            })
        );
    }

    #[test]
    fn snapshot_requires_db_and_dir() {
        assert_eq!(
            parse(&args(&["snapshot", "--dir", "snaps"])),
            Err(UsageError::missing_flag("--db"))
        );
        assert_eq!(
            parse(&args(&["snapshot", "--db", "r.db"])),
            Err(UsageError::missing_flag("--dir"))
        );
    }

    #[test]
    fn snapshot_parses_an_optional_keep() {
        assert_eq!(
            parse(&args(&["snapshot", "--db", "r.db", "--dir", "snaps"])),
            Ok(Command::Snapshot {
                db: PathBuf::from("r.db"),
                dir: PathBuf::from("snaps"),
                keep: None,
            })
        );
        assert_eq!(
            parse(&args(&[
                "snapshot", "--db", "r.db", "--dir", "snaps", "--keep", "4"
            ])),
            Ok(Command::Snapshot {
                db: PathBuf::from("r.db"),
                dir: PathBuf::from("snaps"),
                keep: Some(4),
            })
        );
    }

    #[test]
    fn a_flag_missing_its_value_is_refused() {
        assert_eq!(
            parse(&args(&["status", "--db"])),
            Err(UsageError::missing_value("--db"))
        );
    }
}
