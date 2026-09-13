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
//! Not built here: the Tauri toolchain and the system libraries it needs
//! are not installed in this environment (`Cargo.toml`'s header comment,
//! `apps/desktop/README.md`). Written to compile once the Mac session runs
//! `cargo tauri dev` / `cargo tauri build` from `apps/desktop/src-tauri/`.

use std::sync::Mutex;

use ledger::store::Ledger;
use qbo_local::store::Store;
use serde_json::Value;
use tauri::State;

/// This app's Tauri state: the read-only replica and the read-write ledger,
/// each opened once at startup. A `Mutex` on each because neither
/// `rusqlite::Connection` (the replica) nor `Ledger` (whose own methods take
/// `&self` but whose `mcp::LedgerToolSet` borrows it exclusively for the
/// life of one call, same reasoning) is `Sync` in a way that lets two Tauri
/// commands on different pool threads share one without a lock — every call
/// takes its lock rather than assuming exclusive access.
struct AppState {
    store: Mutex<Store>,
    ledger: Mutex<Ledger>,
    ledger_company: String,
}

/// The read-only replica command. `tool` and `args` are passed straight
/// through to [`qbo_desktop_commands::query`]; its `Ok` becomes the resolved
/// `invoke()` value, its `Err`'s `Display` string (via `thiserror`) becomes
/// the rejection `js/data/tauri.js` turns into an `Error` — no reshaping in
/// either direction, so this command can't drift from what the commands
/// crate's own tests already cover.
#[tauri::command]
fn query(state: State<AppState>, tool: String, args: Value) -> Result<Value, String> {
    let store = state
        .store
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

fn main() {
    let db_path = std::env::var("QBO_LOCAL_DB")
        .expect("QBO_LOCAL_DB must be set to the replica's sqlite file path");
    let store = Store::open_read_only(&db_path)
        .unwrap_or_else(|err| panic!("failed to open {db_path:?} read-only: {err}"));

    // `docs/MCP.md`'s ledger section: `LEDGER_DB` and `LEDGER_COMPANY` are
    // both required, exactly as `ledger-mcp` itself demands — this window
    // is bound to one company's book for its whole lifetime, the same way
    // `ledger-mcp` is bound to one company per process.
    let ledger_db_path = std::env::var("LEDGER_DB")
        .expect("LEDGER_DB must be set to the ledger's sqlite file path");
    let ledger_company = std::env::var("LEDGER_COMPANY").expect(
        "LEDGER_COMPANY must be set to the company id (\"aquamentor\" or \"waterline\")",
    );
    let ledger = Ledger::open(&ledger_db_path)
        .unwrap_or_else(|err| panic!("failed to open {ledger_db_path:?}: {err}"));

    tauri::Builder::default()
        .manage(AppState {
            store: Mutex::new(store),
            ledger: Mutex::new(ledger),
            ledger_company,
        })
        .invoke_handler(tauri::generate_handler![query, ledger_query])
        .run(tauri::generate_context!())
        .expect("error while running the BunzBooks Tauri application");
}
