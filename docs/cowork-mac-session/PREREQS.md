# Prerequisites — have these before opening the session

- [ ] **Intuit developer app** client id and client secret. From
      https://developer.intuit.com, the app that `qbo_headless` already uses if
      one exists; otherwise a new app with the Accounting scope.
- [ ] **Redirect URI** registered on that app: `http://localhost:8765/callback`
      (the port is configurable; it must match exactly).
- [ ] **Realm id** for Aquamentor. It is the `deeplinkcompanyid` parameter on
      any QBO invoice URL, and it is stored wherever `qbo_headless` keeps its
      authorisation (`~/code/aquamentor-mcp`). It goes in `.local/config.toml`
      inside the clone (template: `.local/config.example.toml`), never in a
      tracked file. The client secret goes in the `QBO_CLIENT_SECRET`
      environment variable, not the file.
- [ ] **Does `qbo_headless` hold a valid refresh token?** If yes, the consent
      flow can be skipped by importing it. Check before the session so the
      session does not build a flow it does not need.
- [ ] **Sandbox company** exists on the developer account. Not needed for M0
      (read-only against production is the plan) but needed the moment any
      write path is exercised.
- [ ] Rust toolchain on the Mac: `rustup`, stable. Xcode command line tools for
      `security-framework`. Node 20+ for the UI. `cargo install tauri-cli` for
      the desktop shell.
