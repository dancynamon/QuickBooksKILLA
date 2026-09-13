//! BunzBooks desktop shell. `ROADMAP.md` §B1, D14.
//!
//! The entire Tauri-specific surface is one command, `query`, and its body
//! is one call into [`qbo_desktop_commands::query`] — everything the query
//! actually does (dispatch, argument parsing, the eleven tool
//! implementations) lives in `apps/desktop/commands` and `apps/qbo-local`,
//! both of which build and are tested without Tauri. This file exists only
//! to open the replica once, hold it for the life of the window, and hand
//! `invoke("query", { tool, args })` calls from `js/data/tauri.js` to it.
//!
//! Not built here: the Tauri toolchain and the system libraries it needs
//! are not installed in this environment (`Cargo.toml`'s header comment,
//! `apps/desktop/README.md`). Written to compile once the Mac session runs
//! `cargo tauri dev` / `cargo tauri build` from `apps/desktop/src-tauri/`.

use std::sync::Mutex;

use qbo_local::store::Store;
use serde_json::Value;
use tauri::State;

/// The one thing this app's Tauri state holds: the read-only replica,
/// opened once at startup. A `Mutex` because `rusqlite::Connection` is not
/// `Sync` — `Store` wraps one connection, and Tauri commands can run on any
/// thread in the pool, so every call takes the lock rather than assuming
/// it already has exclusive access.
struct AppState {
    store: Mutex<Store>,
}

/// The whole Tauri command. `tool` and `args` are passed straight through
/// to [`qbo_desktop_commands::query`]; its `Ok` becomes the resolved
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

fn main() {
    let db_path = std::env::var("QBO_LOCAL_DB")
        .expect("QBO_LOCAL_DB must be set to the replica's sqlite file path");
    let store = Store::open_read_only(&db_path)
        .unwrap_or_else(|err| panic!("failed to open {db_path:?} read-only: {err}"));

    tauri::Builder::default()
        .manage(AppState {
            store: Mutex::new(store),
        })
        .invoke_handler(tauri::generate_handler![query])
        .run(tauri::generate_context!())
        .expect("error while running the BunzBooks Tauri application");
}
