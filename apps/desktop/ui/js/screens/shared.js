// Small helpers every screen uses: the sticky view header, loading/empty/
// error states for an async provider call, and the derived status tags the
// prototype computes from a document's own fields rather than storing them
// (`prototype/README.md`: "PO status is derived from the line quantities
// rather than stored, so the two can never disagree" — the same principle
// applies to a document's paid/open state here).

import { esc, plain } from "../format.js";

export function viewHead(title, sub, actionsHtml) {
  return `<div class="view-head">
    <h1 class="view-title">${esc(title)}</h1>
    <span class="view-sub">${sub ? esc(sub) : ""}</span>
    <span class="spacer"></span>${actionsHtml || ""}
  </div>`;
}

export function loadingState(main, label) {
  main.innerHTML = `<div class="state-note">Loading ${esc(label)}…</div>`;
}

export function errorState(main, err) {
  main.innerHTML = `<div class="state-note is-error">Could not load this screen: ${esc(err?.message ?? err)}</div>`;
}

/**
 * Run `load()` and hand its result to `draw()`, showing a loading state
 * first and an error state if the provider call rejects — the one pattern
 * every screen's render function follows, so a live `invoke` failure (a
 * bad realm, a replica that isn't there) renders as a message rather than
 * a blank screen or an uncaught rejection.
 */
export async function withState(main, label, load, draw) {
  loadingState(main, label);
  let result;
  try {
    result = await load();
  } catch (err) {
    errorState(main, err);
    return;
  }
  draw(result);
}

/**
 * A document's paid/open state, derived from `balance` against `total` —
 * never stored, so it can't drift from the numbers it's read off (the same
 * reasoning `query.rs`'s `open_documents` doc comment gives for deriving
 * PO/Estimate openness from `doc_status` and `balance` rather than a
 * separate flag).
 */
export function documentStatusTag(doc) {
  if (doc.balance == null) return `<span class="tag">${esc(doc.doc_status ?? "—")}</span>`;
  const balance = Number(doc.balance);
  const total = Number(doc.total);
  if (balance <= 0) return `<span class="tag ok">paid</span>`;
  if (total > 0 && balance < total) return `<span class="tag warn">partial</span>`;
  return `<span class="tag">open</span>`;
}

export function classCell(classId, classMap) {
  const cls = classId ? classMap.get(classId) : null;
  if (!cls) return `<span class="dim">—</span>`;
  return `<span class="dim">${esc(cls.name)}</span>`;
}

export async function loadClassMap(provider) {
  const classes = await provider.classTree();
  return new Map(classes.map((c) => [c.qbo_id, c]));
}

/** A right-aligned money cell, dashing out a `null`/zero balance the way
 * the prototype's grids do rather than printing "$0.00" into every paid
 * row's balance column. */
export function balanceCell(decimal) {
  const value = plain(decimal, { dashForZero: true });
  return value === "—" ? `<span class="dim">—</span>` : value;
}
