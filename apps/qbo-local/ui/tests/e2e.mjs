// End-to-end smoke against a running `qbo-local serve` on the synthetic book:
//
//   cargo run --release --bin qbo-local -- demo --db /tmp/demo.db --invoices 9000
//   cargo run --release --bin qbo-local -- serve --db /tmp/demo.db &
//   node tests/e2e.mjs
//
// Needs Playwright installed somewhere resolvable (it is not a dependency, so
// `npm install` never downloads a browser). Exits non-zero on any failure.
import assert from "node:assert/strict";

const base = process.env.BASE_URL ?? "http://127.0.0.1:8780";
const realm = "9130000000000001";
let chromium;
try {
  ({ chromium } = await import(process.env.PLAYWRIGHT_MODULE ?? "playwright"));
} catch {
  console.error("playwright not found; set PLAYWRIGHT_MODULE to its index.mjs");
  process.exit(2);
}

const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1400, height: 860 } });
const errors = [];
page.on("pageerror", (e) => errors.push(e.message));
page.on("console", (m) => m.type() === "error" && errors.push(m.text()));

const started = Date.now();
await page.goto(`${base}/`);
await page.waitForSelector(".grid tbody tr[data-index]");
console.log(`cold load to register: ${Date.now() - started} ms`);
assert.match(await page.textContent(".toolbar .muted"), /^9,000 of 9,000/);

// Virtualized: thousands of rows, a few dozen in the DOM.
assert.ok((await page.locator(".grid tbody tr[data-index]").count()) < 120);
for (let i = 0; i < 50; i++) await page.keyboard.press("ArrowDown");
assert.equal(await page.getAttribute(".grid tbody tr.cursor", "data-index"), "50");

// A paid invoice shows its whole chain.
await page.check("label.check input");
await page.uncheck("label.check input");
await page.click('th:has-text("Open")');
await page.click('th:has-text("Open")');
await page.focus(".filter");
await page.keyboard.press("ArrowDown");
await page.keyboard.press("Enter");
await page.waitForSelector(".document .facts");
assert.ok((await page.locator(".chain li").count()) >= 2, "paid invoice links to its payment");

// Palette: a number lands on the document and says why.
await page.keyboard.press("Control+k");
await page.fill(".palette input", "9005");
await page.waitForSelector(".hits li .reason");
assert.equal(await page.textContent(".hits li .reason"), "number");
await page.keyboard.press("Enter");
await page.waitForURL(/\/doc\//);

// Contact view from a register link.
await page.goto(`${base}/#/r/${realm}/list/Bill`);
await page.waitForSelector(".grid tbody tr[data-index]");
await page.click(".grid tbody tr[data-index] td:nth-child(3) a");
await page.waitForURL(/\/contact\/Vendor\//);

// Reports: aging buckets add up to the total.
await page.goto(`${base}/#/r/${realm}/report/ar`);
await page.waitForSelector(".grid tfoot td");
const foot = await page.$$eval(".grid tfoot td.num", (cells) => cells.map((c) => Math.round(Number(c.textContent.replace(/,/g, "")) * 100)));
assert.equal(foot.slice(0, 5).reduce((a, b) => a + b, 0), foot[5], "A/R buckets sum to total");

// Items: a product page lists where it was used.
await page.goto(`${base}/#/r/${realm}/items`);
await page.waitForSelector(".grid tbody tr a");
await page.click(".grid tbody tr:first-child a");
await page.waitForSelector(".facts");
assert.ok((await page.locator(".grid tbody tr").count()) > 0);

// Phone width: no horizontal page scroll.
await page.setViewportSize({ width: 390, height: 800 });
assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), true);

assert.deepEqual(errors, []);
await browser.close();
console.log("e2e: ok");
