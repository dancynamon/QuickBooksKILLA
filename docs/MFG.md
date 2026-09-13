# Manufacturing costing

`LEDGER-DESIGN.md` §11, `DECISIONS.md` D20 (W2, W3, W4, W12). This is the
how-to and the JSON shapes; the policy is in the design document, this file
is the operator's guide. Code: `apps/ledger/src/mfg.rs` and its `landed`,
`sheet`, `build` and `report` submodules.

Built after cutover (ROADMAP §H) — schema and posting effects only, no UI.
Everything below runs through `ledger mfg ...` today.

The idea, straight from the prototype: cost flows from the sheet to the
finished product automatically. Reprice a foam sheet and every product built
from it re-prices with it, because nothing here caches a cost — every
`ledger mfg sheet cost` reads each material's *current* average.

## Why `--class` shows up on `build` and `adjust`

§1's Notes column says a Build's class comes "from the finished item" and an
Inventory adjustment's is "required, from the item." This ledger has no item
catalogue table of its own yet — `PostingContext.items` is assembled
externally, from the QBO replica, for the importer — so there is nothing
here to look an item's default class up in. Until an item table exists,
`class` is a required field on both request shapes below rather than a
lookup.

## Worked example: a 50" rescue tube

Invented figures, used throughout this file and in the crate's tests. A
rescue tube cut from 2 lb XLPE foam sheet at 78% nesting yield, one bought
towline, six minutes of shop time.

| | |
|---|---|
| Sheet | XLPE 2 lb, blue, 4x8x2", 64 board feet per sheet |
| Sheet cost, landed | $228.00 base + $10.80 freight = $238.80 (§ "Landed cost" below shows the freight allocated across a mixed pallet) |
| Landed per board foot | $238.80 / 64 = $3.73/bf |
| Recipe | 2.6 bf of foam per tube, one 6' towline (a bought part, $6.20) |
| Yield | 78% — to ship 2.6 bf you buy 2.6 / 0.78 = 3.33 bf |
| Labour | 6 minutes at $48.00/hour shop rate |

## 1. A material

```
ledger mfg material add --db LEDGER.sqlite --company aquamentor --file material.json
```

```json
{
  "material_id": "FOAM-XLPE-BLU",
  "name": "XLPE 2lb sheet, blue, 4x8x2\"",
  "unit": "sheet",
  "sheet_board_feet": "64",
  "source_ref": null
}
```

`unit` is one of `"bf"`, `"sheet"`, `"ea"`, `"lf"` — how the material is
**bought**. A `bf` or `sheet` material is always **tracked and costed by the
board foot** regardless (a `sheet` material's receipt quantity converts
through `sheet_board_feet` on the way in, once, so every build sheet reads a
single $/bf number rather than a mix of sheets and board feet). `sheet_board_feet`
is required for `"sheet"` and meaningless otherwise. A material starts at
zero cost and zero quantity — it starts to mean something the first time it
is received.

```
ledger mfg material list --db LEDGER.sqlite --company aquamentor
```

## 2. Receiving it: landed cost, freight by board foot

A sheet costs what it cost plus the freight that brought it, and freight on
a mixed pallet is allocated **by board foot**, never evenly across lines and
never by value — splitting a mixed pallet evenly makes the cheaper material
look dearer and every margin downstream inherits the error. `ea`/`lf` lines
(hardware, webbing) allocate by their own unit count only when nothing on
the receipt has board feet at all; otherwise they get zero, since a bag of
buckles does not absorb foam's freight.

```
ledger mfg receive --db LEDGER.sqlite --company aquamentor --file receipt.json
```

```json
{
  "vendor_ref": "Continental Foam",
  "po_ref": "PO-4469",
  "received_on": "2026-08-14",
  "lines": [
    { "material_id": "FOAM-XLPE-BLU", "qty": "2", "base_cost": 45600 },
    { "material_id": "FOAM-XLPE-RED", "qty": "1", "base_cost": 24800 }
  ],
  "freight": 3240
}
```

`qty` is a decimal string in the material's own bought unit (here, sheets).
Money (`base_cost`, `freight`) is minor units — cents — as everywhere else in
this ledger. This receipt allocates $32.40 of freight across 128 bf of blue
and 64 bf of red (2:1 by board foot, so $21.60/$10.80), lands the blue sheet
at ($456.00 + $21.60) / 128 bf = $3.73/bf, and posts:

| Dr 1300 | Cr 2050 |
|---|---|
| $477.60 + $258.80 = $736.40 | $736.40 |

Each material's moving weighted average (W12) recomputes on the spot:
`(old_qty × old_avg + qty × landed_per_unit) / (old_qty + qty)`, rounded to
the cent. Receiving the same material twice at different landed costs moves
the average toward the new receipt without discarding what was on the floor
before it.

## 3. A build sheet

Yield **divides**, it does not subtract: to ship 2.6 bf of finished part at
78% nesting yield you must buy 2.6 / 0.78 = 3.33 bf, and you paid for all of
it. The waste — 0.73 bf here — is a column every costing view carries
(`StandardCost::material_gross_bf`), not a footnote.

```
ledger mfg sheet add --db LEDGER.sqlite --company aquamentor --file sheet.json
```

```json
{
  "sheet_id": "XRT-50-STD",
  "item_id": "XRT-50-STD",
  "version": 1,
  "name": "50in rescue tube",
  "yield_pct": "0.78",
  "labour_minutes": "6",
  "labour_rate_minor_per_hour": 4800,
  "overhead_minor": 150,
  "lines": [
    {
      "material_id": "FOAM-XLPE-BLU",
      "qty_per_unit": "2.6",
      "unit": "bf",
      "note": "round blank from rectangular sheet"
    },
    {
      "part_item_id": "TOWLINE-6FT",
      "qty_per_unit": "1",
      "unit": "ea"
    }
  ]
}
```

Each line names exactly one of `material_id` (a `bf` line, yield-divided,
priced off `materials`) or `part_item_id` (an `ea` line, bought at its own
average, never yield-divided — a bought part is simply a material stocked
`ea`, so `part_item_id` resolves against the same `materials` table). Adding
a sheet with a `sheet_id` that already exists replaces its lines wholesale —
a new recipe version is a deliberate, whole-sheet edit, never a line patch.

```
ledger mfg sheet list --db LEDGER.sqlite --company aquamentor
```

### Standard cost and sensitivity

```
ledger mfg sheet cost --db LEDGER.sqlite --company aquamentor --sheet XRT-50-STD --price 49.95
```

Prints the standard cost — material (yield-divided, at each material's
current average), labour, overhead, total — and, with `--price`, the margin
at that price plus the sensitivity table: foam price ±5%, nesting yield ±5
points, labour ±10%, each against the sheet as it stands today. On this
recipe, five points of yield is worth more than a five percent foam
discount — the sensitivity table is what makes that a row Dan reads rather
than arithmetic he has to do himself.

## 4. Completing a build

What was actually cut against what the sheet said, priced. Every
consumption is priced at its material's *current* average (and reduces
`qty_on_hand`); the difference between the sheet's standard material cost
for the quantity built and what was actually consumed plugs to 5100 (W4) —
positive credits it (a good nest), negative debits it (an overrun). Labour
and overhead post at standard only: what varies build to build, the thing
worth watching, is the nest.

```
ledger mfg build --db LEDGER.sqlite --company aquamentor --file build.json
```

```json
{
  "sheet_id": "XRT-50-STD",
  "qty_built": "35",
  "consumptions": [["FOAM-XLPE-BLU", "101.6"]],
  "labour_minutes_actual": "210",
  "completed_on": "2026-08-19",
  "class": "foam",
  "note": "BLD-0912, order SO-2041"
}
```

`consumptions` is a list of `[material_id, qty_actual]` pairs — every id
must be a component of `sheet_id` (a build cannot consume a material its own
recipe never named). Thirty-five tubes at 2.6/0.78 = 3.33 bf standard is
116.7 bf expected; this build actually used 101.6 bf, a good nest, so 5100
is **credited**.

Every posted `builds` row keeps `standard_cost_minor`, `actual_material_minor`
and `variance_minor` — `ledger mfg variance` reads them back.

## 5. A physical count

```
ledger mfg adjust --db LEDGER.sqlite --company aquamentor \
    --account 1300 --amount -12.50 --class foam \
    --date 2026-08-31 --reason "August physical count, foam room"
```

`--account` is `1300` or `1310`. `--amount` is signed — positive is a count
**up** (Dr the inventory account, Cr 5150), negative a count **down** (Dr
5150, Cr the inventory account). A reason is required on every line, per §1.

## 6. Reports

```
ledger mfg variance --db LEDGER.sqlite --company aquamentor --from 2026-08-01 --to 2026-08-31
ledger mfg onhand   --db LEDGER.sqlite --company aquamentor
```

`variance` groups every build completed in the range by item, worst
variance percentage first — one overrun is noise, the same product every
time is a recipe that is lying. `onhand` values `materials` at average, next
to 1300's own trial-balance reading; the two must agree to the cent after
every receipt and every build, by construction, and the report says so.

## JSON field reference

Money fields are minor units (cents), integers, matching every other posted
document in this ledger. Decimal fields (`qty`, `yield_pct`,
`labour_minutes`, `qty_per_unit`, `qty_built`, quantities inside
`consumptions`) are JSON strings, e.g. `"0.78"`, matching the ledger's
"decimals as text" discipline all the way through to the JSON boundary.
Dates are `"YYYY-MM-DD"`.
