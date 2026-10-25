// Document register, open-documents queue, and the document viewer with its
// lineage rail. One module for every document type (`ROADMAP.md` §B1:
// "document viewers with the lineage rail") rather than one screen per QBO
// form the way the prototype has it — `document_detail`, `list_documents`
// and `open_documents` are already generic over `doc_type`, so the screen
// reading them can be too.

import { provider } from "../data/index.js";
import { esc, plain, date, longDate } from "../format.js";
import { viewHead, withState, documentStatusTag, classCell, loadClassMap, balanceCell } from "./shared.js";

const DOC_TYPES = [
  "Estimate", "Invoice", "SalesReceipt", "CreditMemo", "RefundReceipt", "Payment",
  "PurchaseOrder", "Bill", "BillPayment", "VendorCredit", "Purchase", "Deposit", "JournalEntry",
];

export function isDocType(name) {
  return DOC_TYPES.includes(name);
}

function partyLabel(doc) {
  return doc.contact_name ?? doc.contact_id ?? "—";
}

function registerRows(rows, classMap) {
  if (rows.length === 0) {
    return `<tr><td colspan="7" class="dim">Nothing here.</td></tr>`;
  }
  return rows.map((d) => `
    <tr data-clickable data-doc="${esc(d.qbo_id)}">
      <td class="mono">${esc(d.doc_number ?? d.qbo_id)}</td>
      <td class="mono dim">${date(d.txn_date)}</td>
      <td class="trunc">${esc(partyLabel(d))}</td>
      <td>${classCell(d.class_id, classMap)}</td>
      <td class="r num">${plain(d.total)}</td>
      <td class="r num">${balanceCell(d.balance)}</td>
      <td>${documentStatusTag(d)}</td>
    </tr>`).join("");
}

/**
 * The register for one document type — `list_documents` by default, or
 * `open_documents` when `openOnly` is set (the "AR/AP work queue", per
 * `docs/MCP.md`, e.g. open POs and open estimates from `ROADMAP.md` §B1).
 */
export async function renderRegister(main, docType, openOnly) {
  if (!isDocType(docType)) {
    main.innerHTML = viewHead("Unknown document type", docType);
    return;
  }
  const title = openOnly ? `${docType} — open` : docType;
  await withState(
    main,
    title,
    async () => {
      const [rows, classMap] = await Promise.all([
        openOnly ? provider.openDocuments(docType, 0, 200) : provider.listDocuments(docType, undefined, undefined, 0, 200),
        loadClassMap(provider),
      ]);
      return { rows, classMap };
    },
    ({ rows, classMap }) => {
      const otherHref = openOnly ? `#register/${docType}` : `#open/${docType}`;
      const toggleLabel = openOnly ? "Show all" : "Show open only";
      main.innerHTML = viewHead(
        docType,
        openOnly ? `${rows.length} open` : `${rows.length} on record`,
        `<a class="btn" href="${otherHref}">${esc(toggleLabel)}</a>`,
      ) + `
      <div class="pad">
        <div class="grid-wrap"><div class="grid-scroll"><table>
          <thead><tr><th>No.</th><th>Date</th><th>Party</th><th>Class</th><th class="r">Total</th><th class="r">Balance</th><th>Status</th></tr></thead>
          <tbody>${registerRows(rows, classMap)}</tbody>
        </table></div></div>
        <p class="hint">Click any row to open it. <kbd>⌘K</kbd> jumps straight to a document by number.</p>
      </div>`;

      main.querySelectorAll("tr[data-doc]").forEach((row) => {
        row.addEventListener("click", () => {
          window.location.hash = `#doc/${encodeURIComponent(row.dataset.doc)}`;
        });
      });
    },
  );
}

/** The lineage rail: every document `document_detail` found connected to
 * this one, oldest first — for the chains this app actually shows
 * (estimate -> invoice -> payment, PO -> bill -> bill payment) that reads
 * as the chain in order; a wider graph still renders, just as more boxes. */
function lineageRail(current, lineage) {
  if (lineage.documents.length <= 1) return "";
  const ordered = [...lineage.documents].sort((a, b) => (a.txn_date < b.txn_date ? -1 : a.txn_date > b.txn_date ? 1 : 0));
  return `<div class="chain">${ordered.map((d, i) => `
    ${i ? `<span class="chain-arrow" aria-hidden="true">→</span>` : ""}
    <button class="chain-step${d.qbo_id === current ? " is-here" : ""}" data-doc="${esc(d.qbo_id)}" ${d.qbo_id === current ? "disabled" : ""}>
      <span class="chain-lbl">${esc(d.doc_type)}</span>
      <span class="chain-no mono">${esc(d.doc_number ?? d.qbo_id)}</span>
      <span class="chain-state">${plain(d.total)}</span>
    </button>`).join("")}</div>`;
}

function linkedRecordsPanel(lineage, current) {
  const others = lineage.documents.filter((d) => d.qbo_id !== current);
  const unresolvedCount = lineage.unresolved.length;
  if (others.length === 0 && unresolvedCount === 0) {
    return `<div class="panel"><div class="panel-head"><span class="lbl">Linked records</span></div>
      <div class="panel-body"><p class="hint" style="margin:0">Nothing else links to this document.</p></div></div>`;
  }
  const rows = others.map((d) => `
    <button class="link-row" data-doc="${esc(d.qbo_id)}">
      <span class="link-glyph" aria-hidden="true">▤</span>
      <span class="link-main"><b>${esc(d.doc_type)} ${esc(d.doc_number ?? d.qbo_id)}</b>
        <div class="link-sub">${date(d.txn_date)} · ${plain(d.total)}${d.balance != null ? " · balance " + plain(d.balance) : ""}</div></span>
      <span class="dim" aria-hidden="true">›</span>
    </button>`).join("");
  const unresolvedNote = unresolvedCount
    ? `<p class="hint">${unresolvedCount} linked id${unresolvedCount === 1 ? "" : "s"} outside this replica's mirrored history.</p>`
    : "";
  return `<div class="panel"><div class="panel-head"><span class="lbl">Linked records</span></div>
    <div class="panel-body" style="padding:2px 12px 10px">${rows}${unresolvedNote}</div></div>`;
}

function linesTable(lines, classMap) {
  if (lines.length === 0) return `<p class="hint" style="margin:11px 12px">No line items.</p>`;
  return `<div class="grid-scroll"><table>
    <thead><tr><th style="width:34px">#</th><th>Description</th><th class="r">Qty</th><th class="r">Rate</th><th class="r">Amount</th><th>Class</th></tr></thead>
    <tbody>${lines.map((l) => `
      <tr>
        <td class="mono dim">${l.line_no}</td>
        <td class="trunc">${esc(l.description ?? "—")}</td>
        <td class="r num">${esc(l.qty ?? "")}</td>
        <td class="r num">${l.unit_price != null ? plain(l.unit_price) : ""}</td>
        <td class="r num">${plain(l.amount)}</td>
        <td>${classCell(l.class_id, classMap)}</td>
      </tr>`).join("")}</tbody>
  </table></div>`;
}

/** One document, with its lines and its lineage rail. */
export async function renderDetail(main, qboId) {
  await withState(
    main,
    `document ${qboId}`,
    async () => {
      const [detail, classMap] = await Promise.all([
        provider.documentDetail(qboId),
        loadClassMap(provider),
      ]);
      return { detail, classMap };
    },
    ({ detail, classMap }) => {
      const { document: doc, lines, lineage } = detail;
      main.innerHTML = viewHead(
        `${doc.doc_type} ${doc.doc_number ?? doc.qbo_id}`,
        `${partyLabel(doc)} · ${longDate(doc.txn_date)}`,
      ) + lineageRail(doc.qbo_id, lineage) + `
      <div class="pad">
        <div class="split">
          <div class="panel">
            <div class="doc-head">
              <div class="field"><span class="lbl">Party</span><span class="field-val">${esc(partyLabel(doc))}</span></div>
              <div class="field"><span class="lbl">Date</span><span class="field-val mono">${date(doc.txn_date)}</span></div>
              <div class="field"><span class="lbl">Due</span><span class="field-val mono">${doc.due_date ? date(doc.due_date) : "—"}</span></div>
              <div class="field"><span class="lbl">Class</span><span class="field-val">${classCell(doc.class_id, classMap)}</span></div>
              <div class="field"><span class="lbl">Customer PO</span><span class="field-val mono">${doc.po_number ? esc(doc.po_number) : "—"}</span></div>
              <div class="field"><span class="lbl">Status</span><span class="field-val">${documentStatusTag(doc)}</span></div>
            </div>
            ${linesTable(lines, classMap)}
            <div class="totals"><dl>
              <div class="grand" style="display:contents"><dt>Total</dt><dd>${plain(doc.total)}</dd></div>
              ${doc.balance != null ? `<dt>Balance</dt><dd>${plain(doc.balance)}</dd>` : ""}
            </dl></div>
            ${doc.customer_memo ? `<p class="hint" style="padding:0 12px 11px">${esc(doc.customer_memo)}</p>` : ""}
          </div>
          <div>
            ${linkedRecordsPanel(lineage, doc.qbo_id)}
            <div class="panel" style="margin-top:14px">
              <div class="panel-head"><span class="lbl">Party</span></div>
              <div class="panel-body">
                <button class="btn" style="width:100%" data-party="${doc.contact_type ?? ""}:${esc(doc.contact_id ?? "")}" ${doc.contact_id ? "" : "disabled"}>Open ${esc(doc.contact_type === "Vendor" ? "vendor" : "customer")} page</button>
              </div>
            </div>
          </div>
        </div>
      </div>`;

      main.querySelectorAll("[data-doc]").forEach((el) => {
        el.addEventListener("click", () => {
          window.location.hash = `#doc/${encodeURIComponent(el.dataset.doc)}`;
        });
      });
      const partyBtn = main.querySelector("[data-party]");
      if (partyBtn && doc.contact_id) {
        partyBtn.addEventListener("click", () => {
          const kind = doc.contact_type === "Vendor" ? "vendor" : "customer";
          window.location.hash = `#contact/${encodeURIComponent(`${kind}:${doc.contact_id}`)}`;
        });
      }
    },
  );
}
