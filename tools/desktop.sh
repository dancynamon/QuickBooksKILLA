#!/usr/bin/env bash
# Build and open the BunzBooks desktop app (apps/desktop/src-tauri).
#
#   tools/desktop.sh            build (debug) and run it
#   tools/desktop.sh --selftest run it, have the webview call both commands,
#                               print the SELFTEST line, quit after 15s; uses a
#                               throwaway ledger, never the scratch book
#   tools/desktop.sh --bundle   build the unsigned release .app and print its path
#   tools/desktop.sh --open     open that .app (building it first if missing)
#
# Environment (all optional; apps/desktop/README.md):
#   QBO_LOCAL_DB    replica, opened read-only. Default: <main checkout>/.local/replica.db
#   LEDGER_DB       ledger book. Default: the scratch book under
#                   ~/Library/Application Support/QuickBooksKILLA/
#   LEDGER_COMPANY  default aquamentor
set -euo pipefail

export PATH="$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"

here="$(cd "$(dirname "$0")/.." && pwd)"
crate="$here/apps/desktop/src-tauri"
app="$crate/target/release/bundle/macos/BunzBooks.app"

# The replica lives in the main checkout's gitignored .local/, which a
# worktree does not have — find the main checkout through git's common dir.
if [[ -z "${QBO_LOCAL_DB:-}" ]]; then
  common="$(git -C "$here" rev-parse --path-format=absolute --git-common-dir)"
  export QBO_LOCAL_DB="$(dirname "$common")/.local/replica.db"
fi

ensure_tauri_cli() {
  cargo tauri --version >/dev/null 2>&1 || cargo install tauri-cli --version "^2" --locked
}

build_bundle() {
  ensure_tauri_cli
  (cd "$crate" && cargo tauri build --bundles app)
  echo "$app"
}

case "${1:-}" in
  "")
    (cd "$crate" && cargo build)
    exec "$crate/target/debug/bunzbooks-desktop"
    ;;
  --selftest)
    (cd "$crate" && cargo build)
    tmp="$(mktemp -d)"
    LEDGER_DB="$tmp/selftest-ledger.sqlite" BUNZBOOKS_SELFTEST=1 \
      BUNZBOOKS_SELFTEST_EXIT_AFTER="${BUNZBOOKS_SELFTEST_EXIT_AFTER:-15}" \
      "$crate/target/debug/bunzbooks-desktop"
    ;;
  --bundle)
    build_bundle
    ;;
  --open)
    [[ -d "$app" ]] || build_bundle
    # `open` does not pass this shell's environment to the app; the app's
    # own defaults apply unless LEDGER_DB/QBO_LOCAL_DB are passed with --env.
    args=(--env "QBO_LOCAL_DB=$QBO_LOCAL_DB")
    [[ -n "${LEDGER_DB:-}" ]] && args+=(--env "LEDGER_DB=$LEDGER_DB")
    [[ -n "${LEDGER_COMPANY:-}" ]] && args+=(--env "LEDGER_COMPANY=$LEDGER_COMPANY")
    open "${args[@]}" "$app"
    ;;
  *)
    sed -n '2,15p' "$0"; exit 2
    ;;
esac
