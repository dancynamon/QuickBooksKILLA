# Acceptance — M0 on the Mac

Status as of 28 Sep 2026 (Cowork session, D24–D25). The Cowork shell on the
Mac is a Linux VM without the macOS toolchain or keychain, so every item that
needs the Mac itself runs through `tools/m0-live.sh` from a real Terminal (or a
local Claude Code session) — see `KICKOFF.md`.

From `HANDOFF.md` §5, Aquamentor only:

- [ ] Aquamentor fully mirrored, complete history, actual row counts reported
- [ ] Initial sync wall-clock reported (target under 10 minutes, measured)
- [ ] CDC poll keeps the replica current (cursor advances, quarantine stays 0)
- [ ] Token survives an app restart
- [ ] Token survives a 24-hour idle, including a rotation (follow-up check)
- [x] `cargo test --workspace` still fully offline and green — 672 passed, 4 ignored (Linux, 28 Sep); `m0-live.sh build` re-runs it on the Mac
- [x] Writes still disabled for every realm — nothing in this session touches `is_write_enabled`; no write call was made

Added by the cloud session:

- [x] The four ⚠️ API facts verified and recorded — D25 (batch budget corrected to 40/min; TaxCode/TaxRate moved off CDC; RequestId Customer/Item exception undocumented by Intuit, guard kept)
- [ ] Scrubbed fixtures committed; the suite replays them (`m0-live.sh fixtures` records and scrubs; commit after reading every file)
- [ ] `ledger import` has run once against the real replica and the first
      `tbdiff` report is recorded verbatim as the parallel-run baseline
- [ ] `docs/MCP.md`: `qbo-local-mcp` registered in Claude Desktop against the
      live replica and one read skill repointed for a smoke test

Built in the 28 Sep session:

- [x] `KeychainTokenStore` (`apps/qbo-local/src/keychain.rs`), default token
      store on macOS; real-keychain round trip is `#[ignore]`d
      (`m0-live.sh keychain` runs it)
- [x] `qbo-local import-token` (refresh token on stdin → generation zero) and
      `status --config` (authorised / fingerprint / expiries per realm)
- [x] `.local/config.toml` converted to the schema `config.rs` reads (backup
      `.local/config.toml.bak-20260928`)
- [ ] Realm authorised in the keychain (`m0-live.sh tokens`)
