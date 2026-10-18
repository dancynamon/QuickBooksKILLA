#!/usr/bin/env bash
# Produce a scrubbed fixture from the live replica (HANDOFF.md §2.6, D26, D31).
# Reads the replica only (opened read-only); touches no network; deletes
# nothing. Every file it writes lands under .local/ (gitignored). Committing
# the result is a separate, human step after reading every file.
#
#   cd ~/code/QuickBooksKILLA && tools/make-real-fixture.sh
#   tools/make-real-fixture.sh --customers 5 --vendors 3 --since 2026-01-01
#
# Any flag is passed through to `qbo-local slice` (see `qbo-local --help`):
#   --customers N --vendors N --customer ID --vendor ID --since YYYY-MM-DD
#   --max-docs N --max-records N --pick-seed S
# Environment: DB (default .local/replica.db), SEED (default
#   .local/fixtures/scrub-seed), REALM (default: the first
# [[realms]] id in .local/config.toml).
#
# Steps, each logged to .local/fixtures/slice-<stamp>/:
#   1. qbo-local slice   -> raw/          (real data, plus raw/deny-names.txt)
#   2. scrub-fixtures.py -> scrubbed/     (amounts and names scrubbed with the
#                                          secret seed in .local/fixtures/scrub-seed,
#                                          created on first use; invariants
#                                          re-checked; leak detector must pass)
#   3. qbo-local replay  scrubbed/ into a throwaway replay-check.db, proving
#                                          the scrubbed set replays cleanly
set -euo pipefail
cd "$(dirname "$0")/.."

DB=${DB:-.local/replica.db}
if [ -z "${REALM:-}" ]; then
  REALM=$(awk -F'"' '/^\[\[realms\]\]/{r=1} r && /^id *=/{print $2; exit}' .local/config.toml)
fi
[ -n "$REALM" ] || { echo "no REALM given and no [[realms]] id in .local/config.toml" >&2; exit 2; }
[ -f "$DB" ] || { echo "no replica at $DB" >&2; exit 2; }

STAMP=$(date +%Y%m%d-%H%M%S)
WORK=.local/fixtures/slice-$STAMP
SEED=${SEED:-.local/fixtures/scrub-seed}
mkdir -p .local/fixtures "$WORK"

if [ ! -f "$SEED" ]; then
  # The seed keys every amount factor and fake name: keep it, never commit it.
  # The same seed re-scrubs a re-recorded slice to the same output.
  (umask 077; python3 -c 'import secrets; print(secrets.token_hex(32))' > "$SEED")
  echo "created a new scrub seed at $SEED (keep it; it is what makes re-runs reproducible)"
fi

cargo build --release --bin qbo-local
BIN=target/release/qbo-local

echo "== 1. slice $DB (realm from config) -> $WORK/raw"
$BIN slice --db "$DB" --realm "$REALM" --dir "$WORK/raw" "$@" 2>&1 | tee "$WORK/slice.log"

echo "== 2. scrub -> $WORK/scrubbed"
python3 tools/scrub-fixtures.py --seed-file "$SEED" "$WORK/raw" "$WORK/scrubbed" 2>&1 | tee "$WORK/scrub.log"

echo "== 3. replay check"
$BIN init --db "$WORK/replay-check.db" --realm 1234567890123456 --name "replay check" >/dev/null
$BIN replay --db "$WORK/replay-check.db" --realm 1234567890123456 --dir "$WORK/scrubbed" 2>&1 | tee "$WORK/replay.log"

cat <<EOF

Done. Scrubbed fixture: $WORK/scrubbed
Nothing has been committed. Before any of it goes near git:
  1. read every file under $WORK/scrubbed (grep it for anything you recognise);
  2. copy it to apps/qbo-local/tests/fixtures/real-slice/ and git add only that.
$WORK/raw is real data and stays under .local/.
EOF
