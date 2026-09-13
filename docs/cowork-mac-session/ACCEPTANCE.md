# Acceptance — M0 on the Mac

From `HANDOFF.md` §5, Aquamentor only:

- [ ] Aquamentor fully mirrored, complete history, actual row counts reported
- [ ] Initial sync wall-clock reported (target under 10 minutes, measured)
- [ ] CDC poll keeps the replica current (cursor advances, quarantine stays 0)
- [ ] Token survives an app restart
- [ ] Token survives a 24-hour idle, including a rotation (follow-up check)
- [ ] `cargo test --workspace` still fully offline and green
- [ ] Writes still disabled for every realm

Added by the cloud session:

- [ ] The four ⚠️ API facts verified and recorded (a DECISIONS entry)
- [ ] Scrubbed fixtures committed; the suite replays them
- [ ] `ledger import` has run once against the real replica and the first
      `tbdiff` report is recorded verbatim as the parallel-run baseline
- [ ] `docs/MCP.md`: `qbo-local-mcp` registered in Claude Desktop against the
      live replica and one read skill repointed for a smoke test
