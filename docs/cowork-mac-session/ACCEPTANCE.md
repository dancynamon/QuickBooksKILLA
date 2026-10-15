# Acceptance — M0 on the Mac

Status as of 14 Oct 2026 (local Claude Code session on the Mac, D26–D27).
Earlier: 28 Sep 2026 (Cowork session, D24–D25). The Cowork shell on the
Mac is a Linux VM without the macOS toolchain or keychain, so every item that
needs the Mac itself runs through `tools/m0-live.sh` from a real Terminal (or a
local Claude Code session) — see `KICKOFF.md`.

From `HANDOFF.md` §5, Aquamentor only:

- [x] Aquamentor fully mirrored, complete history, actual row counts reported — 67,747 rows over 22 entity types (D26 has them per entity); reconcile sweep remote = local for every type, `clean: true`, quarantine 0 (`sweep.log`, 14 Oct)
- [x] Initial sync wall-clock reported (target under 10 minutes, measured) — 225 s, 81 requests, clean pull into an empty DB (`initial-pull-timing.log`); 334 s on the first pull before the query fixes
- [x] CDC poll keeps the replica current (cursor advances, quarantine stays 0) — cursor 16:28:50 → 16:41:35 UTC over 10 min, a new Bill mirrored (3,900 → 3,901), quarantine 0 at t0 and t+10 (`status-t0-step5.log`, `status-t10-step5.log`)
- [x] Token survives an app restart — daemon killed and restarted 14 Oct 16:41 UTC, ticked within seconds, no prompt; access token also rotated mid-run (16:33:39 UTC) and the rotated item read back cleanly
- [x] Token survives a 24-hour idle, including a rotation — ticked on rotation evidence (Dan, 15 Oct): over 23 h of live daemon use Intuit rotated the refresh token (expiry 23 Jan → 24 Jan 2027) and access tokens renewed hourly, every one saved to and read back from the keychain with no prompt; `status` authorised 15 Oct 16:03 UTC. Caveat: the token was in use, not idle — a no-use 24 h gap was not tested
- [x] `cargo test --workspace` still fully offline and green — 677 passed, 0 failed, 5 ignored on the Mac, 14 Oct, after the M0 fixes (672 / 4 on Linux, 28 Sep)
- [x] Writes still disabled for every realm — `write enabled : false` in every status log; only reads (query, CDC, GET, reports) went to Intuit, 13–14 Oct

Added by the cloud session:

- [x] The four ⚠️ API facts verified and recorded — D25 (batch budget corrected to 40/min; TaxCode/TaxRate moved off CDC; RequestId Customer/Item exception undocumented by Intuit, guard kept)
- [ ] Scrubbed fixtures committed; the suite replays them — **blocked, D26**: recorded (172 MB, 81 pages), but the scrub is refused by the leak detector (short real names) and would keep real balances; scrubber needs a slice + amount scrubbing first. Nothing committed
- [x] `ledger import` has run once against the real replica and the first
      `tbdiff` report is recorded verbatim as the parallel-run baseline — D27:
      7 must-match failures, 5 may-differ unexplained, 39,963 of 51,233
      documents rejected, 185 accounts to map; ten open questions for Dan and Joel
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
- [x] Realm authorised in the keychain (`m0-live.sh tokens`) — 13 Oct, Playground-minted for this app; refresh expires 23 Jan 2027
