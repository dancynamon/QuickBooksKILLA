// Run with `npm test`: node's built-in runner, stripping types natively.
import { test } from "node:test";
import assert from "node:assert/strict";
import * as mod from "../src/format.ts";

test("money is exact from minor units", () => {
  assert.equal(mod.formatMoney(0), "0.00");
  assert.equal(mod.formatMoney(5), "0.05");
  assert.equal(mod.formatMoney(123456789), "1,234,567.89");
  assert.equal(mod.formatMoney(-1999), "−19.99");
  assert.equal(mod.formatMoney(null), "");
  // 0.1 + 0.2 territory: no float, no drift.
  assert.equal(mod.formatMoney(30), "0.30");
});

test("decimals keep precision past two places", () => {
  assert.equal(mod.formatDecimal("49.2500"), "49.25");
  assert.equal(mod.formatDecimal("18.4375"), "18.4375");
  assert.equal(mod.formatDecimal("40"), "40");
  assert.equal(mod.formatDecimal("3.5"), "3.50");
});

test("dates render US style", () => {
  assert.equal(mod.formatDate("2026-07-14"), "07/14/2026");
});
