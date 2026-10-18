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
//!
//! Always available, no Intuit involved:
//!
//! ```text
//! qbo-local demo --db <path> [--invoices N]     # write a synthetic book
//! qbo-local serve --db <path> [--port 8780]     # UI + API on 127.0.0.1
//! qbo-local snapshot --db <path> --dir <dir> [--keep 14]   # dated copy, rotated
//! qbo-local writes --db <path> --realm <id> [--enable [--confirm] | --disable]
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
        Some("demo") => offline::demo(&args[1..]),
        Some("serve") => offline::serve(&args[1..]),
        Some("snapshot") => offline::snapshot(&args[1..]),
        Some("writes") => offline::writes(&args[1..]),
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

mod offline {
    //! Commands that never reach Intuit: the synthetic book and the dev server.

    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::time::Instant;

    use chrono::Utc;
    use qbo_local::demo::{self, DemoSize};
    use qbo_local::store::Store;

    type Error = Box<dyn std::error::Error>;

    fn value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
    }

    pub fn demo(args: &[String]) -> Result<(), Error> {
        let db = PathBuf::from(value(args, "--db").ok_or("--db is required")?);
        if db.exists() {
            return Err(format!("{} exists; demo writes only to a new file", db.display()).into());
        }
        let mut size = DemoSize::default();
        if let Some(invoices) = value(args, "--invoices") {
            size.invoices = invoices.parse()?;
        }
        let store = Store::open(&db)?;
        let started = Instant::now();
        let written = demo::seed(&store, size, 42, Utc::now())?;
        println!(
            "wrote {written} synthetic entities to {} in {:.2}s (realm {})",
            db.display(),
            started.elapsed().as_secs_f64(),
            demo::DEMO_REALM
        );
        Ok(())
    }

    /// Show or change whether a company takes writes (`DESIGN.md` §8). With
    /// neither flag, reports the current state and what enabling would need.
    pub fn writes(args: &[String]) -> Result<(), Error> {
        let db = PathBuf::from(value(args, "--db").ok_or("--db is required")?);
        let realm = qbo_local::domain::RealmId::parse(value(args, "--realm").ok_or("--realm is required")?)?;
        let store = Store::open(&db)?;
        if !store.realms()?.iter().any(|r| r.realm_id == realm) {
            return Err(format!("{realm} is not in this replica").into());
        }
        let has = |name: &str| args.iter().any(|a| a == name);
        if has("--disable") {
            store.set_write_enabled(&realm, false)?;
            println!("writes off for {realm}; queued writes stay queued");
            return Ok(());
        }
        if has("--enable") {
            qbo_local::writes::check_enable(&store, &realm, has("--confirm"), Utc::now())?;
            store.set_write_enabled(&realm, true)?;
            println!("writes on for {realm} ({})", store.realm_environment(&realm)?.as_str());
            return Ok(());
        }
        let on = store.is_write_enabled(&realm)?;
        println!("{realm}: {} company, writes {}", store.realm_environment(&realm)?.as_str(), if on { "on" } else { "off" });
        if !on {
            match qbo_local::writes::check_enable(&store, &realm, true, Utc::now()) {
                Ok(()) => println!("can be enabled now with --enable --confirm"),
                Err(why) => println!("cannot enable yet: {why}"),
            }
        }
        Ok(())
    }

    /// Run nightly from launchd or cron. Keeps the newest `--keep` dated
    /// snapshots in `--dir` and removes older ones it wrote itself.
    pub fn snapshot(args: &[String]) -> Result<(), Error> {
        let db = PathBuf::from(value(args, "--db").ok_or("--db is required")?);
        let dir = PathBuf::from(value(args, "--dir").ok_or("--dir is required")?);
        if dir.to_string_lossy().to_lowercase().contains("dropbox") {
            return Err("refusing a snapshot directory inside Dropbox".into());
        }
        let keep: usize = value(args, "--keep").unwrap_or("14").parse()?;
        let store = Store::open(&db)?;
        let report = qbo_local::safety::snapshot_and_rotate(&store, &dir, keep, Utc::now())?;
        match &report.written {
            Some(path) => println!("wrote {}", path.display()),
            None => println!("today's snapshot already exists"),
        }
        for path in &report.removed {
            println!("removed {}", path.display());
        }
        println!("{} snapshots kept", report.kept.len());
        Ok(())
    }

    pub fn serve(args: &[String]) -> Result<(), Error> {
        let db = PathBuf::from(value(args, "--db").ok_or("--db is required")?);
        if !db.exists() {
            return Err(format!("{} does not exist", db.display()).into());
        }
        let port: u16 = value(args, "--port").unwrap_or("8780").parse()?;
        let ui = value(args, "--ui")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui/dist"));
        let started = Instant::now();
        let store = Store::open(&db)?;
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        println!(
            "replica open in {:.0} ms; serving http://127.0.0.1:{port}/ (read-only)",
            started.elapsed().as_secs_f64() * 1000.0
        );
        qbo_local::serve::run(listener, &store, &ui)?;
        Ok(())
    }
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
    use qbo_local::auth::{FileTokenStore, TokenStore};
    use qbo_local::keychain::KeychainTokenStore;
    use qbo_local::domain::{EntityType, RealmId};
    use qbo_local::driver::{SyncDriver, SyncError, SyncOptions};
    use qbo_local::http::live::UreqTransport;
    use qbo_local::http::{HttpQboClient, HttpQboConfig};
    use qbo_local::oauth::{authorize_url, await_callback, exchange_code, ClientCredentials, TokenManager};
    use qbo_local::safety::{AuditLog, AuditedClient};
    use qbo_local::store::outbox_store::Environment;
    use qbo_local::store::Store;
    use qbo_local::writes::drain_once;

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

    type Tokens = TokenManager<UreqTransport, Box<dyn TokenStore>>;

    /// Keychain on macOS (`DESIGN.md` §5.1). `--file-tokens`, or any other
    /// platform, uses the 0700 app home instead — development only.
    fn tokens(home: &std::path::Path, args: &[String]) -> Result<Tokens, Error> {
        let store: Box<dyn TokenStore> = if cfg!(target_os = "macos") && !flag(args, "--file-tokens") {
            Box::new(KeychainTokenStore::new())
        } else {
            Box::new(FileTokenStore::new(home))
        };
        Ok(TokenManager::new(UreqTransport::default(), store, credentials()?)
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
        let mut manager = tokens(&home, args)?;
        manager.install(&callback.realm_id, set)?;

        println!("authorised realm {}", callback.realm_id);
        println!("tokens saved to {}", if cfg!(target_os = "macos") && !flag(args, "--file-tokens") { "the login keychain (service qbo-local)".to_string() } else { home.display().to_string() });
        Ok(())
    }

    type LiveDriver = SyncDriver<AuditedClient<HttpQboClient<UreqTransport, Tokens>>>;

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
        let sandbox = flag(args, "--sandbox");
        let config = if sandbox { HttpQboConfig::sandbox() } else { HttpQboConfig::production() };
        let store = Store::open(&db)?;
        let known = store.realms()?.into_iter().any(|r| r.realm_id == realm);
        store.register_realm(&realm, realm.as_str(), Utc::now())?;
        if !known && sandbox {
            store.set_realm_environment(&realm, Environment::Sandbox)?;
        }
        // A realm id names one company in one environment. Pointing a
        // production replica at the sandbox host, or the reverse, would mix
        // two books in one file.
        let recorded = store.realm_environment(&realm)?;
        if (recorded == Environment::Sandbox) != sandbox {
            return Err(format!("this replica is a {} company; run with{} --sandbox", recorded.as_str(), if sandbox { "out" } else { "" }).into());
        }
        let audit = AuditLog::new(home.join(format!("audit-{realm}.jsonl")));
        let client = AuditedClient::new(HttpQboClient::new(UreqTransport::default(), tokens(&home, args)?, config), audit);
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
            // Writes go first, so the sync that follows mirrors what they made.
            if store.is_write_enabled(&realm)? {
                match drain_once(&store, &realm, driver.client_mut(), Utc::now()) {
                    Ok(report) => {
                        let d = &report.drain;
                        if d.applied + d.conflicted + d.rejected + d.retried + report.recovered > 0 {
                            println!(
                                "writes: {} applied ({} adopted), {} conflicted, {} rejected, {} retrying, {} recovered",
                                d.applied, d.adopted, d.conflicted, d.rejected, d.retried, report.recovered
                            );
                        }
                    }
                    Err(error) => eprintln!("writes: {error}"),
                }
            }
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
