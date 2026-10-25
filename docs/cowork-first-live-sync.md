TIER: Sonnet 5.5. Mechanical setup; decided upstream, do not re-classify.

TASK: Do the first live QuickBooks sync for BunzBooks (repo dancynamon/QuickBooksKILLA, branch ccr-3daf8637-ra3gev) on this Mac. Drive everything you can yourself. Stop only at the gates below.

TAB: bunzbooks live sync (color green)
FOLDER: Claude Tasks and Projects/BunzBooks Live Sync. This holds the report only. Never put the replica DB or any key there.

HARD RULES
- Never let the Intuit Client Secret, an access token or a refresh token appear in chat, a screenshot you keep, a file, or a command line. Dan pastes the keys into a hidden prompt himself (step 4).
- Never put the replica in Dropbox. The app home is ~/Library/Application Support/qbo-local.
- Read-only only. Never run `writes --enable`.
- Do not create a new Intuit account, accept paid terms, or submit anything to Intuit without asking Dan first.

STEP 1 — Mac prerequisites (Terminal, no gate)
- If `cargo` is missing, run `curl https://sh.rustup.rs -sSf | sh -s -- -y`, then source ~/.cargo/env.
- If `node` 18+ is missing, install it with Homebrew: `brew install node`.
- If ~/Code/QuickBooksKILLA is missing, run `git clone https://github.com/dancynamon/QuickBooksKILLA.git ~/Code/QuickBooksKILLA`. Then run `git -C ~/Code/QuickBooksKILLA fetch origin ccr-3daf8637-ra3gev && git -C ~/Code/QuickBooksKILLA checkout ccr-3daf8637-ra3gev && git -C ~/Code/QuickBooksKILLA pull`.
- Start the build now so it runs while you work in Chrome: `cd ~/Code/QuickBooksKILLA && cargo build --release --features live --bin qbo-local`.

STEP 2 — Intuit app (Chrome, developer.intuit.com, signed in as Dan)
- If an app named BunzBooks already exists, reuse it. Otherwise: Dashboard → Create an app → "QuickBooks Online and Payments" → name "BunzBooks" → scope Accounting only.
- Keys & credentials:
  - Add Redirect URI `http://localhost:8765/callback` under Development (sandbox).
  - Add the same Redirect URI under Production. Save both.
- Production keys require Intuit's questionnaire. Fill in every field:
  - App URL, privacy policy and terms URLs: use https://aquamentor.com pages. If no privacy or terms page exists, use https://aquamentor.com and FLAG it.
  - Internal use only, single company, US.
  - Hosting: desktop app on the owner's Mac.
  - GATE A: show Dan the filled answers and wait for "submit". Submitting is outward-facing.
- Do not copy, read aloud or screenshot the Client ID or Secret. Leave the Production keys page open for Dan.

STEP 3 — Write the helper script (no secrets in it)
Create ~/bin/bunzbooks-sync.sh with mode 700 and this content:

    #!/bin/bash
    set -euo pipefail
    cd ~/Code/QuickBooksKILLA
    BIN=./target/release/qbo-local
    SVC=bunzbooks-intuit-app
    ENVFLAG="${1:-}"   # pass --sandbox for the sandbox company
    get() { security find-generic-password -s "$SVC$ENVFLAG" -a "$1" -w 2>/dev/null || true; }
    QBO_CLIENT_ID="$(get client_id)"; QBO_CLIENT_SECRET="$(get client_secret)"
    if [ -z "$QBO_CLIENT_ID" ] || [ -z "$QBO_CLIENT_SECRET" ]; then
      read -rp "Paste Client ID: " QBO_CLIENT_ID
      read -rsp "Paste Client Secret (hidden): " QBO_CLIENT_SECRET; echo
      security add-generic-password -U -s "$SVC$ENVFLAG" -a client_id -w "$QBO_CLIENT_ID"
      security add-generic-password -U -s "$SVC$ENVFLAG" -a client_secret -w "$QBO_CLIENT_SECRET"
    fi
    export QBO_CLIENT_ID QBO_CLIENT_SECRET
    LOG="$HOME/Library/Application Support/qbo-local/first-sync$ENVFLAG.log"
    mkdir -p "$(dirname "$LOG")"
    if [ ! -f "$LOG.realm" ]; then
      $BIN authorize $ENVFLAG 2>&1 | tee "$LOG.authorize"
      sed -n 's/^authorised realm //p' "$LOG.authorize" > "$LOG.realm"
    fi
    REALM="$(cat "$LOG.realm")"
    echo "realm $REALM"
    /usr/bin/time -p $BIN sync --realm "$REALM" $ENVFLAG 2>&1 | tee "$LOG"
    $BIN reconcile --realm "$REALM" $ENVFLAG 2>&1 | tee -a "$LOG"
    echo "DONE"

Note: `security add-generic-password -w <value>` briefly puts the secret on argv. That is acceptable here only because it runs in Dan's own Terminal on his own Mac. If `security` instead prompts for the password interactively when -w is given last with no value, use that form.

STEP 4 — Dan's part (GATE B, his hands only)
- When the build from step 1 has finished, open the script in Terminal.app: `open -a Terminal ~/bin/bunzbooks-sync.sh`.
- Tell Dan in one line: "Copy the Production Client ID and Secret from the open Intuit tab into the Terminal prompts. Then Cmd-click the URL Terminal prints, approve Aquamentor, and click Always Allow on the keychain prompt."
- Watch the log file. Do not read the keychain.

STEP 5 — Verify and report
- When the log ends in DONE, read first-sync.log.
- Pull the matching counts from QuickBooks. Use the quickbooks/qbo-headless MCP, and check currentUrl and realmId per Dan's preferences. Get the number of customers, vendors, items, invoices, estimates, bills, purchase orders and payments.
- Compare each count with what the sync reported. Flag any mismatch with both numbers.
- Write BunzBooks Live Sync/first-sync-report.md in the task folder. Include: the realm, wall-clock time, a table of counts (synced vs QBO), any lines from reconcile, and every error line verbatim.
- If sync failed, quote the exact error. Do not retry more than once.

STEP 6 — Open the app (no gate)
    cd ~/Code/QuickBooksKILLA/apps/qbo-local/ui && npm install && npm run build
    cd ../../qbo-local-desktop && cargo install tauri-cli --version "^2" --locked && cargo tauri dev
Take one screenshot of the Invoices register with real data and save it to the task folder.

OPTIONAL — Sandbox (only if Dan says yes at GATE A)
Repeat step 4 with `~/bin/bunzbooks-sync.sh --sandbox`, using the Development keys.

FINAL OUTPUT (Dan's format)
DID / GATE / FLAGGED / BLOCKED lines. Include the report path and the realm ID. Paste the counts table and the verbatim sync summary lines so Dan can paste them into the BunzBooks build session.
