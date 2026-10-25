#!/usr/bin/env bash
# The nightly parallel run, packaged (docs/NIGHTLY.md, "Switching it on").
#
#   tools/nightly.sh run                 one night: check, snapshot, replay, diff, report
#   tools/nightly.sh plists              print both launchd plists + launchctl commands
#   tools/nightly.sh install --dry-run   the same as `plists`
#   tools/nightly.sh install             Dan's step: write the plists and load both jobs
#   tools/nightly.sh uninstall [--dry-run]
#   tools/nightly.sh status              launchd jobs, daemon, cursor age, last night
#
# `run`, in order: the qbo-local daemon is alive; `qbo-local snapshot` of the
# replica; the snapshot's oldest CDC cursor is fresh; QBO's trial balance and
# AR/AP aging as of yesterday; `ledger nightly` (with the configured --from)
# replaying the snapshot into the own book; the ledger's own aging; then
# tools/nightly-verdict.py writes REPORT_DIR/nightly-<as-of>.md and .json and
# keeps the newest KEEP nights. Exit 0 green, 1 unexplained variance or a
# rejected document, 3 daemon down or replica stale, 4 a step failed, 2 bad
# config. Any non-zero exit posts one macOS notification; a green night is
# silent.
#
# Reads the live replica only through `qbo-local snapshot`; everything after
# reads the snapshot. Never starts, stops or signals the daemon. Settings:
# .local/nightly.conf (copy tools/nightly.example.conf), or $NIGHTLY_CONFIG.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
VERDICT="$ROOT/tools/nightly-verdict.py"

die() { echo "nightly: $*" >&2; exit 2; }

# -- config -------------------------------------------------------------------

load_config() {
  NIGHTLY_CONFIG=${NIGHTLY_CONFIG:-"$ROOT/.local/nightly.conf"}
  if [ -f "$NIGHTLY_CONFIG" ]; then
    # shellcheck source=/dev/null
    . "$NIGHTLY_CONFIG"
  fi
  NIGHTLY_CONFIG=$(cd "$(dirname "$NIGHTLY_CONFIG")" && pwd)/$(basename "$NIGHTLY_CONFIG")
  BIN_DIR=${BIN_DIR:-"$ROOT/target/release"}
  QBO_LOCAL=${QBO_LOCAL:-"$BIN_DIR/qbo-local"}
  LEDGER=${LEDGER:-"$BIN_DIR/ledger"}
  LEDGER_AGING=${LEDGER_AGING:-"$BIN_DIR/ledger-aging"}
  QBO_CONFIG=${QBO_CONFIG:-"$ROOT/.local/config.toml"}
  COMPANY=${COMPANY:-aquamentor}
  LEDGER_DB=${LEDGER_DB:-"$ROOT/.local/ledger.db"}
  REPLICA=${REPLICA:-"$ROOT/.local/replica.db"}
  FROM=${FROM-2027-01-01}
  REPORT_DIR=${REPORT_DIR:-"$HOME/Library/Application Support/QuickBooksKILLA/nightly"}
  KEEP=${KEEP:-90}
  SNAPSHOT_DIR=${SNAPSHOT_DIR:-"$REPORT_DIR/snapshots"}
  SNAPSHOT_KEEP=${SNAPSHOT_KEEP:-7}
  TBDIFF_DIR=${TBDIFF_DIR:-"$REPORT_DIR/tbdiff"}
  LOG_DIR=${LOG_DIR:-"$REPORT_DIR/logs"}
  MAX_CURSOR_AGE_MIN=${MAX_CURSOR_AGE_MIN:-360}
  EXPLAINED=${EXPLAINED:-"$ROOT/.local/nightly-explained.csv"}
  DECISIONS=${DECISIONS:-"$ROOT/DECISIONS.md"}
  DAEMON_PATTERN=${DAEMON_PATTERN:-"qbo-local daemon"}
  QBO_REPORT_SOURCE=${QBO_REPORT_SOURCE:---live}
  NOTIFY=${NOTIFY:-1}
  NOTIFY_CMD=${NOTIFY_CMD:-}
  DAEMON_LABEL=${DAEMON_LABEL:-com.aquamentor.qbo-local}
  NIGHTLY_LABEL=${NIGHTLY_LABEL:-com.aquamentor.nightly}
  LAUNCH_AGENTS=${LAUNCH_AGENTS:-"$HOME/Library/LaunchAgents"}
  DAEMON_SNAPSHOT_DIR=${DAEMON_SNAPSHOT_DIR:-"$ROOT/.local/snapshots-nightly"}
  DAEMON_SNAPSHOT_KEEP=${DAEMON_SNAPSHOT_KEEP:-7}
  HOUR=${HOUR:-2}
  MINUTE=${MINUTE:-30}
  if [ -z "${REALM:-}" ] && [ -f "$QBO_CONFIG" ]; then
    REALM=$(awk -F'"' '/^\[\[realms\]\]/{r=1} r && /^id *=/{print $2; exit}' "$QBO_CONFIG")
  fi
  REALM=${REALM:-}
}

yesterday() {
  date -v-1d +%F 2>/dev/null || date -d yesterday +%F
}

# -- run ----------------------------------------------------------------------

notify() {
  local title=$1 message=$2
  [ "$NOTIFY" = 1 ] || return 0
  if [ -n "$NOTIFY_CMD" ]; then
    "$NOTIFY_CMD" "$title" "$message" || true
  else
    osascript -e 'on run argv' \
      -e 'display notification (item 2 of argv) with title (item 1 of argv) sound name "Basso"' \
      -e 'end run' "$title" "$message" >/dev/null 2>&1 || true
  fi
}

# step NAME LOG CMD... — run CMD with its output in LOG, append a line to
# steps.tsv, and return CMD's exit code. A caller that knows better (an exit 1
# that is a red night, not a failure) records its own line with record_step.
STEP_STATUS=""
record_step() {  # name status exit seconds log [detail]
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" "$5" "${6:-}" >>"$RUN_DIR/steps.tsv"
}
step() {
  local log=$2 t0 rc; shift 2
  t0=$(date +%s)
  set +e; "$@" >"$log" 2>&1; rc=$?; set -e
  STEP_SECONDS=$(( $(date +%s) - t0 ))
  if [ "$rc" = 0 ]; then STEP_STATUS=ok; else STEP_STATUS=failed; fi
  return "$rc"
}
step_recorded() {  # like step, and records the line itself
  local name=$1 log=$2 rc=0
  step "$@" || rc=$?
  record_step "$name" "$STEP_STATUS" "$rc" "$STEP_SECONDS" "$log" "$(tail -n 1 "$log" 2>/dev/null | tr '\t' ' ')"
  return "$rc"
}

FINISHED=0
finish() {
  local rc=0 summary
  FINISHED=1
  summary=$(python3 "$VERDICT" finish --run-dir "$RUN_DIR" --report-dir "$REPORT_DIR" \
    --as-of "$AS_OF" --from "$FROM" --company "$COMPANY" --explained "$EXPLAINED" \
    --decisions "$DECISIONS" --keep "$KEEP" 2>&1) || rc=$?
  echo "$summary"
  if [ "$rc" != 0 ]; then
    local first
    first=$(printf '%s\n' "$summary" | sed -n '2s/^  - //p')
    case $rc in
      1|3|4) ;;
      *) first="verdict step crashed (exit $rc): ${first:-see $LOG_DIR}"; rc=4 ;;
    esac
    notify "Nightly $AS_OF: RED (exit $rc)" "${first:-see $REPORT_DIR/nightly-$AS_OF.md}"
  fi
  release_lock
  exit "$rc"
}

LOCK=""
take_lock() {
  LOCK="$REPORT_DIR/.nightly.lock"
  if ! mkdir "$LOCK" 2>/dev/null; then
    local holder
    holder=$(cat "$LOCK/pid" 2>/dev/null || true)
    if [ -n "$holder" ] && kill -0 "$holder" 2>/dev/null; then
      echo "nightly: another run (pid $holder) holds $LOCK" >&2
      exit 4
    fi
    rm -rf "$LOCK"; mkdir "$LOCK"
  fi
  echo $$ >"$LOCK/pid"
}
release_lock() { if [ -n "$LOCK" ]; then rm -rf "$LOCK"; fi; LOCK=""; }

cmd_run() {
  load_config
  [ -n "$REALM" ] || die "no REALM: set it in $NIGHTLY_CONFIG or a [[realms]] id in $QBO_CONFIG"
  AS_OF=${AS_OF:-$(yesterday)}
  mkdir -p "$REPORT_DIR" "$SNAPSHOT_DIR" "$TBDIFF_DIR" "$LOG_DIR"
  take_lock
  RUN_DIR="$REPORT_DIR/runs/$AS_OF"
  rm -rf "$RUN_DIR"; mkdir -p "$RUN_DIR"
  : >"$RUN_DIR/steps.tsv"
  printf '{"started_at": "%s"}\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"$RUN_DIR/run.json"
  echo "nightly $AS_OF: run files in $RUN_DIR"
  # Anything that stops the script before the verdict (an unexpected error
  # under set -e) still ends in a report and a notification.
  trap 'rc=$?; if [ "$FINISHED" = 0 ]; then record_step script failed "$rc" 0 "-" "nightly.sh stopped before the verdict"; finish; fi' EXIT

  # 0. Everything the run reads exists. A missing file is a failed night with
  # a report and a notification, not a silent exit.
  local missing="" path
  for path in "$QBO_LOCAL" "$LEDGER" "$LEDGER_AGING"; do [ -x "$path" ] || missing="$missing $path"; done
  for path in "$REPLICA" "$LEDGER_DB"; do [ -f "$path" ] || missing="$missing $path"; done
  if [ -n "$missing" ]; then
    record_step config failed 2 0 "-" "missing:$missing"
    finish
  fi
  record_step config ok 0 0 "-" "$NIGHTLY_CONFIG"

  # 1. The daemon is alive.
  local pids
  pids=$(pgrep -f "$DAEMON_PATTERN" | tr '\n' ' ' | sed 's/ *$//' || true)
  if [ -n "$pids" ]; then
    printf '{"alive": true, "pids": [%s], "pattern": "%s"}\n' "${pids// /, }" "$DAEMON_PATTERN" >"$RUN_DIR/daemon.json"
    record_step daemon ok 0 0 "-" "pid $pids"
  else
    printf '{"alive": false, "pids": [], "pattern": "%s"}\n' "$DAEMON_PATTERN" >"$RUN_DIR/daemon.json"
    record_step daemon down 1 0 "-" "no process matches '$DAEMON_PATTERN'"
    finish
  fi

  # 2. Snapshot the replica: the only read of the live file.
  step_recorded snapshot "$RUN_DIR/snapshot.log" \
    "$QBO_LOCAL" snapshot --db "$REPLICA" --dir "$SNAPSHOT_DIR" --keep "$SNAPSHOT_KEEP" || finish
  SNAPSHOT=$(sed -n 's/^snapshot *: *//p' "$RUN_DIR/snapshot.log" | head -n 1)
  if [ -z "$SNAPSHOT" ] || [ ! -f "$SNAPSHOT" ]; then
    record_step snapshot-file failed 1 0 "$RUN_DIR/snapshot.log" "qbo-local snapshot named no file"
    finish
  fi

  # 3. Its CDC cursor is fresh.
  local rc=0
  step fresh "$RUN_DIR/freshness.log" python3 "$VERDICT" freshness --replica "$SNAPSHOT" \
    --realm "$REALM" --max-age-min "$MAX_CURSOR_AGE_MIN" --out "$RUN_DIR/freshness.json" || rc=$?
  case $rc in
    0) record_step freshness ok 0 "$STEP_SECONDS" "$RUN_DIR/freshness.log" "$(cat "$RUN_DIR/freshness.log")" ;;
    3) record_step freshness stale 3 "$STEP_SECONDS" "$RUN_DIR/freshness.log" "$(cat "$RUN_DIR/freshness.log")"; finish ;;
    *) record_step freshness failed "$rc" "$STEP_SECONDS" "$RUN_DIR/freshness.log"; finish ;;
  esac

  # 4. QBO's side, as of the same date.
  local qbo_source=()
  read -r -a qbo_source <<<"$QBO_REPORT_SOURCE"
  if [ "${qbo_source[0]:-}" = "--live" ]; then qbo_source+=(--config "$QBO_CONFIG"); fi
  local tb_ok=1
  step_recorded qbo-tb "$RUN_DIR/qbo-tb.log" "$QBO_LOCAL" report --realm "$REALM" \
    --name trial-balance --as-of "$AS_OF" --out "$RUN_DIR/qbo-tb.csv" "${qbo_source[@]}" || tb_ok=0
  step_recorded qbo-ar "$RUN_DIR/qbo-ar.log" "$QBO_LOCAL" report --realm "$REALM" \
    --name ar-aging --as-of "$AS_OF" --out "$RUN_DIR/qbo-ar.csv" "${qbo_source[@]}" || true
  step_recorded qbo-ap "$RUN_DIR/qbo-ap.log" "$QBO_LOCAL" report --realm "$REALM" \
    --name ap-aging --as-of "$AS_OF" --out "$RUN_DIR/qbo-ap.csv" "${qbo_source[@]}" || true

  # 5. Replay into the own book and diff the trial balance.
  if [ "$tb_ok" = 1 ]; then
    local window=() out="$RUN_DIR/ledger-nightly.out" csv="$TBDIFF_DIR/tbdiff-$AS_OF.csv"
    [ -n "$FROM" ] && window=(--from "$FROM")
    # A rerun for the same date: set the earlier diff aside, so the check
    # below sees only a file this run wrote (bash 3.2's -nt is whole seconds).
    [ -f "$csv" ] && mv "$csv" "$csv.prev"
    rc=0
    step ledger-nightly "$out" "$LEDGER" nightly --db "$LEDGER_DB" --company "$COMPANY" \
      --replica "$SNAPSHOT" --realm "$REALM" --qbo-csv "$RUN_DIR/qbo-tb.csv" \
      --out "$TBDIFF_DIR" ${window[@]+"${window[@]}"} --as-of "$AS_OF" || rc=$?
    # Exit 1 is a red night only if it got as far as the verdict and a fresh
    # diff; the same 1 with no verdict line is a runtime error.
    if { [ "$rc" = 0 ] || [ "$rc" = 1 ]; } && grep -q '^nightly: ' "$out" && [ -f "$csv" ]; then
      rm -f "$csv.prev"
      cp "$csv" "$RUN_DIR/tbdiff.csv"
      cp "$TBDIFF_DIR/tbdiff-$AS_OF.txt" "$RUN_DIR/tbdiff.txt" 2>/dev/null || true
      record_step ledger-nightly ok "$rc" "$STEP_SECONDS" "$out" "$(grep '^nightly: ' "$out" | head -n 1)"
    else
      if [ -f "$csv.prev" ] && [ ! -f "$csv" ]; then mv "$csv.prev" "$csv"; fi
      record_step ledger-nightly failed "$rc" "$STEP_SECONDS" "$out" "$(tail -n 1 "$out" | tr '\t' ' ')"
    fi
  else
    record_step ledger-nightly skipped 0 0 "-" "no QBO trial balance to diff against"
  fi

  # 6. The ledger's aging, in QBO's CSV shape.
  if step_recorded ledger-aging "$RUN_DIR/ledger-aging.log" "$LEDGER_AGING" --db "$LEDGER_DB" \
      --company "$COMPANY" --as-of "$AS_OF" --out "$RUN_DIR/ledger-aging"; then
    mv "$RUN_DIR/ledger-aging/ar-$AS_OF.csv" "$RUN_DIR/ledger-ar.csv"
    mv "$RUN_DIR/ledger-aging/ap-$AS_OF.csv" "$RUN_DIR/ledger-ap.csv"
    rmdir "$RUN_DIR/ledger-aging"
  fi

  # 7. The verdict.
  finish
}

# -- launchd ------------------------------------------------------------------

xml_escape() { sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g' <<<"$1"; }

render() {  # template -> stdout, every @NAME@ from the variable NAME
  local template=$1 name value
  local text
  text=$(cat "$template")
  for name in DAEMON_LABEL NIGHTLY_LABEL QBO_LOCAL REPLICA REALM QBO_CONFIG ROOT LOG_DIR \
      NIGHTLY_CONFIG HOUR MINUTE DAEMON_SNAPSHOT_DIR DAEMON_SNAPSHOT_KEEP; do
    value=$(xml_escape "${!name}")
    text=${text//"@$name@"/"$value"}
  done
  if grep -q '@[A-Z_]*@' <<<"$text"; then
    die "unfilled placeholder in $template: $(grep -o '@[A-Z_]*@' <<<"$text" | head -n 1)"
  fi
  printf '%s\n' "$text"
}

plist_paths() {
  DAEMON_PLIST="$LAUNCH_AGENTS/$DAEMON_LABEL.plist"
  NIGHTLY_PLIST="$LAUNCH_AGENTS/$NIGHTLY_LABEL.plist"
  DOMAIN="gui/$(id -u)"
}

loaded() { launchctl print "$DOMAIN/$1" >/dev/null 2>&1; }

# Daemon processes launchd did not start: a hand-run daemon. Installing the
# KeepAlive job beside one would put two writers on one replica.
hand_run_daemons() {
  local pids launchd_pid=""
  pids=$(pgrep -f "$DAEMON_PATTERN" || true)
  if loaded "$DAEMON_LABEL"; then
    launchd_pid=$(launchctl print "$DOMAIN/$DAEMON_LABEL" 2>/dev/null | awk '/^\tpid = /{print $3}')
  fi
  for pid in $pids; do [ "$pid" = "$launchd_pid" ] || printf '%s ' "$pid"; done
}

preflight() {
  local problems=0 path
  for path in "$QBO_LOCAL" "$LEDGER" "$LEDGER_AGING"; do
    [ -x "$path" ] || { echo "  ! missing $path (cargo build --release)"; problems=1; }
  done
  [ -f "$QBO_CONFIG" ] || { echo "  ! missing $QBO_CONFIG"; problems=1; }
  [ -f "$REPLICA" ] || { echo "  ! missing replica $REPLICA"; problems=1; }
  [ -f "$LEDGER_DB" ] || { echo "  ! missing ledger $LEDGER_DB"; problems=1; }
  [ -n "$REALM" ] || { echo "  ! no REALM"; problems=1; }
  local hand
  hand=$(hand_run_daemons)
  if [ -n "$hand" ]; then
    echo "  ! a hand-run qbo-local daemon is running (pid ${hand% }): stop it first (kill ${hand% })"
    echo "    — launchd's KeepAlive copy would otherwise be a second writer on the replica"
    problems=1
  fi
  return "$problems"
}

print_commands() {
  cat <<EOF
# The commands \`tools/nightly.sh install\` runs, in order:
mkdir -p "$LAUNCH_AGENTS" "$LOG_DIR" "$REPORT_DIR"
# write the two plists above to:
#   $DAEMON_PLIST
#   $NIGHTLY_PLIST
plutil -lint "$DAEMON_PLIST" "$NIGHTLY_PLIST"
launchctl bootout $DOMAIN/$DAEMON_LABEL 2>/dev/null || true   # only if already loaded
launchctl bootout $DOMAIN/$NIGHTLY_LABEL 2>/dev/null || true  # only if already loaded
launchctl bootstrap $DOMAIN "$DAEMON_PLIST"
launchctl bootstrap $DOMAIN "$NIGHTLY_PLIST"
launchctl print $DOMAIN/$DAEMON_LABEL | grep -E 'state|pid'
# then, to see one night now rather than at $(printf '%02d:%02d' "$HOUR" "$MINUTE"):
tools/nightly.sh run
EOF
}

confirm_dan() {  # what
  if [ ! -t 0 ] || [ ! -t 1 ]; then
    die "$1 changes this Mac's launchd jobs and is Dan's step: run it yourself in a terminal (use --dry-run to see what it would do)"
  fi
  local answer
  read -r -p "$1: type yes to go ahead: " answer
  [ "$answer" = yes ] || die "not confirmed; nothing changed"
}

cmd_install() {
  load_config; plist_paths
  local dry=0
  [ "${1:-}" = "--dry-run" ] && dry=1
  local daemon_xml nightly_xml
  daemon_xml=$(render "$ROOT/launchd/qbo-local-daemon.plist.in")
  nightly_xml=$(render "$ROOT/launchd/nightly.plist.in")
  if [ "$dry" = 1 ]; then
    echo "# ---- $DAEMON_PLIST"
    echo "$daemon_xml"
    echo
    echo "# ---- $NIGHTLY_PLIST"
    echo "$nightly_xml"
    echo
    print_commands
    echo
    echo "# preflight:"
    if preflight; then echo "  ok — nothing in the way"; fi
    return 0
  fi
  echo "preflight:"
  preflight || die "fix the above first; nothing changed"
  echo "  ok"
  confirm_dan "install $DAEMON_LABEL and $NIGHTLY_LABEL into $DOMAIN"
  mkdir -p "$LAUNCH_AGENTS" "$LOG_DIR" "$REPORT_DIR"
  printf '%s\n' "$daemon_xml" >"$DAEMON_PLIST"
  printf '%s\n' "$nightly_xml" >"$NIGHTLY_PLIST"
  plutil -lint "$DAEMON_PLIST" "$NIGHTLY_PLIST"
  local label plist
  for label in "$DAEMON_LABEL" "$NIGHTLY_LABEL"; do
    plist="$LAUNCH_AGENTS/$label.plist"
    loaded "$label" && launchctl bootout "$DOMAIN/$label"
    launchctl bootstrap "$DOMAIN" "$plist"
    echo "loaded $label"
  done
  echo "installed. Next: tools/nightly.sh status, then tools/nightly.sh run to see a night now."
}

cmd_uninstall() {
  load_config; plist_paths
  local archive label plist stamp
  archive="$REPORT_DIR/launchd-archive"
  stamp=$(date +%Y%m%d-%H%M%S)
  if [ "${1:-}" = "--dry-run" ]; then
    for label in "$NIGHTLY_LABEL" "$DAEMON_LABEL"; do
      echo "launchctl bootout $DOMAIN/$label"
      echo "mv \"$LAUNCH_AGENTS/$label.plist\" \"$archive/$label.$stamp.plist\""
    done
    echo "# the daemon stops with its job; the replica, ledger and reports stay."
    return 0
  fi
  confirm_dan "unload $NIGHTLY_LABEL and $DAEMON_LABEL (the daemon stops)"
  mkdir -p "$archive"
  for label in "$NIGHTLY_LABEL" "$DAEMON_LABEL"; do
    plist="$LAUNCH_AGENTS/$label.plist"
    if loaded "$label"; then launchctl bootout "$DOMAIN/$label"; echo "unloaded $label"; fi
    if [ -f "$plist" ]; then mv "$plist" "$archive/$label.$stamp.plist"; echo "archived $plist"; fi
  done
}

cmd_status() {
  load_config; plist_paths
  local bad=0 label info
  echo "launchd ($DOMAIN):"
  for label in "$DAEMON_LABEL" "$NIGHTLY_LABEL"; do
    if info=$(launchctl print "$DOMAIN/$label" 2>/dev/null); then
      echo "  $label: loaded, $(awk -F' = ' '/^\tstate = /{s=$2} /^\tpid = /{p=" pid "$2} /last exit code = /{e=", last exit "$2} END{print s p e}' <<<"$info")"
    else
      echo "  $label: not loaded"; bad=1
    fi
  done
  local pids
  pids=$(pgrep -f "$DAEMON_PATTERN" | tr '\n' ' ' || true)
  if [ -n "$pids" ]; then echo "daemon: running (pid ${pids% })"; else echo "daemon: NOT RUNNING"; bad=1; fi
  if [ -f "$REPLICA" ] && [ -n "$REALM" ]; then
    local tmp
    tmp=$(mktemp)
    printf 'replica: '
    python3 "$VERDICT" freshness --replica "$REPLICA" --realm "$REALM" \
      --max-age-min "$MAX_CURSOR_AGE_MIN" --out "$tmp" || bad=1
    rm -f "$tmp"
  else
    echo "replica: $REPLICA not found or no REALM"; bad=1
  fi
  local last
  last=$(find "$REPORT_DIR" -maxdepth 1 -name 'nightly-*.json' 2>/dev/null | sort | tail -n 1)
  if [ -n "$last" ]; then
    python3 - "$last" <<'EOF' || bad=1
import json, sys
r = json.load(open(sys.argv[1]))
print(f"last night: {r['as_of']} {r['verdict'].upper()} (exit {r['exit_code']}), finished {r['finished_at']}")
for reason in r["reasons"]:
    print(f"  - {reason}")
print(f"  {sys.argv[1][:-5]}.md")
sys.exit(0 if r["verdict"] == "green" else 1)
EOF
  else
    echo "last night: no report yet in $REPORT_DIR"
  fi
  return "$bad"
}

case "${1:-}" in
  run) shift; cmd_run "$@" ;;
  plists) shift; cmd_install --dry-run ;;
  install) shift; cmd_install "$@" ;;
  uninstall) shift; cmd_uninstall "$@" ;;
  status) shift; cmd_status "$@" ;;
  *) sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
