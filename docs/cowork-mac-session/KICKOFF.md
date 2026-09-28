# Kickoff — what is left, and how to run it

**Updated 28 Sep 2026.** Steps 1–3 of the original prompt below are done
(D24 plan change, D25 API facts, `KeychainTokenStore`, `import-token`,
`status --config`; `.local/config.toml` converted). The rest needs the Mac's
own shell — `cargo`, the login keychain, a long-running daemon — which a
Cowork session does not have (its shell is a Linux VM; Terminal is
click-only). Run it from Terminal on the Mac:

```sh
cd ~/code/QuickBooksKILLA
tools/m0-live.sh build keychain     # offline suite + real keychain round trip
tools/m0-live.sh tokens             # paste a Playground-minted refresh token
tools/m0-live.sh sync fixtures ledger daemon
```

Logs land in `.local/m0-run/` (row counts, `wall_clock_secs`, the tbdiff
report). A Cowork session then reads those logs, records the counts and the
tbdiff baseline in `DECISIONS.md`, reads every scrubbed fixture before
committing it, and ticks `ACCEPTANCE.md`.

Corrections to the prompt below:

- `HttpQboClient`, the OAuth loopback flow and `--live` **exist** (13 Sep,
  `38db865`). A clone 16 commits behind origin is what made them look absent.
- **Do not import `qbo-transaction-jumper`'s refresh token.** That server
  refreshes `tokens.json` itself; importing its token forks one rotation chain
  between two holders and one of them loses. Mint a token for qbo-local in the
  Intuit OAuth Playground (the redirect that is registered on the app) and
  `import-token` it. `qbo-local auth` works only if
  `http://localhost:8765/callback` is also registered.
- The client secret stays in `.local/config.toml` (Dan, 28 Sep);
  `QBO_CLIENT_SECRET` still overrides it when set.
- Step 8 (Tauri) is M1 and not part of this run.

---

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
   `ledger import --db .local/ledger.db --company aquamentor --replica
   .local/replica.db --realm <id>` and `ledger tb --as-of <today>`. Pull
   QBO's trial balance as of the same date with `qbo-local report --realm
   <id> --name trial-balance --as-of <today> --out .local/qbo-tb.csv --live`
   and run `ledger tbdiff --qbo-csv .local/qbo-tb.csv`. Then `qbo-local
   snapshots --from-year 2012 --to-year 2025 --dir .local/snapshots --live`
   and `ledger boundary --snapshots .local/snapshots` to find the boundary
   year. Do not chase the variances; record both reports verbatim in
   `DECISIONS.md` as the baseline the parallel run starts from, and install
   the plist `ledger nightly --plist` prints (`docs/NIGHTLY.md`).
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
