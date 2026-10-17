//! `qbo-local` — status entry point, plus the live commands.
//!
//! The Tauri shell and React UI are not built yet. With no arguments this
//! binary reports what the M0 foundations hold. Built with `--features live` it
//! also runs the two commands M0 acceptance needs on a machine with Intuit
//! credentials:
//!
//! ```text
//! qbo-local authorize [--port 8765] [--sandbox]
//! qbo-local sync --realm <id> [--db <path>] [--sandbox] [--watch <seconds>]
//! qbo-local reconcile --realm <id> [--db <path>] [--sandbox]
//! ```

use chrono::Utc;
use qbo_local::domain::{EntityType, RealmId, SyncTier};
use qbo_local::ratelimit::RealmLimits;
use qbo_local::store::{latest_version, ProjectedTable, Store};
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("status") => status(),
        #[cfg(feature = "live")]
        Some(command) => live::run(command, &args[1..]),
        #[cfg(not(feature = "live"))]
        Some(command) => Err(format!(
            "unknown command {command:?}; `authorize` and `sync` need a build with --features live"
        )
        .into()),
    }
}

fn status() -> Result<(), Box<dyn std::error::Error>> {
    let store = Store::open_in_memory()?;
    // Nothing is mirrored yet; this exists so the counts below have a realm to
    // be zero for, rather than being printed as an unexplained blank.
    let realm = RealmId::parse("0000000000000000")?;
    store.register_realm(&realm, "empty replica", Utc::now())?;
    let limits = RealmLimits::default();

    println!("qbo-local — M0 foundations");
    println!();
    println!("  replica schema version : {}", store.schema_version()?);
    println!("  latest migration       : {}", latest_version());
    println!();

    let masters = EntityType::ALL.iter().filter(|e| e.tier() == SyncTier::Masters).count();
    let documents = EntityType::ALL.iter().filter(|e| e.tier() == SyncTier::Documents).count();
    let peripheral = EntityType::ALL.iter().filter(|e| e.tier() == SyncTier::Peripheral).count();

    println!("  entity types           : {} total", EntityType::ALL.len());
    println!("    masters (M0)         : {masters}");
    println!("    documents (M0)       : {documents}");
    println!("    peripheral (later)   : {peripheral}");
    println!();

    println!("  rate budgets per realm");
    println!("    general              : {}/min", limits.general.refill_per_minute);
    println!("    batch                : {}/min", limits.batch.refill_per_minute);
    println!("    reports              : {}/min", limits.reports.refill_per_minute);
    println!("    max concurrent       : {}", limits.max_concurrent);
    println!();

    println!("  cdc lookback (documented) : {CDC_LOOKBACK_DAYS} days");
    println!("  cdc cursor max age (ours) : {DEFAULT_CDC_MAX_AGE_DAYS} days");
    println!();
    // An empty replica: the interesting figure is that these are all derived,
    // so every one of them can be thrown away and rebuilt from `entities`.
    let projected: i64 = PROJECTED_TABLES
        .iter()
        .map(|table| store.count_projected(&realm, *table).unwrap_or(0))
        .sum();

    println!("  projection (§3.2)");
    println!("    tables       : {}", PROJECTED_TABLES.len());
    println!("    rows         : {projected}");
    println!("    rebuilt from : entities.raw_json, no Intuit round-trip");
    println!();

    println!("  writes: disabled for every realm until explicitly enabled");
    println!("  generated at {}", Utc::now().to_rfc3339());

    Ok(())
}

#[cfg(feature = "live")]
mod live {
    //! The commands that talk to Intuit. Nothing here holds logic worth
    //! testing — it is argument parsing and wiring; the logic lives in the
    //! library, where it is tested offline.

    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use chrono::Utc;
    use qbo_local::auth::FileTokenStore;
    use qbo_local::domain::{EntityType, RealmId};
    use qbo_local::driver::{SyncDriver, SyncError, SyncOptions};
    use qbo_local::http::live::UreqTransport;
    use qbo_local::http::{HttpQboClient, HttpQboConfig};
    use qbo_local::oauth::{authorize_url, await_callback, exchange_code, ClientCredentials, TokenManager};
    use qbo_local::store::Store;

    type Error = Box<dyn std::error::Error>;

    pub fn run(command: &str, args: &[String]) -> Result<(), Error> {
        match command {
            "authorize" => authorize(args),
            "sync" => sync(args),
            "reconcile" => reconcile(args),
            other => Err(format!("unknown command {other:?}").into()),
        }
    }

    fn flag(args: &[String], name: &str) -> bool {
        args.iter().any(|a| a == name)
    }

    fn value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
    }

    /// Where tokens, logs and the replica live. `QBO_LOCAL_HOME` overrides.
    /// Refuses anything under Dropbox: a synced SQLite file is a corrupted one,
    /// and a synced token file is a leaked one (`DESIGN.md` §8).
    fn home() -> Result<PathBuf, Error> {
        let path = match std::env::var_os("QBO_LOCAL_HOME") {
            Some(path) => PathBuf::from(path),
            None => {
                let base = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?);
                if cfg!(target_os = "macos") {
                    base.join("Library/Application Support/qbo-local")
                } else {
                    base.join(".local/share/qbo-local")
                }
            }
        };
        if path.to_string_lossy().to_lowercase().contains("dropbox") {
            return Err(format!("refusing {}: never inside Dropbox", path.display()).into());
        }
        std::fs::create_dir_all(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(path)
    }

    fn credentials() -> Result<ClientCredentials, Error> {
        let read = |name: &str| std::env::var(name).map_err(|_| format!("{name} is not set"));
        Ok(ClientCredentials {
            client_id: read("QBO_CLIENT_ID")?,
            client_secret: read("QBO_CLIENT_SECRET")?,
        })
    }

    fn tokens(home: &std::path::Path) -> Result<TokenManager<UreqTransport, FileTokenStore>, Error> {
        Ok(TokenManager::new(UreqTransport::default(), FileTokenStore::new(home), credentials()?)
            .with_rotation_log(home.join("rotation-log.jsonl")))
    }

    fn authorize(args: &[String]) -> Result<(), Error> {
        let home = home()?;
        let port: u16 = value(args, "--port").unwrap_or("8765").parse()?;
        let redirect_uri = format!("http://localhost:{port}/callback");
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let state = uuid::Uuid::now_v7().simple().to_string();
        let credentials = credentials()?;

        println!("Open this URL, approve access, and pick the company:\n");
        println!("  {}\n", authorize_url(&credentials, &redirect_uri, &state));
        println!("Waiting for the redirect to {redirect_uri} ...");

        let callback = await_callback(&listener, &state)?;
        let mut transport = UreqTransport::default();
        let set = exchange_code(&mut transport, &credentials, &callback.code, &redirect_uri, Utc::now())?;
        let mut manager = tokens(&home)?;
        manager.install(&callback.realm_id, set)?;

        println!("authorised realm {}", callback.realm_id);
        println!("tokens saved under {}", home.display());
        Ok(())
    }

    type LiveDriver = SyncDriver<HttpQboClient<UreqTransport, TokenManager<UreqTransport, FileTokenStore>>>;

    /// Store and driver for `--realm`, shared by `sync` and `reconcile`.
    fn open(args: &[String]) -> Result<(RealmId, Store, LiveDriver), Error> {
        let home = home()?;
        let realm = RealmId::parse(value(args, "--realm").ok_or("--realm is required")?)?;
        let db = value(args, "--db")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(format!("replica-{realm}.db")));
        if db.to_string_lossy().to_lowercase().contains("dropbox") {
            return Err("refusing a replica path inside Dropbox".into());
        }
        let config = if flag(args, "--sandbox") {
            HttpQboConfig::sandbox()
        } else {
            HttpQboConfig::production()
        };
        let store = Store::open(&db)?;
        store.register_realm(&realm, realm.as_str(), Utc::now())?;
        let client = HttpQboClient::new(UreqTransport::default(), tokens(&home)?, config);
        let driver = SyncDriver::new(client, SyncOptions::default(), Utc::now());
        Ok((realm, store, driver))
    }

    fn reconcile(args: &[String]) -> Result<(), Error> {
        let (realm, store, mut driver) = open(args)?;
        let scope: Vec<EntityType> = EntityType::m0_scope().collect();
        let started = Instant::now();
        let (reports, orphans) = driver.reconcile_realm(&store, &realm, &scope, Utc::now())?;
        let mut dirty = 0;
        for report in &reports {
            println!(
                "  {:<14} remote {:>6}  local {:>6}  healed {:>4}  quarantined {:>3}  requests {:>4}",
                report.entity_type.as_str(),
                report.remote,
                report.local,
                report.healed,
                report.quarantined_extra,
                report.requests
            );
            dirty += report.discrepancies.len();
        }
        println!(
            "{dirty} discrepancies, {orphans} orphaned lines, {:.1}s wall clock",
            started.elapsed().as_secs_f64()
        );
        if orphans > 0 {
            return Err(format!("{orphans} document lines have no parent document").into());
        }
        Ok(())
    }

    fn sync(args: &[String]) -> Result<(), Error> {
        let watch = value(args, "--watch").map(str::parse::<u64>).transpose()?;
        let (realm, store, mut driver) = open(args)?;
        let scope: Vec<EntityType> = EntityType::m0_scope().collect();

        loop {
            let started = Instant::now();
            match driver.sync_realm(&store, &realm, &scope, Utc::now()) {
                Ok(report) => {
                    for entity in &report.entities {
                        println!(
                            "  {:<14} mirrored {:>6}  quarantined {:>3}  requests {:>4}  {:?}",
                            entity.entity_type.as_str(),
                            entity.mirrored,
                            entity.quarantined,
                            entity.requests,
                            entity.path
                        );
                    }
                    println!(
                        "{} mirrored, {} quarantined, {} requests, {:.1}s wall clock",
                        report.mirrored(),
                        report.quarantined(),
                        report.requests(),
                        started.elapsed().as_secs_f64()
                    );
                }
                // Out of budget is not a failure; the next pass resumes from
                // the cursors already committed.
                Err(SyncError::RateBudgetExhausted { realm }) => {
                    eprintln!("rate budget exhausted for {realm}; resuming next pass");
                }
                Err(error) => return Err(error.into()),
            }
            match watch {
                Some(seconds) => std::thread::sleep(Duration::from_secs(seconds)),
                None => return Ok(()),
            }
        }
    }
}
