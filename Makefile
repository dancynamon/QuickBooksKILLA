# Convenience targets; the real steps live in tools/desktop.sh.
.PHONY: app desktop desktop-selftest

# Unsigned BunzBooks.app under apps/desktop/src-tauri/target/release/bundle/macos/
app:
	tools/desktop.sh --bundle

# Build (debug) and run the desktop app
desktop:
	tools/desktop.sh

# Launch, have the webview call both commands, print SELFTEST, quit
desktop-selftest:
	tools/desktop.sh --selftest
