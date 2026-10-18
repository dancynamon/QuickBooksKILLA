// The write path end to end in a browser, against `qbo-local serve
// --demo-writes` on a fresh synthetic book (writes land in a stand-in
// QuickBooks after a few seconds):
//
//   cargo run --release --bin qbo-local -- demo --db /tmp/demo.db
//   cargo run --release --bin qbo-local -- serve --db /tmp/demo.db --demo-writes &
//   node tests/e2e-writes.mjs [screenshot-dir]
import assert from "node:assert/strict";

const base = process.env.BASE_URL ?? "http://127.0.0.1:8780";
const realm = "9130000000000001";
const shots = process.argv[2];
let chromium;
try {
  ({ chromium } = await import(process.env.PLAYWRIGHT_MODULE ?? "playwright"));
} catch {
  console.error("playwright not found; set PLAYWRIGHT_MODULE to its index.mjs");
  process.exit(2);
}
const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1400, height: 900 } });
const errors = [];
page.on("pageerror", (e) => errors.push(e.message));
const shot = async (name) => shots && (await page.screenshot({ path: `${shots}/${name}.png` }));

await page.goto(`${base}/#/r/${realm}/list/Invoice`);
await page.waitForSelector(".grid tbody tr[data-index]");
await page.click("text=New invoice");
await page.waitForSelector(".editor");

// A customer that does not exist yet, added inline.
const name = `Gull Pond Swim Team ${Date.now() % 100000}`;
await page.fill('input[list="bb-customers"]', name);
await page.click(`text=Add “${name}” as a new customer`);
await page.click("text=Add customer");
await page.waitForSelector(".inline-form", { state: "detached" });

// One line from the product list; the amount is computed as typed.
const firstItem = await page.$eval("#bb-items option", (o) => o.value);
await page.fill('[aria-label="Line 1 product"]', firstItem);
await page.press('[aria-label="Line 1 product"]', "Tab");
await page.fill('[aria-label="Line 1 quantity"]', "3");
await page.fill('[aria-label="Line 1 rate"]', "11.50");
assert.equal((await page.textContent(".edit-lines tbody tr td:nth-child(6)")).trim(), "34.50");
await shot("editor");
await page.keyboard.press("Control+s");

// Saved locally: the document shows at once, honestly marked.
await page.waitForSelector(".document .banner");
assert.match(await page.textContent(".banner"), /Not yet in QuickBooks/);
await shot("saved-pending");

// The sync queue shows the invoice waiting on its new customer, or both landing.
await page.goto(`${base}/#/r/${realm}/writes`);
await page.waitForSelector("h1");
await shot("queue");

// It lands: the register row turns into a real QuickBooks invoice.
await page.goto(`${base}/#/r/${realm}/list/Invoice`);
const started = Date.now();
for (;;) {
  await page.waitForSelector(".grid tbody tr[data-index]");
  const pending = await page.locator(".grid tbody tr .badge.warn", { hasText: "Not yet in QuickBooks" }).count();
  if (pending === 0) break;
  assert.ok(Date.now() - started < 30_000, "writes landed within 30 s");
  await page.waitForTimeout(1500);
  await page.reload();
}
console.log(`landed in ${((Date.now() - started) / 1000).toFixed(1)} s`);
await page.fill(".filter", name);
await page.waitForTimeout(100);
assert.equal(await page.locator(".grid tbody tr[data-index]").count(), 1);
await page.click(".grid tbody tr[data-index] td:nth-child(2) a");
await page.waitForSelector(".document .facts");
assert.equal(await page.locator(".banner").count(), 0, "no banner once it has landed");
assert.match(await page.textContent(".facts"), /34\.50/);

// Edit it: a sparse update, queued, then landed.
await page.click("a.button:has-text('Edit')");
await page.waitForSelector(".editor");
await page.fill("textarea >> nth=1", "Ship with the next pallet");
await page.keyboard.press("Control+s");
await page.waitForSelector(".banner");
assert.match(await page.textContent(".banner"), /on its way/);
await page.waitForSelector(".banner", { state: "detached", timeout: 30_000 });
assert.match(await page.textContent(".note"), /Ship with the next pallet/);
await shot("edited");

await page.goto(`${base}/#/r/${realm}/writes`);
await page.waitForSelector("text=Nothing waiting");
assert.deepEqual(errors, []);
await browser.close();
console.log("e2e-writes: ok");
