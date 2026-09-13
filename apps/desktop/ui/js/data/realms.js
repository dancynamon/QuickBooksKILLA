// The realm switcher's choices. `DESIGN.md`: "Two companies... switching
// realms swaps the whole book. They share nothing." There is no MCP tool to
// discover this list — `docs/MCP.md`'s eleven tools all take a `realm_id`
// they assume the caller already knows, the same way a real installation is
// pointed at specific companies by whoever set up `QBO_LOCAL_DB` rather than
// asking the replica what it contains. So this is static configuration, not
// a query result, and it is the one place both providers are told which
// realms exist.
//
// Names and ids here are invented, same as `prototype/bunzbooks.html`
// (`prototype/README.md`: "invent nothing real") — not the operator's real
// companies, which never belong in this repository.

export const REALMS = Object.freeze([
  { id: "1234567890123456", name: "Harborworks, Inc." },
  { id: "9876543210123456", name: "Tidewater CNC Works" },
]);

export const DEFAULT_REALM_ID = REALMS[0].id;

export function realmName(id) {
  return REALMS.find((r) => r.id === id)?.name ?? id;
}
