# Payroll: journaling SurePayroll pay runs

SurePayroll stays the payroll engine (`LEDGER-DESIGN.md` §12). It computes
the paychecks, pays employees, and files and remits the taxes. The own book
records one journal entry per pay run from SurePayroll's summary and
computes nothing itself. Until cutover (4/1/27, D24/D29) payroll comes into
the book through the nightly QBO replay. Each run from then on is imported
with the tools below. D38 has the reasoning.

## The entry

One SurePayroll run (one check date) becomes one `JournalEntry` document,
`payroll:<check date>:<run id>`, and one **flagged** entry (§4) dated the
check date:

| Leg | Account | Class |
|---|---|---|
| Dr cash earnings | the person's wages account, 5400 by default | the person's class |
| Dr employer taxes and employer deductions | the person's employer-tax account, which defaults to their wages account | the person's class |
| Cr withholdings, employee deductions, employer taxes, employer deductions | 2400 payroll liabilities | none |
| Cr net pay | 1100, or `bank_account` | none |

Debits are summed by account and class. Credits to 2400 are kept as one line
per SurePayroll item (`FED WTH`, `CO FICA`, …), so the 2400 detail lines up
with what SurePayroll later remits. The import never writes an employee name
or an individual's pay into the document or the entry; the book holds totals
by account and class.

When SurePayroll debits the bank to remit the taxes, that withdrawal clears
2400. `docs/BANK.md` already proposes 2400 for a `SUREPAYROLL` bank line.
SurePayroll's own service fee (`FEES` on the summary) is invoiced separately
and is entered as a bill or purchase. It is not part of the pay-run journal.

## What it refuses

The whole file is refused, and nothing is written, when:

- an employee is not mapped under `[payroll.employees]` and their
  department is not mapped under `[payroll.departments]`. The error lists
  every unmapped person, and no class is ever guessed;
- a stated `net_pay` does not equal that employee's earnings less
  withholdings and deductions, to the cent;
- net pay for the run comes out negative;
- a check date is on or before the locked-through date;
- the mapping names a class or account the book does not have.

## Input: the pay-run lines CSV

SurePayroll's Reports → **Payroll Summary** (pick the year and check date,
then Update Report) is the source. Its CSV/Excel/PDF exports are renderings
of that page rather than a data file. So the import reads a flat CSV with one
row per figure on the summary:

```
run_id,pay_date,period_start,period_end,employee,department,section,item,amount
```

| Column | Required | Notes |
|---|---|---|
| `run_id` | yes | SurePayroll's id for the run (the value behind the check-date selector), or any id that is stable for that run. Combined with `pay_date`, it is the dedupe key. |
| `pay_date` | yes | The check date. `YYYY-MM-DD` or `MM/DD/YYYY`. |
| `period_start`, `period_end` | no | Shown in the entry memo. |
| `employee` | yes | The key `[payroll.employees]` maps: the SurePayroll employee id is preferred, but the name as printed also works. |
| `department` | no | Used as a fallback key into `[payroll.departments]`. |
| `section` | yes | One of the values below. |
| `item` | yes | The SurePayroll item code exactly as printed. |
| `amount` | yes | The PERIOD column, not YTD. `$`, commas and `(negative)` are accepted, with at most two decimal places. |

How the Payroll Summary's columns map to `section`:

| Summary column | Items seen (NJ) | `section` |
|---|---|---|
| EARNINGS, cash | REGULAR, SALARY, O/TIME, BONUS$, VACTION | `earning` |
| EARNINGS marked non-cash | (whichever the asterisk footnote covers) | `noncash_earning`, which is read but not posted |
| EMPLOYEE TAXES | FED WTH, FICA, MEDFICA, STATE-NJ, NJ SUI EE, DISAB-NJ, NJ FLI, NJWFEE, NJSWF | `employee_tax` |
| EMPLOYEE DEDUCTIONS | 401(k), premium share, garnishment | `employee_deduction` |
| EMPLOYER TAXES | CO FICA, CO MEDC, FUTA, CO UNEM-NJ, ER SDI-NJ, NJWFER | `employer_tax` |
| EMPLOYER DEDUCTIONS | match, premium share | `employer_deduction` |
| NET PAY (per employee) | NET PAY | `net_pay`. Optional, but it is the cross-check, so include it. |

Leave out the `FEES` line and the TOTAL rows. The import only reads rows,
so a total would be counted twice.

One file can hold several runs. They are grouped by `(run_id, pay_date)`.

The MCP tool also takes the same data as JSON (`runs`). See
`docs/MCP.md`, `import_payroll`.

## Mapping: `.local/config.toml`

`.local/` is gitignored. The template, `.local/config.example.toml`, maps
nobody:

```toml
[payroll]
# bank_account = "1100"
# liabilities_account = "2400"
# wages_account = "5400"
# employer_tax_account = ""      # unset: each person's wages account

[payroll.employees]
"EMP-0001" = { class = "foam" }
"EMP-0003" = { class = "drop", wages_account = "6900" }

[payroll.departments]
"Shop" = { class = "foam" }
```

Lookup order is employee first, then department, then refuse. Each person
has exactly one class. Splitting one person's wage across classes is not
supported. The fallback is to give them a department that maps to the
class they mostly work in.

## Running it

```bash
ledger payroll import run-2027-04-09.csv --db ledger.db --company aquamentor --dry-run
```

```bash
ledger payroll import run-2027-04-09.csv --db ledger.db --company aquamentor
```

`--config PATH` overrides the config file. The default is
`$LEDGER_PAYROLL_CONFIG`, else `.local/config.toml`. The output is totals
only: gross, employer costs, owed to 2400, and net pay.

Running the same file again posts nothing and reports `already_imported`.
If a run was imported with wrong figures, reverse its entry
(`reverse_entry` on the MCP server), then import the
corrected file.

Over MCP: `import_payroll` with `{ "actor": "dan", "path": "…" }`. It
returns the same totals and the posted entry.
