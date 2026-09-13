# Kickoff prompt — paste this into the Mac session

TIER: Sonnet is enough for everything here except step 1, which is reading and
judgement; run the whole session on Sonnet and re-run step 1 on Opus only if
the verification findings look thin.

---

You are finishing M0 of `qbo-local` on the machine that holds the Intuit
credentials. The clone is mounted; branch `claude/modest-allen-82wp1w`. Read,
in this order: `README.md`, `HANDOFF.md` (all of it), `ROADMAP.md` §0 and §A,
`docs/cowork-mac-session/ACCEPTANCE.md`. Then check `git log --oneline -30` so
you know what the last cloud session built: the HTTP client, the OAuth loopback
flow, the fixture recorder, the daemon, the sweep, the ledger, the MCP servers.
Do not rebuild anything that exists; wire it.

Work in this order, one commit per concern, conventional commits, and never
push to any branch other than `claude/modest-allen-82wp1w`:

1. **Verify the four ⚠️ API facts** against `developer.intuit.com` (HANDOFF §4,
   DESIGN §0 and §12). Record findings in `DECISIONS.md` as a new D entry,
   update DESIGN §0's confidence column, and change code only where a fact
   turned out wrong (the batch limit is a config value; the query endpoint's
   `LastUpdatedTime` range support is what the D12 backfill stands on).
2. **`KeychainTokenStore`** implementing `auth::TokenStore` with the
   `security-framework` crate, `cfg(target_os = "macos")`, `TokenGenerations`
   as the stored unit, a test that round-trips through the real keychain.
3. **Config and tokens.** `cp .local/config.example.toml .local/config.toml`,
   fill in the client id and the realm, and export `QBO_CLIENT_SECRET` in the
   shell rather than writing the secret into the file. If `qbo_headless`
   holds a valid refresh token, import it into the keychain store as
   generation zero. Otherwise run `qbo-local auth --realm <id>`, which is the
   loopback flow built in the cloud (`apps/qbo-local/src/oauth.rs`), and
   complete consent in the browser. Either way, confirm
   `qbo-local status --db .local/replica.db` shows the realm authorised.
4. **First live sync**, read-only, production realm:
   `qbo-local init --db .local/replica.db --realm <id> --name Aquamentor`,
   then `qbo-local sweep --db .local/replica.db --realm <id> --live`, then
   `qbo-local daemon --db .local/replica.db --realm <id> --live --once`.
   `HttpQboClient` is built and proven against a fake Intuit server
   (`apps/qbo-local/tests/http_client.rs`); this is its first contact with
   the real one, so read the first error carefully rather than patching. Report row counts per entity type and
   wall-clock for the initial pull, measured, in `DECISIONS.md`. Compare the
   counts with what the QBO web UI shows for customers, invoices and bills.
5. **Record fixtures** with `qbo-local record --db .local/replica.db --realm
   <id> --dir .local/fixtures/raw --live`, run
   `python3 tools/scrub-fixtures.py .local/fixtures/raw .local/fixtures/scrubbed`
   (deterministic, with a leak detector that fails on any surviving real
   name), verify with `qbo-local replay --dir .local/fixtures/scrubbed`, and
   commit only the scrubbed synthetic set under `apps/qbo-local/tests/fixtures/`
   once you have read every file for real names. HANDOFF §2.6 is the rule.
6. **Leave the daemon running** (`qbo-local daemon --live`) and confirm after
   ten minutes that `status` shows the CDC cursor advancing and zero
   quarantined rows. Then the 24-hour idle and rotation check in ACCEPTANCE.md
   is a follow-up, not this session.
7. **The ledger's first real run**: `ledger init --db .local/ledger.db
   --company aquamentor --name Aquamentor --realm <id>`, then
   `ledger import --replica .local/replica.db --realm <id>` and `ledger tb
   --as-of <today>`. Pull QBO's Trial Balance report as of the same date
   (Reports API, accrual) into a CSV and run `ledger tbdiff`. Do not chase the
   variances; record the report verbatim in `DECISIONS.md` as the baseline the
   parallel run starts from.
8. **Tauri shell**: read `apps/desktop/README.md`. The UI is built and
   tested (`cd apps/desktop/ui && node --test`); `apps/desktop/commands` is
   the whole command body; `apps/desktop/src-tauri` is the scaffold, outside
   the workspace because it only compiles with the Tauri toolchain. Add
   `apps/desktop/src-tauri` to the workspace members, `cargo tauri dev` with
   `QBO_LOCAL_DB=.local/replica.db`, and every screen should show the live
   replica through the `invoke` provider. If it does not build on this Mac,
   record the exact error and stop there; the UI is M1, not M0.

Constraints that do not relax: `realms.is_write_enabled` stays 0; no write
path is exercised against production; secrets go to the keychain only; the
replica and the ledger never live in Dropbox; every commit keeps `cargo test
--workspace` green and offline.

Report at the end in Dan's DID / GATE / FLAGGED / BLOCKED shape, with the
measured numbers from steps 4 and 7.
