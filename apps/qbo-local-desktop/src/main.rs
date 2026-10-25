//! BunzBooks — the desktop shell around `qbo-local`.
//!
//! One command, `api`, forwards to `qbo_local::api::dispatch`: the same entry
//! point the loopback dev server uses, so the UI is identical in both and every
//! command is tested in the library rather than here. The replica opens before
//! the window does; the UI never waits on the network because there is none in
//! its path.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Mutex;

use qbo_local::store::Store;

/// SQLite connections are not shareable across threads; Tauri runs commands on
/// a pool. One lock around one connection is correct for a read-only UI whose
/// every query returns in milliseconds.
struct Replica(Mutex<Store>);

#[tauri::command]
fn api(
    replica: tauri::State<'_, Replica>,
    command: String,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let store = replica.0.lock().map_err(|_| "replica lock poisoned".to_string())?;
    qbo_local::api::dispatch(&store, &command, args).map_err(|error| error.to_string())
}

/// `QBO_LOCAL_DB` if set, else the first `replica-*.db` in the app home the CLI
/// writes to (`HANDOFF.md` §1.1).
fn replica_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("QBO_LOCAL_DB") {
        return Ok(PathBuf::from(path));
    }
    let home = match std::env::var_os("QBO_LOCAL_HOME") {
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
    let mut replicas: Vec<PathBuf> = std::fs::read_dir(&home)
        .map_err(|e| format!("{}: {e}", home.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            name.starts_with("replica-") && name.ends_with(".db")
        })
        .collect();
    replicas.sort();
    replicas
        .into_iter()
        .next()
        .ok_or_else(|| format!("no replica-*.db in {}; run `qbo-local sync` first", home.display()))
}

fn main() {
    let store = replica_path().and_then(|path| {
        if path.to_string_lossy().to_lowercase().contains("dropbox") {
            return Err("refusing a replica inside Dropbox".to_string());
        }
        Store::open(&path).map_err(|e| format!("{}: {e}", path.display()))
    });
    let store = match store {
        Ok(store) => store,
        Err(message) => {
            eprintln!("BunzBooks: {message}");
            std::process::exit(1);
        }
    };

    tauri::Builder::default()
        .manage(Replica(Mutex::new(store)))
        .invoke_handler(tauri::generate_handler![api])
        .run(tauri::generate_context!())
        .expect("BunzBooks failed to start");
}
