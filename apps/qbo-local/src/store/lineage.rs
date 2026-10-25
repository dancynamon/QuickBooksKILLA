//! Walking QBO's `LinkedTxn` graph. Estimate to invoice, PO to bill to bill
//! payment — the questions a job shop actually asks about a document.
//!
//! Links are stored as written, in one direction, on the document that carries
//! them. Both directions are read from that one table: an invoice names the
//! estimate it came from, so "what did this estimate become?" is the same rows
//! read backwards, which is what `idx_links_to` exists for.

use rusqlite::params;

use super::{document_row, DocumentRow, Store, StoreError, DOCUMENT_SELECT};
use crate::domain::RealmId;

/// One edge of the graph.
///
/// `to_type` stays a string rather than becoming a [`crate::domain::DocumentType`].
/// QBO's `TxnType` vocabulary is wider than the entity names it accepts on the
/// wire — `BillPaymentCheck`, `Check`, `ReimburseCharge` and others appear here
/// and map to no single entity endpoint. Narrowing it would mean either
/// dropping edges or inventing a mapping; the payload's own word is kept.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct DocumentLink {
    pub from_qbo_id: String,
    pub from_type: String,
    pub to_qbo_id: String,
    pub to_type: String,
    /// `None` when the link is on the header rather than a line. A bill payment
    /// carries its links per line, one per bill it settles.
    pub line_no: Option<i64>,
}

/// The connected component around one document.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Lineage {
    pub root: String,
    /// Every reachable document this replica actually holds, root included.
    pub documents: Vec<DocumentRow>,
    pub edges: Vec<DocumentLink>,
    /// Ids reached by an edge but not present in the replica — a document
    /// outside the mirrored history window, or one of the peripheral types.
    /// Reported rather than hidden, so a gap in a chain reads as a gap.
    pub unresolved: Vec<String>,
}

impl Store {
    /// Links this document declares — what it was built from.
    pub fn links_from(
        &self,
        realm: &RealmId,
        qbo_id: &str,
    ) -> Result<Vec<DocumentLink>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT from_qbo_id, from_type, to_qbo_id, to_type, line_no
             FROM document_links
             WHERE realm_id = ?1 AND from_qbo_id = ?2
             ORDER BY seq",
        )?;
        let rows = statement.query_map(params![realm.as_str(), qbo_id], link)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Links pointing at this document — what came out of it.
    pub fn links_to(&self, realm: &RealmId, qbo_id: &str) -> Result<Vec<DocumentLink>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT from_qbo_id, from_type, to_qbo_id, to_type, line_no
             FROM document_links
             WHERE realm_id = ?1 AND to_qbo_id = ?2
             ORDER BY from_qbo_id, seq",
        )?;
        let rows = statement.query_map(params![realm.as_str(), qbo_id], link)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Walk outward from a document in both directions, breadth-first.
    ///
    /// `max_depth` bounds the walk; a visited set bounds it absolutely, so a
    /// cycle — which QBO's graph does not forbid — terminates rather than
    /// spinning.
    pub fn lineage(
        &self,
        realm: &RealmId,
        qbo_id: &str,
        max_depth: usize,
    ) -> Result<Lineage, StoreError> {
        let mut visited = vec![qbo_id.to_string()];
        let mut frontier = vec![qbo_id.to_string()];
        let mut edges: Vec<DocumentLink> = Vec::new();

        for _ in 0..max_depth {
            let mut next = Vec::new();
            for current in &frontier {
                for edge in self
                    .links_from(realm, current)?
                    .into_iter()
                    .chain(self.links_to(realm, current)?)
                {
                    let neighbour = if edge.from_qbo_id == *current {
                        edge.to_qbo_id.clone()
                    } else {
                        edge.from_qbo_id.clone()
                    };
                    if !edges.contains(&edge) {
                        edges.push(edge);
                    }
                    if !visited.contains(&neighbour) {
                        visited.push(neighbour.clone());
                        next.push(neighbour);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }

        let mut documents = Vec::new();
        let mut unresolved = Vec::new();
        for id in &visited {
            match self.get_document(realm, id)? {
                Some(document) => documents.push(document),
                None => unresolved.push(id.clone()),
            }
        }
        documents.sort_by(|a, b| a.txn_date.cmp(&b.txn_date).then(a.qbo_id.cmp(&b.qbo_id)));

        Ok(Lineage {
            root: qbo_id.to_string(),
            documents,
            edges,
            unresolved,
        })
    }

    /// Documents facing one contact, newest first — a customer's or vendor's
    /// whole history in one read.
    pub fn documents_for_contact(
        &self,
        realm: &RealmId,
        contact_type: crate::domain::ContactType,
        contact_id: &str,
        limit: i64,
    ) -> Result<Vec<DocumentRow>, StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "{DOCUMENT_SELECT}
             WHERE d.realm_id = ?1 AND d.contact_type = ?2 AND d.contact_id = ?3
               AND d.is_deleted = 0
             ORDER BY d.txn_date DESC, d.qbo_id DESC
             LIMIT ?4"
        ))?;
        let rows = statement.query_map(
            params![realm.as_str(), contact_type.as_str(), contact_id, limit],
            document_row,
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Every document with a line for this item — "where is this part used".
    pub fn documents_for_item(
        &self,
        realm: &RealmId,
        item_id: &str,
        limit: i64,
    ) -> Result<Vec<DocumentRow>, StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "{DOCUMENT_SELECT}
             WHERE d.realm_id = ?1 AND d.is_deleted = 0 AND d.qbo_id IN (
                 SELECT doc_qbo_id FROM document_lines
                 WHERE realm_id = ?1 AND item_id = ?2
             )
             ORDER BY d.txn_date DESC, d.qbo_id DESC
             LIMIT ?3"
        ))?;
        let rows = statement.query_map(params![realm.as_str(), item_id, limit], document_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

fn link(row: &rusqlite::Row<'_>) -> rusqlite::Result<DocumentLink> {
    Ok(DocumentLink {
        from_qbo_id: row.get(0)?,
        from_type: row.get(1)?,
        to_qbo_id: row.get(2)?,
        to_type: row.get(3)?,
        line_no: row.get(4)?,
    })
}
