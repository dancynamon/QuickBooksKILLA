// A hash router. No history API, no framework — `location.hash` is the
// whole state (`#view/arg`), which is enough for a desktop shell with no
// back/forward affordance in its own chrome, and it means a route can be
// pasted, bookmarked in dev tools, or driven from a test without a DOM.

function parseHash(hash) {
  const raw = (hash || "").replace(/^#\/?/, "");
  if (!raw) return { view: "today", arg: null };
  const slash = raw.indexOf("/");
  if (slash === -1) return { view: raw, arg: null };
  return { view: raw.slice(0, slash), arg: decodeURIComponent(raw.slice(slash + 1)) };
}

function formatHash(view, arg) {
  if (arg == null || arg === "") return `#${view}`;
  return `#${view}/${encodeURIComponent(arg)}`;
}

/**
 * Create a router bound to `window.location.hash`. `onRoute(route)` fires
 * once immediately with the current route, then again on every navigation
 * — a screen's render function is `onRoute` itself, or is called from it.
 * @param {(route: { view: string, arg: string | null }) => void} onRoute
 */
export function createRouter(onRoute) {
  function emit() {
    onRoute(parseHash(window.location.hash));
  }

  window.addEventListener("hashchange", emit);

  return {
    /** Navigate to a view, optionally with one string argument. Re-emits
     * even when the hash is unchanged (a "refresh this screen" request —
     * `hashchange` alone would not fire for that). */
    go(view, arg) {
      const next = formatHash(view, arg);
      if (window.location.hash === next) emit();
      else window.location.hash = next;
    },
    /** Read the current route without waiting for a navigation. */
    current() {
      return parseHash(window.location.hash);
    },
    /** Fire `onRoute` once for whatever the hash already is — call after
     * wiring up screens, so the page one lands on renders immediately. */
    start() {
      emit();
    },
  };
}

export { parseHash, formatHash };
