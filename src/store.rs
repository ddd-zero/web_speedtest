use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use chrono::Utc;
use rusqlite::{Connection, ToSql, params, params_from_iter};

use crate::{ip::mask_ip_for_public, models::PublicSpeedRecord};

#[derive(Clone)]
pub struct SpeedStore {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("数据库错误: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("更新令牌无效")]
    InvalidUpdateToken,
    #[error("数据库连接锁已损坏")]
    LockPoisoned,
}

#[derive(Debug, Clone)]
pub struct SaveResultInput {
    pub id: Option<i64>,
    pub domain: String,
    pub https_latency_ms: f64,
    pub https_jitter_ms: f64,
    pub download_mbps: f64,
    pub client_ip: Option<String>,
    pub ip_country: Option<String>,
    pub ip_region: Option<String>,
    pub ip_city: Option<String>,
    pub ip_isp: Option<String>,
    pub colo: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct QueryFilter {
    pub domain: Option<String>,
    pub q: Option<String>,
    pub limit: usize,
}

impl SpeedStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    pub fn in_memory() -> Result<Self, StoreError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self, StoreError> {
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), StoreError> {
        let conn = self.conn.lock().map_err(|_| StoreError::LockPoisoned)?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS speed_results (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                domain TEXT NOT NULL,
                https_latency_ms REAL,
                https_jitter_ms REAL,
                download_mbps REAL,
                client_ip TEXT,
                ip_country TEXT,
                ip_region TEXT,
                ip_city TEXT,
                ip_isp TEXT,
                colo TEXT,
                created_at TEXT NOT NULL
            );
            "#,
        )?;
        conn.execute_batch(
            r#"

            CREATE INDEX IF NOT EXISTS idx_speed_results_created_at
                ON speed_results(created_at);
            CREATE INDEX IF NOT EXISTS idx_speed_results_domain
                ON speed_results(domain);
            "#,
        )?;
        Ok(())
    }

    pub fn save_result(&self, input: SaveResultInput) -> Result<i64, StoreError> {
        match input.id {
            Some(id) => self.update_result(id, input),
            None => self.insert_result(input),
        }
    }

    fn insert_result(&self, input: SaveResultInput) -> Result<i64, StoreError> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().map_err(|_| StoreError::LockPoisoned)?;

        conn.execute(
            r#"
            INSERT INTO speed_results (
                domain, https_latency_ms, https_jitter_ms, download_mbps,
                client_ip, ip_country, ip_region, ip_city, ip_isp, colo, created_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            "#,
            params![
                input.domain,
                input.https_latency_ms,
                input.https_jitter_ms,
                input.download_mbps,
                input.client_ip,
                input.ip_country,
                input.ip_region,
                input.ip_city,
                input.ip_isp,
                input.colo,
                now,
            ],
        )?;

        Ok(conn.last_insert_rowid())
    }

    fn update_result(&self, id: i64, input: SaveResultInput) -> Result<i64, StoreError> {
        let conn = self.conn.lock().map_err(|_| StoreError::LockPoisoned)?;
        let rows = conn.execute(
            r#"
            UPDATE speed_results
            SET
                domain = ?2,
                https_latency_ms = ?3,
                https_jitter_ms = ?4,
                download_mbps = ?5,
                client_ip = COALESCE(?6, client_ip),
                ip_country = COALESCE(?7, ip_country),
                ip_region = COALESCE(?8, ip_region),
                ip_city = COALESCE(?9, ip_city),
                ip_isp = COALESCE(?10, ip_isp),
                colo = COALESCE(?11, colo)
            WHERE id = ?1
            "#,
            params![
                id,
                input.domain,
                input.https_latency_ms,
                input.https_jitter_ms,
                input.download_mbps,
                input.client_ip,
                input.ip_country,
                input.ip_region,
                input.ip_city,
                input.ip_isp,
                input.colo,
            ],
        )?;

        if rows == 0 {
            return Err(StoreError::InvalidUpdateToken);
        }
        Ok(id)
    }

    pub fn query_results(&self, filter: QueryFilter) -> Result<Vec<PublicSpeedRecord>, StoreError> {
        let mut sql = String::from(
            r#"
            SELECT
                id, domain, https_latency_ms, https_jitter_ms, download_mbps,
                client_ip, ip_country, ip_region, ip_city, ip_isp, colo, created_at
            FROM speed_results
            "#,
        );
        let mut clauses = Vec::new();
        let mut values: Vec<Box<dyn ToSql>> = Vec::new();

        if let Some(domain) = filter.domain {
            clauses.push("domain = ?".to_string());
            values.push(Box::new(domain));
        }

        if let Some(q) = filter.q.filter(|q| !q.trim().is_empty()) {
            for keyword in q.split_whitespace() {
                let pattern = format!("%{}%", keyword.trim());
                clauses.push(
                    "(domain LIKE ? OR client_ip LIKE ? OR ip_country LIKE ? OR ip_region LIKE ? OR ip_city LIKE ? OR ip_isp LIKE ? OR colo LIKE ?)".to_string(),
                );
                for _ in 0..7 {
                    values.push(Box::new(pattern.clone()));
                }
            }
        }

        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }

        sql.push_str(" ORDER BY datetime(created_at) DESC, id DESC LIMIT ?");
        values.push(Box::new(filter.limit.max(1) as i64));

        let conn = self.conn.lock().map_err(|_| StoreError::LockPoisoned)?;
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params_from_iter(values.iter().map(|value| value.as_ref())),
            |row| {
                let client_ip: Option<String> = row.get(5)?;
                Ok(PublicSpeedRecord {
                    id: row.get(0)?,
                    domain: row.get(1)?,
                    https_latency_ms: row.get(2)?,
                    https_jitter_ms: row.get(3)?,
                    download_mbps: row.get(4)?,
                    client_ip: client_ip
                        .as_deref()
                        .and_then(mask_ip_for_public)
                        .unwrap_or_else(|| "未知".to_string()),
                    ip_country: row.get(6)?,
                    ip_region: row.get(7)?,
                    ip_city: row.get(8)?,
                    ip_isp: row.get(9)?,
                    colo: row.get(10)?,
                    created_at: row.get(11)?,
                })
            },
        )?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::{QueryFilter, SaveResultInput, SpeedStore};

    fn sample_input() -> SaveResultInput {
        SaveResultInput {
            id: None,
            domain: "a1.example.com".to_string(),
            https_latency_ms: 42.0,
            https_jitter_ms: 6.5,
            download_mbps: 80.0,
            client_ip: Some("1.2.3.4".to_string()),
            ip_country: Some("德国".to_string()),
            ip_region: Some("黑森州".to_string()),
            ip_city: Some("法兰克福".to_string()),
            ip_isp: Some("xtom.com".to_string()),
            colo: Some("SJC".to_string()),
        }
    }

    fn table_columns(store: &SpeedStore) -> Vec<String> {
        let conn = store.conn.lock().expect("lock should be available");
        let mut stmt = conn
            .prepare("PRAGMA table_info(speed_results)")
            .expect("schema should be readable");
        stmt.query_map([], |row| row.get::<_, String>(1))
            .expect("columns should query")
            .collect::<Result<Vec<_>, _>>()
            .expect("columns should collect")
    }

    #[test]
    fn store_should_create_simplified_schema() {
        let store = SpeedStore::in_memory().expect("store should initialize");

        let columns = table_columns(&store);

        assert!(columns.iter().any(|column| column == "domain"));
        assert!(columns.iter().any(|column| column == "https_latency_ms"));
        assert!(columns.iter().any(|column| column == "https_jitter_ms"));
        assert!(columns.iter().any(|column| column == "download_mbps"));
        assert!(!columns.iter().any(|column| column == "update_token"));
        assert!(!columns.iter().any(|column| column == "domain_key"));
        assert!(!columns.iter().any(|column| column == "domain_host"));
        assert!(
            !columns
                .iter()
                .any(|column| column == "partial_download_mbps")
        );
        assert!(!columns.iter().any(|column| column == "final_download_mbps"));
        assert!(!columns.iter().any(|column| column == "updated_at"));
    }

    #[test]
    fn store_should_insert_record_and_query_simplified_public_record() {
        let store = SpeedStore::in_memory().expect("store should initialize");
        let saved = store.save_result(sample_input()).expect("save should work");

        let records = store
            .query_results(QueryFilter::default())
            .expect("query should work");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, saved);
        assert_eq!(records[0].domain, "a1.example.com");
        assert_eq!(records[0].https_latency_ms, Some(42.0));
        assert_eq!(records[0].https_jitter_ms, Some(6.5));
        assert_eq!(records[0].download_mbps, Some(80.0));
        assert_eq!(records[0].client_ip, "1.2.3.*");
        assert_eq!(records[0].ip_country.as_deref(), Some("德国"));
        assert_eq!(records[0].ip_isp.as_deref(), Some("xtom.com"));
    }

    #[test]
    fn store_should_update_existing_record_without_database_token() {
        let store = SpeedStore::in_memory().expect("store should initialize");
        let saved = store.save_result(sample_input()).expect("save should work");
        let mut update = sample_input();
        update.id = Some(saved);
        update.download_mbps = 120.0;

        store.save_result(update).expect("update should work");
        let records = store
            .query_results(QueryFilter::default())
            .expect("query should work");

        assert_eq!(records[0].download_mbps, Some(120.0));
    }

    #[test]
    fn store_should_query_multiple_keywords_across_ip_location_and_isp() {
        let store = SpeedStore::in_memory().expect("store should initialize");
        store.save_result(sample_input()).expect("save should work");

        let records = store
            .query_results(QueryFilter {
                q: Some("德国 xtom".to_string()),
                limit: 80,
                ..QueryFilter::default()
            })
            .expect("query should work");

        assert_eq!(records.len(), 1);
    }

    #[test]
    fn store_should_apply_resolved_query_limit() {
        let store = SpeedStore::in_memory().expect("store should initialize");
        store.save_result(sample_input()).expect("save should work");
        let mut second = sample_input();
        second.domain = "a2.example.com".to_string();
        store.save_result(second).expect("save should work");

        let records = store
            .query_results(QueryFilter {
                limit: 1,
                ..QueryFilter::default()
            })
            .expect("query should work");

        assert_eq!(records.len(), 1);
    }
}
