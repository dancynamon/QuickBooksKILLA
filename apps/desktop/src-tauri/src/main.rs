//! BunzBooks desktop shell. `ROADMAP.md` §B1, D14.
//!
//! Two commands, each one call into `apps/desktop/commands`:
//! `query` wraps [`qbo_desktop_commands::query`] over the read-only
//! qbo-local replica; `ledger_query` wraps
//! [`qbo_desktop_commands::ledger_query`] over the ledger's own reads and
//! gated writes. Everything either command actually does — argument
//! parsing, dispatch, error shaping — lives in `apps/desktop/commands` and
//! is already tested there (`cargo test -p qbo-desktop-commands`); this file
//! exists only to open each store once, hold it for the life of the window,
//! and hand `invoke(...)` calls from `js/data/tauri.js` and
//! `js/data/ledger/tauri.js` to them.
//!
//! Configuration (environment, all optional — `apps/desktop/README.md`):
//!
//! - `QBO_LOCAL_DB` — the replica. Opened `SQLITE_OPEN_READ_ONLY`
//!   ([`Store::open_read_only`]): the `qbo-local daemon` owns every write to
//!   it, and this process can never become a second writer. Not
//!   `immutable=1` — the daemon is writing the file while the window is open,
//!   and an immutable open would tell SQLite to skip the WAL and locking and
//!   read torn or stale pages. Unset, the repo's own `.local/replica.db` is
//!   used when it exists; with no replica at all the window still opens and
//!   `query` answers every call with an error saying so.
//! - `LEDGER_DB` / `LEDGER_COMPANY` — the book the ledger screens read and
//!   post to. Unset, a **scratch** book under
//!   `~/Library/Application Support/QuickBooksKILLA/scratch-ledger.sqlite`,
//!   company `aquamentor`. Never a real book by default: a real one is only
//!   ever opened because `LEDGER_DB` names it. The chart and classes are
//!   seeded ([`Ledger::create_company`]) only into the default scratch file or
//!   a file this process just created — never into an existing book someone
//!   pointed `LEDGER_DB` at, whose company row `ledger init` already wrote.
//! - `BUNZBOOKS_SELFTEST=1` — after the page loads, the webview itself calls
//!   both commands through `window.__TAURI__.core.invoke` (replica sync
//!   status, ledger trial balance, a test invoice, the trial balance again),
//!   prints the results to stdout as one JSON line prefixed `SELFTEST `, and
//!   shows the trial balance screen. `BUNZBOOKS_SELFTEST_EXIT_AFTER=<secs>`
//!   quits that many seconds later (time for a screenshot).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ledger::store::Ledger;
use qbo_local::store::Store;
use serde_json::{json, Value};
use tauri::{State, WebviewWindowBuilder};

/// This app's Tauri state: the read-only replica and the read-write ledger,
/// each opened once at startup. A `Mutex` on each because neither
/// `rusqlite::Connection` (the replica) nor `Ledger` (whose own methods take
/// `&self` but whose `mcp::LedgerToolSet` borrows it exclusively for the
/// life of one call, same reasoning) is `Sync` in a way that lets two Tauri
/// commands on different pool threads share one without a lock — every call
/// takes its lock rather than assuming exclusive access.
struct AppState {
    /// `Err` carries why there is no replica, so every `query` can say so.
    store: Result<Mutex<Store>, String>,
    ledger: Mutex<Ledger>,
    ledger_company: String,
    selftest: Option<SelfTest>,
}

struct SelfTest {
    exit_after_secs: Option<u64>,
}

/// The read-only replica command. `tool` and `args` are passed straight
/// through to [`qbo_desktop_commands::query`]; its `Ok` becomes the resolved
/// `invoke()` value, its `Err`'s `Display` string (via `thiserror`) becomes
/// the rejection `js/data/tauri.js` turns into an `Error` — no reshaping in
/// either direction, so this command can't drift from what the commands
/// crate's own tests already cover.
#[tauri::command]
fn query(state: State<AppState>, tool: String, args: Value) -> Result<Value, String> {
    let store = state.store.as_ref().map_err(Clone::clone)?;
    let store = store
        .lock()
        .map_err(|_| "the replica's lock was poisoned by an earlier panic".to_string())?;
    qbo_desktop_commands::query(&store, &tool, args).map_err(|err| err.to_string())
}

/// The ledger command — reads and gated writes alike. `tool` and `args` are
/// passed straight through to [`qbo_desktop_commands::ledger_query`], scoped
/// to the company `LEDGER_COMPANY` named at startup; the same no-reshaping
/// contract as [`query`] applies, so `js/data/ledger/tauri.js`'s rejection
/// handling and this command can never drift from the commands crate's own
/// tests. A write's domain failure (a closed period, a missing class, an
/// unbalanced journal) reaches the frontend exactly the way a read's does —
/// as a rejected `invoke()` carrying the tool's own message — never a panic.
#[tauri::command]
fn ledger_query(state: State<AppState>, tool: String, args: Value) -> Result<Value, String> {
    let ledger = state
        .ledger
        .lock()
        .map_err(|_| "the ledger's lock was poisoned by an earlier panic".to_string())?;
    qbo_desktop_commands::ledger_query(&ledger, &state.ledger_company, &tool, args)
        .map_err(|err| err.to_string())
}

/// Where `BUNZBOOKS_SELFTEST`'s page script reports. Refused outside self-test
/// mode, so a page can never use it to quit the app.
#[tauri::command]
fn selftest_report(app: tauri::AppHandle, state: State<AppState>, report: Value) -> Result<(), String> {
    let selftest = state
        .selftest
        .as_ref()
        .ok_or_else(|| "not in self-test mode".to_string())?;
    println!("SELFTEST {report}");
    if let Some(secs) = selftest.exit_after_secs {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            app.exit(if report["ok"] == json!(true) { 0 } else { 1 });
        });
    }
    Ok(())
}

/// The repo checkout this binary was built from — only used to find the
/// gitignored `.local/replica.db` when `QBO_LOCAL_DB` is unset. A build
/// from a Claude Code worktree (`<repo>/.claude/worktrees/<name>`) has no
/// `.local/` of its own, so that resolves to the main checkout above it.
fn repo_root() -> PathBuf {
    let built_from = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let built_from = built_from.canonicalize().unwrap_or(built_from);
    let text = built_from.to_string_lossy();
    match text.find("/.claude/worktrees/") {
        Some(at) => PathBuf::from(&text[..at]),
        None => built_from,
    }
}

fn open_replica() -> (Result<Mutex<Store>, String>, Vec<Value>) {
    let path = match std::env::var_os("QBO_LOCAL_DB") {
        Some(path) => PathBuf::from(path),
        None => repo_root().join(".local/replica.db"),
    };
    if !path.exists() {
        let why = format!(
            "no replica at {} — set QBO_LOCAL_DB to the qbo-local replica's sqlite path",
            path.display()
        );
        eprintln!("bunzbooks: {why}");
        return (Err(why), Vec::new());
    }
    match Store::open_read_only(&path) {
        Ok(store) => {
            let realms = store
                .list_realms()
                .unwrap_or_default()
                .into_iter()
                .map(|r| json!({ "id": r.realm_id.as_str(), "name": r.display_name }))
                .collect();
            eprintln!("bunzbooks: replica {} (read-only)", path.display());
            (Ok(Mutex::new(store)), realms)
        }
        Err(err) => {
            let why = format!("failed to open the replica {} read-only: {err}", path.display());
            eprintln!("bunzbooks: {why}");
            (Err(why), Vec::new())
        }
    }
}

fn scratch_ledger_path() -> PathBuf {
    let home = std::env::var_os("HOME").expect("HOME must be set");
    PathBuf::from(home).join("Library/Application Support/QuickBooksKILLA/scratch-ledger.sqlite")
}

fn open_ledger() -> (Ledger, String) {
    let company = std::env::var("LEDGER_COMPANY").unwrap_or_else(|_| "aquamentor".to_string());
    let (path, is_default_scratch) = match std::env::var_os("LEDGER_DB") {
        Some(path) => (PathBuf::from(path), false),
        None => (scratch_ledger_path(), true),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|err| panic!("failed to create {}: {err}", parent.display()));
    }
    let is_new = !path.exists();
    let ledger =
        Ledger::open(&path).unwrap_or_else(|err| panic!("failed to open {}: {err}", path.display()));
    if is_default_scratch || is_new {
        let display_name = match company.as_str() {
            "waterline" => "WaterLine CNC (scratch)",
            _ => "Aquamentor (scratch)",
        };
        ledger
            .create_company(&company, display_name, None, chrono::Utc::now())
            .unwrap_or_else(|err| panic!("failed to seed {company:?} in {}: {err}", path.display()));
    }
    eprintln!(
        "bunzbooks: ledger {} company {company}{}",
        path.display(),
        if is_default_scratch { " (scratch)" } else { "" }
    );
    (ledger, company)
}

/// Runs before any page script: the replica's own realm list for
/// `js/data/realms.js` (the shipped list is invented placeholder data), and in
/// self-test mode the self-test itself.
fn initialization_script(realms: &[Value], selftest: bool) -> String {
    let mut script = format!("window.__BUNZBOOKS_REALMS__ = {};\n", Value::from(realms.to_vec()));
    if selftest {
        script.push_str(SELFTEST_JS);
    }
    script
}

const SELFTEST_JS: &str = r##"
window.addEventListener("load", () => setTimeout(async () => {
  const invoke = window.__TAURI__.core.invoke;
  const report = { ok: true, steps: {} };
  const step = async (name, fn) => {
    try { report.steps[name] = await fn(); }
    catch (err) { report.ok = false; report.steps[name] = { error: String(err) }; }
  };
  const realm = (window.__BUNZBOOKS_REALMS__[0] || {}).id;
  const today = new Date().toISOString().slice(0, 10);
  await step("replica_sync_status", async () => {
    const s = await invoke("query", { tool: "sync_status", args: { realm_id: realm } });
    return { realm, write_enabled: s.write_enabled,
             mirrored: (s.entities || []).reduce((n, e) => n + (e.mirrored || 0), 0) };
  });
  const tb = async () => {
    const t = await invoke("ledger_query", { tool: "trial_balance", args: { as_of: today } });
    return { total_debits: t.total_debits, total_credits: t.total_credits, rows: t.rows.length };
  };
  await step("ledger_trial_balance_before", tb);
  await step("ledger_create_invoice", async () => {
    await invoke("ledger_query", { tool: "upsert_contact", args: { contact: {
      contact_id: "selftest-customer", kind: "Customer", display_name: "Self-test Customer",
      terms_days: 30, default_class: "foam" } } });
    await invoke("ledger_query", { tool: "upsert_item", args: { item: {
      item_id: "selftest-item", sku: "SELFTEST-1", name: "Self-test item",
      item_type: "non_inventory", income_account: "4100", expense_account: "5000",
      default_class: "foam", sales_price: "10.00", is_taxable: false } } });
    const inv = await invoke("ledger_query", { tool: "create_document", args: { actor: "selftest",
      document: { kind: "Invoice", txn_date: today,
        contact: { kind: "Customer", id: "selftest-customer" },
        lines: [{ kind: "Item", item_id: "selftest-item", qty: "2", amount: "20.00",
                  is_taxable: false, description: "Self-test item" }] } } });
    return { number: inv.number, total: inv.total, document_id: inv.document_id, entry_id: inv.entry_id };
  });
  await step("ledger_trial_balance_after", tb);
  location.hash = "#ledger-tb";
  await invoke("selftest_report", { report });
}, 1500));
"##;

fn main() {
    let (store, realms) = open_replica();
    let (ledger, ledger_company) = open_ledger();
    let selftest = std::env::var("BUNZBOOKS_SELFTEST").is_ok_and(|v| v == "1").then(|| SelfTest {
        exit_after_secs: std::env::var("BUNZBOOKS_SELFTEST_EXIT_AFTER")
            .ok()
            .and_then(|v| v.parse().ok()),
    });
    let init_script = initialization_script(&realms, selftest.is_some());

    tauri::Builder::default()
        .manage(AppState {
            store,
            ledger: Mutex::new(ledger),
            ledger_company,
            selftest,
        })
        .invoke_handler(tauri::generate_handler![query, ledger_query, selftest_report])
        .setup(move |app| {
            // `tauri.conf.json`'s window has `"create": false` so it can be
            // built here with the initialization script attached.
            let config = app.config().app.windows[0].clone();
            WebviewWindowBuilder::from_config(app.handle(), &config)?
                .initialization_script(&init_script)
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the BunzBooks Tauri application");
}
