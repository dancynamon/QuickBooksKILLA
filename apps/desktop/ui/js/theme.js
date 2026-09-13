// Text size and light/dark controls. `prototype/README.md`: "Adjustable
// text" and "Light and dark" both persist between sessions where the
// browser allows storage, and both stay reachable from the chrome buttons
// carried over from the prototype (`A-`/`A+`, `☀`/`☾`).

const FS_MIN = 0.9;
const FS_MAX = 1.7;
const FS_STEP = 0.1;
const FS_KEY = "bunzbooks.fs";
const THEME_KEY = "bunzbooks.theme"; // "light" | "dark" | "system"

/** Read a value out of localStorage, tolerating a browser that refuses it
 * (private window, blocked site data) rather than throwing. */
function readStorage(key) {
  try {
    return window.localStorage.getItem(key);
  } catch {
    return null;
  }
}

function writeStorage(key, value) {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    // Best-effort. A viewer without storage still gets a working session —
    // it just forgets its size/theme choice on reload.
  }
}

function clampFs(n) {
  return Math.min(FS_MAX, Math.max(FS_MIN, Math.round(n * 10) / 10));
}

/**
 * Wire the text-size and theme controls in the chrome to `document`'s
 * `--fs` custom property and `data-theme` attribute, restoring whatever was
 * saved last time. Returns the current font-scale getter so the status bar
 * can show it without re-reading localStorage itself.
 */
export function initTheme({ onFsChange } = {}) {
  const root = document.documentElement;

  let fs = clampFs(Number(readStorage(FS_KEY)) || 1);
  let theme = readStorage(THEME_KEY) || "system";

  function applyFs() {
    root.style.setProperty("--fs", String(fs));
    onFsChange?.(fs);
  }

  function applyTheme() {
    if (theme === "system") {
      root.removeAttribute("data-theme");
    } else {
      root.setAttribute("data-theme", theme);
    }
    document.getElementById("lightBtn")?.setAttribute("aria-pressed", String(theme === "light"));
    document.getElementById("darkBtn")?.setAttribute("aria-pressed", String(theme === "dark"));
  }

  function setFs(next) {
    fs = clampFs(next);
    writeStorage(FS_KEY, String(fs));
    applyFs();
  }

  function setTheme(next) {
    theme = next;
    writeStorage(THEME_KEY, theme);
    applyTheme();
  }

  document.getElementById("fsDown")?.addEventListener("click", () => setFs(fs - FS_STEP));
  document.getElementById("fsUp")?.addEventListener("click", () => setFs(fs + FS_STEP));
  document.getElementById("lightBtn")?.addEventListener("click", () => setTheme(theme === "light" ? "system" : "light"));
  document.getElementById("darkBtn")?.addEventListener("click", () => setTheme(theme === "dark" ? "system" : "dark"));

  applyFs();
  applyTheme();

  return {
    getFs: () => fs,
    setFs,
    getTheme: () => theme,
    setTheme,
  };
}
