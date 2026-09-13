//! `qbo-local` — the subcommand binary. `HANDOFF.md` §2.5, `ROADMAP.md` §A.
//!
//! Wires the pieces built so far — [`Store`], [`Daemon`], [`Reconciler`],
//! [`HttpQboClient`] — into something runnable against a file database and,
//! with `--live`, against a real QBO realm. Every subcommand that would need
//! Intuit credentials accepts either `--live` (backed by [`HttpQboClient`],
//! built from `--config`; `HANDOFF.md` §2.3, §2.1) or `--mock` (backed by
//! [`MockQbo`]); without either, each refuses with the same exact message
//! rather than guessing which the caller meant.
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
use uuid::Uuid;

use qbo_local::auth::{FileTokenStore, TokenGenerations, TokenStore};
use qbo_local::client::{MockQbo, QboClient};
use qbo_local::clock::{Clock, SystemClock};
use qbo_local::config::LocalConfig;
use qbo_local::daemon::{Cadence, Daemon, DaemonOptions, Tick};
use qbo_local::domain::{EntityType, RealmId, SyncTier};
use qbo_local::driver::{SyncDriver, SyncOptions, SyncPath, SyncReport};
use qbo_local::fixture::{FixtureQbo, RecordingQbo};
use qbo_local::http::{HttpQboClient, TokenSource};
use qbo_local::oauth::{self, OAuthConfig};
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

/// Every subcommand that talks to QBO needs a client, and there are exactly
/// two ways to get one: `--live` (real HTTP, `HANDOFF.md` §2.3) or `--mock`
/// ([`MockQbo`]). Neither given is a usage error, not a guess.
const NO_CLIENT_SELECTED: &str =
    "no QBO client selected: pass --live (see `qbo-local auth`) or --mock to exercise the loop";

/// Where `--live` looks for its config when `--config` is not given.
/// `HANDOFF.md` §0: `.local/` is gitignored in full.
const DEFAULT_CONFIG_PATH: &str = ".local/config.toml";

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
        Err(CommandError::NoClientSelected) => {
            eprintln!("{NO_CLIENT_SELECTED}");
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
    Auth {
        realm: RealmId,
        port: Option<u16>,
        config: Option<PathBuf>,
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
        live: bool,
        config: Option<PathBuf>,
    },
    Sweep {
        db: PathBuf,
        realm: RealmId,
        mock: bool,
        live: bool,
        config: Option<PathBuf>,
    },
    Snapshot {
        db: PathBuf,
        dir: PathBuf,
        keep: Option<usize>,
    },
    Record {
        db: PathBuf,
        realm: RealmId,
        dir: PathBuf,
        mock: bool,
        live: bool,
        config: Option<PathBuf>,
    },
    Replay {
        db: PathBuf,
        realm: RealmId,
        dir: PathBuf,
    },
}

const USAGE: &str = "\
qbo-local — local SQLite replica of QuickBooks Online

USAGE:
    qbo-local status [--db PATH]
    qbo-local init --db PATH --realm ID --name \"Display Name\"
    qbo-local auth --realm ID [--port N] [--config PATH]
    qbo-local daemon --db PATH --realm ID [--focused-secs N] [--idle-secs N]
                      [--snapshot-dir DIR --keep N] [--snapshot-hour-utc H]
                      [--once] (--live [--config PATH] | --mock)
    qbo-local sweep --db PATH --realm ID (--live [--config PATH] | --mock)
    qbo-local snapshot --db PATH --dir DIR [--keep N]
    qbo-local record --db PATH --realm ID --dir DIR [--mock|--live] [--config PATH]
    qbo-local replay --db PATH --realm ID --dir DIR
    qbo-local --help

SUBCOMMANDS:
    status      Print the M0 foundations, and sync status per realm with --db.
    init        Open (creating if absent) a file store and register a realm.
                Idempotent — registering an existing realm is not an error.
    auth        Run the OAuth loopback flow and save generation zero for a
                realm through FileTokenStore. (The macOS keychain backend
                arrives when this runs on Dan's machine — HANDOFF.md §2.1;
                FileTokenStore is the store everywhere else, tests included.)
    daemon      Run the CDC poll loop against a realm until stopped.
    sweep       Run one reconciliation sweep against QBO's index.
    snapshot    Take one nightly-style backup of the replica, with rotation.
    record      Record every QBO response for a realm's sync into DIR as
                fixtures (HANDOFF.md §2.6). Scrub before committing any of
                it — see tools/scrub-fixtures.py.
    replay      Sync a realm from fixtures previously recorded into DIR,
                touching no network at all.

FLAGS:
    --db PATH               Path to the SQLite replica file.
    --realm ID              A QBO realm id (digits only).
    --name \"Display Name\"   The realm's display name (init only).
    --port N                Loopback redirect port (auth; default from config).
    --config PATH           Path to the local config file (default: .local/config.toml).
    --focused-secs N        Focused-cadence poll interval, in seconds (daemon).
    --idle-secs N           Idle-cadence poll interval, in seconds (daemon).
    --snapshot-dir DIR      Where nightly snapshots go (daemon; pair with --keep).
    --keep N                Snapshots to retain (daemon, snapshot).
    --snapshot-hour-utc H   UTC hour at or after which a snapshot may fire (daemon).
    --dir DIR               Where a snapshot is written (snapshot), or where
                            fixtures are recorded to / replayed from (record,
                            replay).
    --once                  Run a single tick and exit (daemon).
    --live                  Use HttpQboClient against a real QBO realm (HANDOFF.md §2.3).
    --mock                  Use an empty in-memory MockQbo instead of live QBO.

Without --live or --mock, daemon, sweep and record exit 2: choose one.
Ctrl-C is safe to stop `daemon` with — every write is transactional — or type
\"stop\" and press Enter on its stdin for a clean exit.

EXIT CODES:
    0   ok
    1   runtime error
    2   usage error (this message), or neither --live nor --mock given (daemon, sweep)
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
        "auth" => parse_auth(args),
        "daemon" => parse_daemon(args),
        "sweep" => parse_sweep(args),
        "snapshot" => parse_snapshot(args),
        "record" => parse_record(args),
        "replay" => parse_replay(args),
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

fn parse_u16(raw: &str, flag: &str) -> Result<u16, UsageError> {
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

fn parse_auth(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut realm = None;
    let mut port = None;
    let mut config = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--realm" => realm = Some(parse_realm(&next_value(&mut args, "--realm")?)?),
            "--port" => port = Some(parse_u16(&next_value(&mut args, "--port")?, "--port")?),
            "--config" => config = Some(PathBuf::from(next_value(&mut args, "--config")?)),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(Command::Auth {
        realm: realm.ok_or_else(|| UsageError::missing_flag("--realm"))?,
        port,
        config,
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
    let mut live = false;
    let mut config = None;

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
            "--live" => live = true,
            "--config" => config = Some(PathBuf::from(next_value(&mut args, "--config")?)),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }

    if snapshot_dir.is_some() != keep.is_some() {
        return Err(UsageError::new(
            "--snapshot-dir and --keep must be given together",
        ));
    }
    if mock && live {
        return Err(UsageError::new("--mock and --live cannot both be given"));
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
        live,
        config,
    })
}

fn parse_sweep(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut realm = None;
    let mut mock = false;
    let mut live = false;
    let mut config = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--realm" => realm = Some(parse_realm(&next_value(&mut args, "--realm")?)?),
            "--mock" => mock = true,
            "--live" => live = true,
            "--config" => config = Some(PathBuf::from(next_value(&mut args, "--config")?)),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    if mock && live {
        return Err(UsageError::new("--mock and --live cannot both be given"));
    }
    Ok(Command::Sweep {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        realm: realm.ok_or_else(|| UsageError::missing_flag("--realm"))?,
        mock,
        live,
        config,
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

fn parse_record(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut realm = None;
    let mut dir = None;
    let mut mock = false;
    let mut live = false;
    let mut config = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--realm" => realm = Some(parse_realm(&next_value(&mut args, "--realm")?)?),
            "--dir" => dir = Some(PathBuf::from(next_value(&mut args, "--dir")?)),
            "--mock" => mock = true,
            "--live" => live = true,
            "--config" => config = Some(PathBuf::from(next_value(&mut args, "--config")?)),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    if mock && live {
        return Err(UsageError::new("--mock and --live are mutually exclusive"));
    }
    Ok(Command::Record {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        realm: realm.ok_or_else(|| UsageError::missing_flag("--realm"))?,
        dir: dir.ok_or_else(|| UsageError::missing_flag("--dir"))?,
        mock,
        live,
        config,
    })
}

fn parse_replay(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut realm = None;
    let mut dir = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--realm" => realm = Some(parse_realm(&next_value(&mut args, "--realm")?)?),
            "--dir" => dir = Some(PathBuf::from(next_value(&mut args, "--dir")?)),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(Command::Replay {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        realm: realm.ok_or_else(|| UsageError::missing_flag("--realm"))?,
        dir: dir.ok_or_else(|| UsageError::missing_flag("--dir"))?,
    })
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

enum CommandError {
    /// Neither `--live` nor `--mock` was given to a subcommand that needs a
    /// QBO client.
    NoClientSelected,
    Runtime(String),
}

fn runtime<E: std::fmt::Display>(error: E) -> CommandError {
    CommandError::Runtime(error.to_string())
}

fn execute(command: Command) -> Result<(), CommandError> {
    match command {
        Command::Status { db } => run_status(db),
        Command::Init { db, realm, name } => run_init(&db, &realm, &name),
        Command::Auth {
            realm,
            port,
            config,
        } => run_auth(&realm, port, config.as_deref()),
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
            live,
            config,
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
            live,
            config.as_deref(),
        ),
        Command::Sweep {
            db,
            realm,
            mock,
            live,
            config,
        } => run_sweep(&db, &realm, mock, live, config.as_deref()),
        Command::Snapshot { db, dir, keep } => run_snapshot(&db, &dir, keep),
        Command::Record {
            db,
            realm,
            dir,
            mock,
            live,
            config,
        } => run_record(&db, &realm, &dir, mock, live, config.as_deref()),
        Command::Replay { db, realm, dir } => run_replay(&db, &realm, &dir),
    }
}

/// Build an [`HttpQboClient`] from `--config` (or [`DEFAULT_CONFIG_PATH`])
/// for `realm`. Shared by `daemon --live` and `sweep --live` so the two
/// don't drift on how a config file becomes a client.
fn build_http_client(
    config_path: Option<&Path>,
    realm: &RealmId,
) -> Result<HttpQboClient, CommandError> {
    let path = config_path.unwrap_or(Path::new(DEFAULT_CONFIG_PATH));
    let config = LocalConfig::load(path).map_err(runtime)?;
    // Validates that `realm` is one this config file actually lists, so a
    // typo'd --realm fails loudly here rather than as a confusing empty sync.
    config.realm(realm.as_str()).map_err(runtime)?;
    let secret = config.client_secret().map_err(runtime)?;
    let redirect_uri = format!("http://localhost:{}/callback", config.intuit.redirect_port);
    let oauth_config = OAuthConfig::intuit(config.intuit.client_id.clone(), secret, redirect_uri);
    let token_store: Box<dyn TokenStore> = Box::new(FileTokenStore::new(&config.tokens_dir));
    let tokens = TokenSource::new(token_store, oauth_config, realm.clone());
    Ok(HttpQboClient::new(
        config.base_url(),
        tokens,
        config.rotation_log.clone(),
    ))
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

// -- auth -------------------------------------------------------------------

/// Run the OAuth loopback flow for one realm and save generation zero
/// through [`FileTokenStore`] (`HANDOFF.md` §2.1, §2.2). The keychain-backed
/// store arrives when this runs on Dan's machine; `FileTokenStore` is the
/// store everywhere else, `--live` included.
fn run_auth(
    realm: &RealmId,
    port: Option<u16>,
    config_path: Option<&Path>,
) -> Result<(), CommandError> {
    let path = config_path.unwrap_or(Path::new(DEFAULT_CONFIG_PATH));
    let config = LocalConfig::load(path).map_err(runtime)?;
    let secret = config.client_secret().map_err(runtime)?;
    let port = port.unwrap_or(config.intuit.redirect_port);
    let redirect_uri = format!("http://localhost:{port}/callback");
    let oauth_config = OAuthConfig::intuit(config.intuit.client_id.clone(), secret, redirect_uri);

    let state = Uuid::now_v7().to_string();
    println!("Open this URL to authorize qbo-local for realm {realm}:\n");
    println!("  {}\n", oauth_config.authorize_url(&state));
    println!("Waiting for the redirect on http://localhost:{port}/callback ...");

    let auth_code = oauth::run_loopback(port, Duration::from_secs(300)).map_err(runtime)?;
    if auth_code.state != state {
        return Err(CommandError::Runtime(
            "state mismatch on the OAuth callback (possible CSRF) — aborting without saving anything".to_string(),
        ));
    }
    if !auth_code.realm_id.is_empty() && auth_code.realm_id != realm.as_str() {
        eprintln!(
            "warning: the callback's realmId ({}) does not match --realm {realm}; saving under --realm as given",
            auth_code.realm_id
        );
    }

    let agent = ureq::AgentBuilder::new().build();
    let token_set =
        oauth::exchange_code(&oauth_config, &auth_code.code, &agent).map_err(runtime)?;

    let generations = TokenGenerations::new(realm.clone(), token_set);
    let store = FileTokenStore::new(&config.tokens_dir);
    store.save(&generations).map_err(runtime)?;

    println!(
        "Saved generation zero for realm {realm} to {}",
        config.tokens_dir.display()
    );
    Ok(())
}

// -- sweep ------------------------------------------------------------------

fn run_sweep(
    db: &Path,
    realm: &RealmId,
    mock: bool,
    live: bool,
    config: Option<&Path>,
) -> Result<(), CommandError> {
    match (mock, live) {
        (true, _) => run_sweep_with_client(db, realm, MockQbo::new(Utc::now())),
        (false, true) => run_sweep_with_client(db, realm, build_http_client(config, realm)?),
        (false, false) => Err(CommandError::NoClientSelected),
    }
}

fn run_sweep_with_client<C: QboClient>(
    db: &Path,
    realm: &RealmId,
    client: C,
) -> Result<(), CommandError> {
    let store = Store::open(db).map_err(runtime)?;
    let now = Utc::now();
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

// -- record -----------------------------------------------------------------

/// `HANDOFF.md` §2.6: record every response a sync makes into `dir` as
/// offline fixtures, via [`RecordingQbo`].
///
/// `--live` is deliberately its own arm rather than falling through to the
/// `!mock` case below, even though both exit the same way today: the day
/// `HttpQboClient` (`HANDOFF.md` §2.3) lands, this arm is the one line that
/// changes, and every other subcommand here that will eventually take a live
/// client shares the same shape.
fn run_record(
    db: &Path,
    realm: &RealmId,
    dir: &Path,
    mock: bool,
    live: bool,
    config: Option<&Path>,
) -> Result<(), CommandError> {
    // The parser already rejects --mock together with --live.
    match (mock, live) {
        (true, _) => record_with_client(db, realm, dir, MockQbo::new(Utc::now())),
        (false, true) => record_with_client(db, realm, dir, build_http_client(config, realm)?),
        (false, false) => Err(CommandError::NoClientSelected),
    }
}

/// `HANDOFF.md` §2.6: sweep the realm through a [`RecordingQbo`] wrapped
/// around whichever client was chosen, so the same code path records from the
/// mock (proving the plumbing) and from live QBO (the real fixtures).
fn record_with_client<C: QboClient>(
    db: &Path,
    realm: &RealmId,
    dir: &Path,
    client: C,
) -> Result<(), CommandError> {
    let store = Store::open(db).map_err(runtime)?;
    let now = Utc::now();
    let recorder = RecordingQbo::new(client, dir);
    let mut driver = SyncDriver::new(recorder, SyncOptions::default(), now);
    let entity_types: Vec<EntityType> = EntityType::m0_scope().collect();
    let report = driver
        .sync_realm(&store, realm, &entity_types, now)
        .map_err(runtime)?;

    print_sync_report_table(&report);
    println!();
    println!("recorded to      : {}", dir.display());
    println!(
        "manifest entries : {}",
        driver.client_mut().manifest().len()
    );
    Ok(())
}

// -- replay -------------------------------------------------------------

/// `HANDOFF.md` §2.6: sync a realm entirely from fixtures previously recorded
/// into `dir`, via [`FixtureQbo`] — no network involved at any point.
fn run_replay(db: &Path, realm: &RealmId, dir: &Path) -> Result<(), CommandError> {
    let store = Store::open(db).map_err(runtime)?;
    let now = Utc::now();
    let client = FixtureQbo::new(dir);
    let mut driver = SyncDriver::new(client, SyncOptions::default(), now);
    let entity_types: Vec<EntityType> = EntityType::m0_scope().collect();
    let report = driver
        .sync_realm(&store, realm, &entity_types, now)
        .map_err(runtime)?;

    print_sync_report_table(&report);
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
    live: bool,
    config: Option<&Path>,
) -> Result<(), CommandError> {
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

    match (mock, live) {
        (true, _) => {
            run_daemon_with_client(db, realm, clock, options, once, MockQbo::new(clock.now()))
        }
        (false, true) => {
            let client = build_http_client(config, realm)?;
            run_daemon_with_client(db, realm, clock, options, once, client)
        }
        (false, false) => Err(CommandError::NoClientSelected),
    }
}

fn run_daemon_with_client<C: QboClient>(
    db: &Path,
    realm: &RealmId,
    clock: SystemClock,
    options: DaemonOptions,
    once: bool,
    client: C,
) -> Result<(), CommandError> {
    let store = Store::open(db).map_err(runtime)?;
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
                live: false,
                config: None,
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
    fn daemon_rejects_mock_and_live_together() {
        assert!(parse(&args(&[
            "daemon",
            "--db",
            "r.db",
            "--realm",
            "1234567890123456",
            "--mock",
            "--live",
        ]))
        .is_err());
    }

    #[test]
    fn daemon_parses_live_with_a_config_path() {
        let parsed = parse(&args(&[
            "daemon",
            "--db",
            "r.db",
            "--realm",
            "1234567890123456",
            "--live",
            "--config",
            "custom.toml",
        ]))
        .unwrap();
        match parsed {
            Command::Daemon {
                live, config, mock, ..
            } => {
                assert!(live);
                assert!(!mock);
                assert_eq!(config, Some(PathBuf::from("custom.toml")));
            }
            other => panic!("expected Command::Daemon, got {other:?}"),
        }
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
                live: false,
                config: None,
            })
        );
    }

    #[test]
    fn sweep_rejects_mock_and_live_together() {
        assert!(parse(&args(&[
            "sweep",
            "--db",
            "r.db",
            "--realm",
            "1234567890123456",
            "--mock",
            "--live",
        ]))
        .is_err());
    }

    #[test]
    fn sweep_parses_live_with_a_config_path() {
        assert_eq!(
            parse(&args(&[
                "sweep",
                "--db",
                "r.db",
                "--realm",
                "1234567890123456",
                "--live",
                "--config",
                "custom.toml",
            ])),
            Ok(Command::Sweep {
                db: PathBuf::from("r.db"),
                realm: RealmId::parse("1234567890123456").unwrap(),
                mock: false,
                live: true,
                config: Some(PathBuf::from("custom.toml")),
            })
        );
    }

    #[test]
    fn auth_requires_realm() {
        assert_eq!(
            parse(&args(&["auth"])),
            Err(UsageError::missing_flag("--realm"))
        );
    }

    #[test]
    fn auth_parses_port_and_config() {
        assert_eq!(
            parse(&args(&[
                "auth",
                "--realm",
                "1234567890123456",
                "--port",
                "9999",
                "--config",
                "custom.toml",
            ])),
            Ok(Command::Auth {
                realm: RealmId::parse("1234567890123456").unwrap(),
                port: Some(9999),
                config: Some(PathBuf::from("custom.toml")),
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
    fn record_requires_db_realm_and_dir() {
        assert_eq!(
            parse(&args(&[
                "record",
                "--realm",
                "1234567890123456",
                "--dir",
                "fx"
            ])),
            Err(UsageError::missing_flag("--db"))
        );
        assert_eq!(
            parse(&args(&["record", "--db", "r.db", "--dir", "fx"])),
            Err(UsageError::missing_flag("--realm"))
        );
        assert_eq!(
            parse(&args(&[
                "record",
                "--db",
                "r.db",
                "--realm",
                "1234567890123456"
            ])),
            Err(UsageError::missing_flag("--dir"))
        );
    }

    #[test]
    fn record_defaults_mock_and_live_to_false() {
        let parsed = parse(&args(&[
            "record",
            "--db",
            "r.db",
            "--realm",
            "1234567890123456",
            "--dir",
            "fx",
        ]))
        .unwrap();
        assert_eq!(
            parsed,
            Command::Record {
                db: PathBuf::from("r.db"),
                realm: RealmId::parse("1234567890123456").unwrap(),
                dir: PathBuf::from("fx"),
                mock: false,
                live: false,
                config: None,
            }
        );
    }

    #[test]
    fn record_parses_mock() {
        let parsed = parse(&args(&[
            "record",
            "--db",
            "r.db",
            "--realm",
            "1234567890123456",
            "--dir",
            "fx",
            "--mock",
        ]))
        .unwrap();
        match parsed {
            Command::Record { mock, live, .. } => {
                assert!(mock);
                assert!(!live);
            }
            other => panic!("expected Command::Record, got {other:?}"),
        }
    }

    #[test]
    fn record_rejects_mock_and_live_together() {
        assert!(parse(&args(&[
            "record",
            "--db",
            "r.db",
            "--realm",
            "1234567890123456",
            "--dir",
            "fx",
            "--mock",
            "--live",
        ]))
        .is_err());
    }

    #[test]
    fn record_rejects_an_unknown_flag() {
        assert_eq!(
            parse(&args(&[
                "record",
                "--db",
                "r.db",
                "--realm",
                "1234567890123456",
                "--dir",
                "fx",
                "--bogus"
            ])),
            Err(UsageError::unknown_flag("--bogus"))
        );
    }

    #[test]
    fn replay_requires_db_realm_and_dir() {
        assert_eq!(
            parse(&args(&[
                "replay",
                "--realm",
                "1234567890123456",
                "--dir",
                "fx"
            ])),
            Err(UsageError::missing_flag("--db"))
        );
        assert_eq!(
            parse(&args(&["replay", "--db", "r.db", "--dir", "fx"])),
            Err(UsageError::missing_flag("--realm"))
        );
        assert_eq!(
            parse(&args(&[
                "replay",
                "--db",
                "r.db",
                "--realm",
                "1234567890123456"
            ])),
            Err(UsageError::missing_flag("--dir"))
        );
    }

    #[test]
    fn replay_parses_a_complete_invocation() {
        assert_eq!(
            parse(&args(&[
                "replay",
                "--db",
                "r.db",
                "--realm",
                "1234567890123456",
                "--dir",
                "fx",
            ])),
            Ok(Command::Replay {
                db: PathBuf::from("r.db"),
                realm: RealmId::parse("1234567890123456").unwrap(),
                dir: PathBuf::from("fx"),
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
