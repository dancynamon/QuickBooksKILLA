#!/usr/bin/env bash
# M0 live steps on Dan's Mac, in order (HANDOFF.md §2, docs/cowork-mac-session/KICKOFF.md).
# Read-only against production QBO. Writes stay disabled; nothing here calls a
# write, delete or void endpoint. Everything lands under .local/ (gitignored,
# not in Dropbox). Logs: .local/m0-run/<step>.log. Never prints a token.
#
#   cd ~/code/QuickBooksKILLA && tools/m0-live.sh            # all steps
#   tools/m0-live.sh tokens                                   # one step
#
# Steps: build keychain tokens sync fixtures ledger daemon
set -euo pipefail
cd "$(dirname "$0")/.."
RUN=.local/m0-run; mkdir -p "$RUN"
BIN=target/release
REALM=$(awk -F'"' '/^\[\[realms\]\]/{r=1} r && /^id *=/{print $2; exit}' .local/config.toml)
[ -n "$REALM" ] || { echo "no [[realms]] id in .local/config.toml"; exit 2; }
TODAY=$(date +%F)
DB=.local/replica.db; LDB=.local/ledger.db
log() { echo "== $(date '+%F %T') $*" | tee -a "$RUN/steps.log"; }
timed() { local name=$1; shift; local t0 rc; t0=$(date +%s)
  set +e; "$@" 2>&1 | tee "$RUN/$name.log"; rc=${PIPESTATUS[0]}; set -e
  echo "wall_clock_secs=$(( $(date +%s) - t0 )) rc=$rc" | tee -a "$RUN/$name.log"; return "$rc"; }

step_build() {
  log "0. toolchain + offline suite"
  cargo --version | tee "$RUN/toolchain.log"
  timed test cargo test --workspace
  cargo build --release --bin qbo-local --bin ledger
}
step_keychain() {
  log "3. real-keychain round trip (macOS may ask to allow access once)"
  timed keychain-test cargo test -p qbo-local --lib keychain -- --ignored
}
step_tokens() {
  log "5. tokens -> keychain"
  if $BIN/qbo-local status --config .local/config.toml | grep -q "$REALM : authorised"; then
    log "already authorised"; else
    echo "Paste a refresh token minted for THIS app in the Intuit OAuth Playground"
    echo "(do NOT reuse qbo-transaction-jumper's tokens.json: that forks its chain)."
    read -rs -p "refresh token: " RT; echo
    printf '%s\n' "$RT" | $BIN/qbo-local import-token --realm "$REALM"; unset RT
  fi
  $BIN/qbo-local status --config .local/config.toml | tee "$RUN/status-tokens.log"
}
step_sync() {
  log "6. first live sync, read-only"
  $BIN/qbo-local init --db $DB --realm "$REALM" --name "Aquamentor, Inc."
  # Initial pull first: the driver's full sweep pages SELECT * (1000/page).
  # The reconcile sweep after it only verifies; run on an empty replica it
  # heals every id with its own GET and exhausts the budget (M0, 13 Oct 2026).
  timed daemon-once $BIN/qbo-local daemon --db $DB --realm "$REALM" --live --once
  timed sweep $BIN/qbo-local sweep --db $DB --realm "$REALM" --live
  $BIN/qbo-local status --db $DB --config .local/config.toml | tee "$RUN/status-after-sync.log"
}
step_fixtures() {
  log "7. record + scrub fixtures (commit happens after a human-or-Claude read of every file)"
  rm -rf .local/fixtures/raw .local/fixtures/scrubbed .local/record.db .local/replay-check.db
  $BIN/qbo-local init --db .local/record.db --realm "$REALM" --name "Aquamentor, Inc."
  $BIN/qbo-local init --db .local/replay-check.db --realm "$REALM" --name "Aquamentor, Inc."
  timed record $BIN/qbo-local record --db .local/record.db --realm "$REALM" --dir .local/fixtures/raw --live
  timed scrub python3 tools/scrub-fixtures.py .local/fixtures/raw .local/fixtures/scrubbed
  timed replay $BIN/qbo-local replay --db .local/replay-check.db --realm "$REALM" --dir .local/fixtures/scrubbed
}
step_ledger() {
  log "8. ledger first run + tbdiff baseline (do not chase variances)"
  $BIN/ledger init --db $LDB --company aquamentor --name "Aquamentor, Inc." --realm "$REALM"
  timed ledger-import $BIN/ledger import --db $LDB --company aquamentor --replica $DB --realm "$REALM"
  timed ledger-tb $BIN/ledger tb --db $LDB --company aquamentor --as-of "$TODAY"
  timed qbo-tb $BIN/qbo-local report --realm "$REALM" --name trial-balance --as-of "$TODAY" --out .local/qbo-tb.csv --live
  timed tbdiff $BIN/ledger tbdiff --db $LDB --company aquamentor --as-of "$TODAY" --qbo-csv .local/qbo-tb.csv || true
}
step_daemon() {
  log "9. daemon left running; CDC cursor + quarantine checked at +10 min"
  $BIN/qbo-local status --db $DB | tee "$RUN/status-t0.log"
  nohup $BIN/qbo-local daemon --db $DB --realm "$REALM" --live \
    --snapshot-dir .local/snapshots-nightly --keep 7 >>"$RUN/daemon.log" 2>&1 &
  echo $! > "$RUN/daemon.pid"; log "daemon pid $(cat "$RUN/daemon.pid")"
  sleep 600
  $BIN/qbo-local status --db $DB | tee "$RUN/status-t10.log"
  kill -0 "$(cat "$RUN/daemon.pid")" && log "daemon still running"
}
ALL="build keychain tokens sync fixtures ledger daemon"
for s in ${@:-$ALL}; do "step_$s"; done
log "done: $*"
