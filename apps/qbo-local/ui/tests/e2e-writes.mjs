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
await page.waitForSelector("#bb-items option", { state: "attached" });

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

// A purchase order to a vendor: expense lines, no tax column, no customer PO.
await page.goto(`${base}/#/r/${realm}/new/PurchaseOrder`);
await page.waitForSelector(".editor");
// Pickers fill asynchronously; choose only once the lists are there.
await page.waitForSelector("#bb-customers option", { state: "attached" });
await page.waitForSelector("#bb-items option", { state: "attached" });
assert.equal(await page.locator("text=Customer PO").count(), 0);
const vendor = await page.$eval("#bb-customers option", (o) => o.value);
await page.fill('input[list="bb-customers"]', vendor);
await page.fill('[aria-label="Line 1 product"]', firstItem);
await page.press('[aria-label="Line 1 product"]', "Tab");
await page.fill('[aria-label="Line 1 quantity"]', "40");
await page.fill('[aria-label="Line 1 rate"]', "18.4375");
assert.equal((await page.textContent(".edit-lines tbody tr td:nth-child(6)")).trim(), "737.50");
await page.keyboard.press("Control+s");
await page.waitForSelector(".banner");
await page.waitForSelector(".banner", { state: "detached", timeout: 30_000 });
assert.match(await page.textContent(".facts"), /737\.50/);

await page.goto(`${base}/#/r/${realm}/writes`);
await page.waitForSelector("text=Nothing waiting");
// Manufacturing: a new material, a delivery, a build sheet, a run.
await page.goto(`${base}/#/r/${realm}/materials`);
await page.waitForSelector(".grid tbody tr");
await page.click("text=New material");
const material = `Test EVA ${Date.now() % 100000}`;
await page.fill('[aria-label="Material name"]', material);
await page.fill('[aria-label="Thickness in inches"]', "1");
await page.click("text=Add material");
await page.waitForSelector(`text=${material}`);
await page.click("text=Record a delivery");
await page.selectOption('[aria-label="Line 1 material"]', { label: material });
await page.fill('[aria-label="Line 1 sheets"]', "10");
await page.fill('[aria-label="Line 1 price per sheet"]', "60");
await page.fill('[aria-label="Freight"]', "32");
await page.click('button.primary:has-text("Record delivery")');
// 10 sheets × 32 bf = 320 bf; (600 + 32) / 320 = 1.975
await page.waitForSelector(".inline-form", { state: "detached" });
const row = page.locator(".grid tbody tr", { hasText: material }).first();
await page.waitForFunction((m) => [...document.querySelectorAll(".grid tbody tr")].some((r) => r.textContent.includes(m) && r.textContent.includes("1.9750")), material);
assert.match(await row.textContent(), /1\.9750/);

// Stock: a reorder point and a count in sheets. 10 sheets were received;
// counting 9 leaves 288 bf, under a 300 bf reorder point, and the missing
// sheet is priced at the average.
await page.click(`[aria-label="Set reorder point for ${material}"]`);
await page.fill(`[aria-label="Reorder point for ${material}"]`, "300");
await page.press(`[aria-label="Reorder point for ${material}"]`, "Enter");
await page.locator(".grid tbody tr", { hasText: material }).first().locator("text=Count").click();
await page.fill('[aria-label="Counted amount"]', "9");
await page.click('button.primary:has-text("Record count")');
await page.waitForFunction((m) => [...document.querySelectorAll(".grid tbody tr")].some((r) => r.textContent.includes(m) && r.textContent.includes("Low") && r.textContent.includes("288")), material);
await page.locator(".grid tbody tr", { hasText: material }).first().locator("text=History").click();
await page.waitForSelector("text=History ·");
assert.match(await page.textContent("main"), /Count/);

await page.goto(`${base}/#/r/${realm}/sheets`);
await page.waitForSelector(".grid tbody tr a");
await page.selectOption('[aria-label="Material cost basis"]', "average");
await page.waitForTimeout(300);
await page.selectOption('[aria-label="Material cost basis"]', "latest");
await page.click(".grid tbody tr a >> nth=0");
await page.waitForSelector(".components");
await page.click("text=Add material");
const last = (await page.locator(".components tbody tr").count());
await page.selectOption(`[aria-label="Component ${last} material"]`, { label: material });
await page.fill(`[aria-label="Component ${last} net board-feet"]`, "1");
await page.fill(`[aria-label="Component ${last} yield"]`, "80");
await page.click("text=Save build sheet");
// Saved and re-costed: the new line buys 1 / 0.8 bf.
await page.waitForFunction(() => document.querySelector(".components")?.textContent.includes("buys 1.25 bf"));
await page.click("text=Record a build");
await page.fill('[aria-label="Units built"]', "10");
await page.click('button.primary:has-text("Record build")');
await page.waitForSelector(".inline-form", { state: "detached" });
await page.waitForFunction(() => [...document.querySelectorAll(".grid tbody tr td:nth-child(2)")].some((td) => td.textContent.trim() === "10"));

// Import: preview, an error by line, then a clean import, all or nothing.
await page.goto(`${base}/#/r/${realm}/import`);
await page.waitForSelector("text=Import from a spreadsheet");
await page.fill('[aria-label="CSV text"]', "name,thickness,width,length\nNeoprene 0.5 in,0.5,48,96\nBroken,0,48,96\n");
await page.click('button:has-text("Preview")');
await page.waitForSelector("text=Fix these rows first");
assert.match(await page.textContent(".grid tbody"), /3.*more than zero/s);
await page.fill('[aria-label="CSV text"]', "name,thickness,width,length\nNeoprene 0.5 in,0.5,48,96\n");
await page.click('button:has-text("Preview")');
await page.waitForSelector("text=Ready to import 1 rows");
await page.click('button.primary:has-text("Import")');
await page.waitForSelector("text=Imported 1 rows");
await page.selectOption('[aria-label="Import kind"]', "counts");
await page.fill('[aria-label="CSV text"]', "what,date,amount,unit\nRT-RED-000,2026-10-31,40,units\n");
await page.click('button:has-text("Preview")');
await page.waitForSelector("text=Ready to import 1 rows");
await page.click('button.primary:has-text("Import")');
await page.waitForSelector("text=Imported 1 rows");

// Receive payment: from an open invoice, a cheque spread oldest first.
await page.goto(`${base}/#/r/${realm}/list/Invoice`);
await page.waitForSelector(".grid tbody tr[data-index]");
await page.check("text=Open only");
await page.waitForSelector(".grid tbody tr[data-index]");
await page.dblclick(".grid tbody tr[data-index]");
await page.click('a:has-text("Receive payment")');
await page.waitForSelector("h1:has-text('Receive payment')");
await page.waitForSelector('[aria-label^="Payment on invoice"]');
await page.fill('[aria-label="Amount received"]', "10");
await page.waitForFunction(() => document.querySelector(".facts")?.textContent.includes("Applied"));
const applied = await page.$$eval('[aria-label^="Payment on invoice"]', (inputs) => inputs.map((i) => i.value).filter(Boolean));
assert.deepEqual(applied, ["10.00"], "ten dollars goes on the oldest invoice");
await page.getByLabel("Cheque or reference no.").fill("4417");
await shot("receive-payment");
await page.keyboard.press("Control+s");
await page.waitForSelector(".document .banner");
assert.match(await page.textContent(".lines tbody"), /Applied to invoice/);

// Pay bills: from an open bill, paid in full from the checking account.
await page.goto(`${base}/#/r/${realm}/list/Bill`);
await page.waitForSelector(".grid tbody tr[data-index]");
await page.check("text=Open only");
await page.waitForSelector(".grid tbody tr[data-index]");
await page.dblclick(".grid tbody tr[data-index]");
await page.click('a:has-text("Pay bills")');
await page.waitForSelector("h1:has-text('Pay bills')");
await page.waitForSelector('[aria-label^="Payment on bill"]');
await page.waitForFunction(() => document.querySelector('[aria-label="Paid from"]')?.selectedOptions[0]?.textContent.includes("Checking"));
const owed = (await page.textContent(".lines tbody tr td:nth-child(4)")).trim().split(/\s/)[0].replace(/,/g, "");
await page.fill('[aria-label="Amount paid"]', owed);
await page.waitForFunction(() => document.querySelector(".facts")?.textContent.includes("Paid from"));
await shot("pay-bills");
await page.keyboard.press("Control+s");
await page.waitForSelector(".document .banner");
assert.match(await page.textContent(".lines tbody"), /Applied to bill/);

// Make a deposit: two cheques waiting in Undeposited Funds, banked together.
await page.goto(`${base}/#/r/${realm}/list/Deposit`);
await page.click('a:has-text("Make a deposit")');
await page.waitForSelector("h1:has-text('Make a deposit')");
await page.waitForSelector('tbody [aria-label^="Deposit "]');
const boxes = page.locator('tbody [aria-label^="Deposit "]');
await boxes.nth(0).check();
await boxes.nth(1).check();
assert.match(await page.textContent(".lines tfoot"), /2 ticked/);
await shot("deposit");
await page.keyboard.press("Control+s");
await page.waitForSelector(".document .banner");
assert.match(await page.textContent(".lines tbody"), /Payment from/);

// An expense on the card: two lines to two expense accounts.
await page.goto(`${base}/#/r/${realm}/list/Purchase`);
await page.click('a:has-text("New expense")');
await page.waitForSelector("h1:has-text('New expense')");
await page.waitForSelector('[aria-label="Line 1 account"] option:nth-child(2)', { state: "attached" });
await page.selectOption('[aria-label="Line 1 account"]', { label: "Shipping and Freight" });
await page.fill('[aria-label="Line 1 description"]', "LTL to Garwood");
await page.fill('[aria-label="Line 1 amount"]', "412.50");
await page.click("text=Add a line");
await page.selectOption('[aria-label="Line 2 account"]', { label: "Shop Supplies" });
await page.fill('[aria-label="Line 2 amount"]', "38");
assert.match(await page.textContent(".edit-lines tfoot"), /450\.50/);
await shot("expense");
await page.keyboard.press("Control+s");
await page.waitForSelector(".document .banner");
assert.match(await page.textContent(".lines tbody"), /Shipping and Freight: LTL to Garwood/);

assert.deepEqual(errors, []);
await browser.close();
console.log("e2e-writes: ok");
