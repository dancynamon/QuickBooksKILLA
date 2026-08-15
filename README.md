# ledger-workspace

Cargo workspace for Dan Cynamon's accounting build (Aquamentor / WaterLine CNC).

## Layout

- `crates/ledger-core` — shared money type, domain enums, QBO entity models. No I/O.
- `apps/qbo-local` — Tauri v2 + Rust + SQLite replica of QuickBooks Online, outbox write queue.
- `apps/ledger` — standalone double-entry GL. Stub only, not yet built.

## Spec

See the prompt files in Dropbox:
`DC Claude Shared/Claude Tasks and Projects/Accounting System Build/`
