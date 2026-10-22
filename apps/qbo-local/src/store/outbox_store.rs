//! Durable outbox and id map. `DESIGN.md` §6.2, §6.4.
//!
//! The outbox is the one table in the replica that QBO cannot rebuild: it can
//! hold writes QBO has never seen. Every function here writes in a single
//! statement or transaction, and nothing here ever deletes a record.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

use super::{Store, StoreError};
use crate::domain::{EntityType, RealmId};
use crate::outbox::{Operation, OutboxRecord, OutboxState};

/// Which Intuit environment a realm lives in (§8: sandbox first).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Environment {
    Production,
    Sandbox,
}

impl Environment {
    pub const fn as_str(self) -> &'static str {
        match self {
            Environment::Production => "production",
            Environment::Sandbox => "sandbox",
        }
    }
}

const RECORD_COLUMNS: &str = "id, realm_id, entity_type, operation, payload_json, local_entity_id,
    base_sync_token, request_id, state, attempts, depends_on, last_error, created_at, updated_at";

fn corrupt(what: &str, value: &str) -> StoreError {
    StoreError::Corrupt(format!("outbox {what} {value:?}"))
}

fn timestamp(raw: &str) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(raw)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|_| corrupt("timestamp", raw))
}

type RawRecord = (
    String, String, String, String, String, String, Option<String>, String, String, i64,
    Option<String>, Option<String>, String, String,
);

fn raw_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRecord> {
    Ok((
        row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?,
        row.get(7)?, row.get(8)?, row.get(9)?, row.get(10)?, row.get(11)?, row.get(12)?, row.get(13)?,
    ))
}

fn record_from(raw: RawRecord) -> Result<OutboxRecord, StoreError> {
    let (id, realm, entity_type, operation, payload, local_id, base, request_id, state, attempts, depends_on, last_error, created, updated) = raw;
    let uuid = |s: &str| Uuid::parse_str(s).map_err(|_| corrupt("uuid", s));
    Ok(OutboxRecord {
        id: uuid(&id)?,
        realm_id: RealmId::parse(realm.clone()).map_err(|_| corrupt("realm", &realm))?,
        entity_type: EntityType::parse(&entity_type).map_err(|_| corrupt("entity type", &entity_type))?,
        operation: Operation::parse(&operation).ok_or_else(|| corrupt("operation", &operation))?,
        payload_json: serde_json::from_str(&payload)?,
        local_entity_id: local_id,
        base_sync_token: base,
        request_id: uuid(&request_id)?,
        state: OutboxState::parse(&state).ok_or_else(|| corrupt("state", &state))?,
        attempts: u32::try_from(attempts).map_err(|_| corrupt("attempts", &attempts.to_string()))?,
        depends_on: depends_on.as_deref().map(uuid).transpose()?,
        last_error,
        created_at: timestamp(&created)?,
        updated_at: timestamp(&updated)?,
    })
}

impl Store {
    pub fn realm_environment(&self, realm: &RealmId) -> Result<Environment, StoreError> {
        let raw: Option<String> = self
            .connection
            .query_row("SELECT environment FROM realms WHERE realm_id = ?1", params![realm.as_str()], |row| row.get(0))
            .optional()?;
        match raw.as_deref() {
            Some("sandbox") => Ok(Environment::Sandbox),
            Some(_) => Ok(Environment::Production),
            None => Err(StoreError::Corrupt(format!("realm {realm} is not registered"))),
        }
    }

    pub fn set_realm_environment(&self, realm: &RealmId, environment: Environment) -> Result<(), StoreError> {
        self.connection.execute(
            "UPDATE realms SET environment = ?2 WHERE realm_id = ?1",
            params![realm.as_str(), environment.as_str()],
        )?;
        Ok(())
    }

    /// When the first complete sweep of this realm finished — the start of the
    /// read-only soak (§8). `None` until a sweep has completed.
    pub fn first_full_sweep(&self, realm: &RealmId) -> Result<Option<DateTime<Utc>>, StoreError> {
        let raw: Option<String> = self.connection.query_row(
            "SELECT MIN(last_full_sweep) FROM sync_cursors WHERE realm_id = ?1",
            params![realm.as_str()],
            |row| row.get(0),
        )?;
        raw.as_deref().map(timestamp).transpose()
    }

    /// Add a new record. Refuses an id already present: a record is written
    /// once and then only changes state.
    pub fn enqueue(&self, record: &OutboxRecord) -> Result<(), StoreError> {
        self.connection.execute(
            &format!("INSERT INTO outbox ({RECORD_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"),
            params![
                record.id.to_string(),
                record.realm_id.as_str(),
                record.entity_type.as_str(),
                record.operation.as_str(),
                record.payload_json.to_string(),
                record.local_entity_id,
                record.base_sync_token,
                record.request_id.to_string(),
                record.state.as_str(),
                record.attempts,
                record.depends_on.map(|d| d.to_string()),
                record.last_error,
                record.created_at.to_rfc3339(),
                record.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Persist a record's state after a transition. Only the fields a
    /// transition changes are written; the payload and `RequestId` are fixed
    /// at enqueue and never rewritten.
    pub fn save_outbox_state(&self, record: &OutboxRecord) -> Result<(), StoreError> {
        let changed = self.connection.execute(
            "UPDATE outbox SET state = ?2, attempts = ?3, last_error = ?4, updated_at = ?5,
                    base_sync_token = ?6
             WHERE id = ?1",
            params![
                record.id.to_string(),
                record.state.as_str(),
                record.attempts,
                record.last_error,
                record.updated_at.to_rfc3339(),
                record.base_sync_token,
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::Corrupt(format!("outbox record {} not found", record.id)));
        }
        Ok(())
    }

    pub fn outbox_record(&self, id: Uuid) -> Result<Option<OutboxRecord>, StoreError> {
        let raw = self
            .connection
            .query_row(&format!("SELECT {RECORD_COLUMNS} FROM outbox WHERE id = ?1"), params![id.to_string()], raw_record)
            .optional()?;
        raw.map(record_from).transpose()
    }

    /// Records of a realm in the given states, oldest first (UUIDv7 order).
    pub fn outbox_records(&self, realm: &RealmId, states: &[OutboxState]) -> Result<Vec<OutboxRecord>, StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {RECORD_COLUMNS} FROM outbox WHERE realm_id = ?1 ORDER BY id"
        ))?;
        let rows = statement.query_map(params![realm.as_str()], raw_record)?;
        let mut out = Vec::new();
        for row in rows {
            let record = record_from(row?)?;
            if states.contains(&record.state) {
                out.push(record);
            }
        }
        Ok(out)
    }

    /// Records about one entity that have not reached a terminal state —
    /// what the document view shows as "queued" or "needs attention".
    pub fn open_writes_for(&self, realm: &RealmId, local_entity_id: &str) -> Result<Vec<OutboxRecord>, StoreError> {
        Ok(self
            .outbox_records(realm, &[OutboxState::Pending, OutboxState::InFlight, OutboxState::Conflicted, OutboxState::Rejected])?
            .into_iter()
            .filter(|r| r.local_entity_id == local_entity_id)
            .collect())
    }

    pub fn record_id_mapping(
        &self,
        realm: &RealmId,
        local_entity_id: &str,
        entity_type: EntityType,
        qbo_id: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.connection.execute(
            "INSERT INTO local_id_map (realm_id, local_entity_id, entity_type, qbo_id, mapped_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(realm_id, local_entity_id) DO NOTHING",
            params![realm.as_str(), local_entity_id, entity_type.as_str(), qbo_id, now.to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn id_map(&self, realm: &RealmId) -> Result<HashMap<(String, String), String>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT local_entity_id, qbo_id FROM local_id_map WHERE realm_id = ?1")?;
        let rows = statement.query_map(params![realm.as_str()], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        let mut map = HashMap::new();
        for row in rows {
            let (local, qbo) = row?;
            map.insert((realm.as_str().to_string(), local), qbo);
        }
        Ok(map)
    }

    /// Active contacts of one type by name — the customer picker's list.
    /// Active bank and credit card accounts, by name: `(id, name, type)`.
    pub fn payment_accounts(&self, realm: &RealmId) -> Result<Vec<(String, String, String)>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT qbo_id, name, account_type FROM accounts
             WHERE realm_id = ?1 AND is_active = 1 AND is_deleted = 0 AND account_type IN ('Bank', 'Credit Card')
             ORDER BY account_type, name COLLATE NOCASE",
        )?;
        let rows = statement.query_map(params![realm.as_str()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn contacts(&self, realm: &RealmId, contact_type: crate::domain::ContactType) -> Result<Vec<(String, String, Option<String>)>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT qbo_id, display_name, company_name FROM contacts
             WHERE realm_id = ?1 AND contact_type = ?2 AND is_deleted = 0 AND is_active = 1
             ORDER BY display_name COLLATE NOCASE",
        )?;
        let rows = statement.query_map(params![realm.as_str(), contact_type.as_str()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn contact_name(&self, realm: &RealmId, contact_type: crate::domain::ContactType, qbo_id: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .connection
            .query_row(
                "SELECT display_name FROM contacts WHERE realm_id = ?1 AND contact_type = ?2 AND qbo_id = ?3",
                params![realm.as_str(), contact_type.as_str(), qbo_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// The `DefinitionId` of the book's "P.O. Number" custom field, read from
    /// any sales document that carries it. QBO assigns these per company, so
    /// it is looked up rather than assumed.
    pub fn po_field_definition(&self, realm: &RealmId) -> Result<Option<String>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT raw_json FROM entities
             WHERE realm_id = ?1 AND entity_type IN ('Invoice', 'Estimate', 'SalesReceipt')
               AND raw_json LIKE '%CustomField%' LIMIT 200",
        )?;
        let rows = statement.query_map(params![realm.as_str()], |row| row.get::<_, String>(0))?;
        for row in rows {
            let value: serde_json::Value = serde_json::from_str(&row?)?;
            for field in value.get("CustomField").and_then(|f| f.as_array()).into_iter().flatten() {
                let name = field.get("Name").and_then(|n| n.as_str()).unwrap_or_default();
                let normalized: String = name.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
                if normalized == "ponumber" || normalized == "purchaseordernumber" {
                    if let Some(id) = field.get("DefinitionId").and_then(|d| d.as_str()) {
                        return Ok(Some(id.to_string()));
                    }
                }
            }
        }
        Ok(None)
    }

    /// The QBO id a local id became, if it has.
    pub fn resolve_local_id(&self, realm: &RealmId, local_entity_id: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .connection
            .query_row(
                "SELECT qbo_id FROM local_id_map WHERE realm_id = ?1 AND local_entity_id = ?2",
                params![realm.as_str(), local_entity_id],
                |row| row.get(0),
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbox::NewOutboxRecord;

    fn realm() -> RealmId {
        RealmId::parse("1234567890123456").unwrap()
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_790_000_000, 0).unwrap()
    }

    fn store() -> Store {
        let store = Store::open_in_memory().unwrap();
        store.register_realm(&realm(), "Test", now()).unwrap();
        store
    }

    fn record() -> OutboxRecord {
        OutboxRecord::new(
            NewOutboxRecord {
                id: Uuid::now_v7(),
                request_id: Uuid::now_v7(),
                realm_id: realm(),
                entity_type: EntityType::Invoice,
                operation: Operation::Create,
                payload_json: serde_json::json!({ "DocNumber": "T-1" }),
                local_entity_id: "local:1".into(),
                base_sync_token: None,
            },
            now(),
        )
    }

    #[test]
    fn a_record_round_trips_with_its_request_id() {
        let store = store();
        let original = record();
        store.enqueue(&original).unwrap();
        let loaded = store.outbox_record(original.id).unwrap().unwrap();
        assert_eq!(loaded.request_id, original.request_id);
        assert_eq!(loaded.payload_json, original.payload_json);
        assert_eq!(loaded.state, OutboxState::Pending);
        assert!(store.enqueue(&original).is_err(), "a record is written once");
    }

    #[test]
    fn state_changes_persist_and_filter() {
        let store = store();
        let mut r = record();
        store.enqueue(&r).unwrap();
        r.begin_attempt(true, now()).unwrap();
        store.save_outbox_state(&r).unwrap();
        assert_eq!(store.outbox_records(&realm(), &[OutboxState::InFlight]).unwrap().len(), 1);
        assert!(store.outbox_records(&realm(), &[OutboxState::Pending]).unwrap().is_empty());
        assert_eq!(store.open_writes_for(&realm(), "local:1").unwrap().len(), 1);
    }

    #[test]
    fn id_mappings_are_first_writer_wins() {
        let store = store();
        store.record_id_mapping(&realm(), "local:1", EntityType::Customer, "77", now()).unwrap();
        store.record_id_mapping(&realm(), "local:1", EntityType::Customer, "99", now()).unwrap();
        assert_eq!(store.resolve_local_id(&realm(), "local:1").unwrap().as_deref(), Some("77"));
        assert_eq!(store.id_map(&realm()).unwrap().len(), 1);
    }

    #[test]
    fn realms_default_to_production() {
        let store = store();
        assert_eq!(store.realm_environment(&realm()).unwrap(), Environment::Production);
        store.set_realm_environment(&realm(), Environment::Sandbox).unwrap();
        assert_eq!(store.realm_environment(&realm()).unwrap(), Environment::Sandbox);
    }
}
