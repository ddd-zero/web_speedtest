use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use chrono::Utc;
use rusqlite::{Connection, ToSql, params, params_from_iter};
use uuid::Uuid;

use crate::{
    ip::mask_ip_for_public,
    models::{LatencyStats, PublicSpeedRecord, ResultStatus, SaveResultResponse},
};

#[derive(Clone)]
pub struct SpeedStore {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("数据库错误: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("JSON 序列化失败: {0}")]
    Json(#[from] serde_json::Error),
    #[error("更新记录缺少更新令牌")]
    MissingUpdateToken,
    #[error("更新令牌无效")]
    InvalidUpdateToken,
    #[error("数据库连接锁已损坏")]
    LockPoisoned,
}

#[derive(Debug, Clone)]
pub struct SaveResultInput {
    pub id: Option<i64>,
    pub update_token: Option<String>,
    pub domain_key: String,
    pub domain_host: String,
    pub trace_url: String,
    pub download_url: String,
    pub https_latency: LatencyStats,
    pub partial_download_mbps: Option<f64>,
    pub final_download_mbps: Option<f64>,
    pub status: ResultStatus,
    pub client_ip: String,
    pub location: Option<String>,
    pub isp: Option<String>,
    pub colo: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct QueryFilter {
    pub domain: Option<String>,
    pub status: Option<ResultStatus>,
    pub q: Option<String>,
    pub limit: Option<usize>,
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
                update_token TEXT NOT NULL,
                domain_key TEXT NOT NULL,
                domain_host TEXT NOT NULL,
                trace_url TEXT NOT NULL,
                download_url TEXT NOT NULL,
                https_latency_median_ms REAL,
                https_latency_min_ms REAL,
                https_latency_max_ms REAL,
                https_latency_samples_json TEXT,
                partial_download_mbps REAL,
                final_download_mbps REAL,
                status TEXT NOT NULL,
                client_ip TEXT NOT NULL,
                location TEXT,
                isp TEXT,
                colo TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_speed_results_created_at
                ON speed_results(created_at);
            CREATE INDEX IF NOT EXISTS idx_speed_results_domain
                ON speed_results(domain_key);
            "#,
        )?;
        Ok(())
    }

    pub fn save_result(&self, input: SaveResultInput) -> Result<SaveResultResponse, StoreError> {
        match input.id {
            Some(id) => {
                let token = input
                    .update_token
                    .clone()
                    .ok_or(StoreError::MissingUpdateToken)?;
                self.update_result(id, &token, input)?;
                Ok(SaveResultResponse {
                    id,
                    update_token: token,
                })
            }
            None => self.insert_result(input),
        }
    }

    fn insert_result(&self, input: SaveResultInput) -> Result<SaveResultResponse, StoreError> {
        let token = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let samples_json = serde_json::to_string(&input.https_latency.samples_ms)?;
        let conn = self.conn.lock().map_err(|_| StoreError::LockPoisoned)?;

        // update_token 只返回给本次测速页面，用来约束后续完成态更新。
        conn.execute(
            r#"
            INSERT INTO speed_results (
                update_token, domain_key, domain_host, trace_url, download_url,
                https_latency_median_ms, https_latency_min_ms, https_latency_max_ms,
                https_latency_samples_json, partial_download_mbps, final_download_mbps,
                status, client_ip, location, isp, colo, created_at, updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
            "#,
            params![
                token,
                input.domain_key,
                input.domain_host,
                input.trace_url,
                input.download_url,
                input.https_latency.median_ms,
                input.https_latency.min_ms,
                input.https_latency.max_ms,
                samples_json,
                input.partial_download_mbps,
                input.final_download_mbps,
                input.status.as_str(),
                input.client_ip,
                input.location,
                input.isp,
                input.colo,
                now,
                now,
            ],
        )?;

        Ok(SaveResultResponse {
            id: conn.last_insert_rowid(),
            update_token: token,
        })
    }

    fn update_result(
        &self,
        id: i64,
        token: &str,
        input: SaveResultInput,
    ) -> Result<(), StoreError> {
        let now = Utc::now().to_rfc3339();
        let samples_json = serde_json::to_string(&input.https_latency.samples_ms)?;
        let conn = self.conn.lock().map_err(|_| StoreError::LockPoisoned)?;
        let rows = conn.execute(
            r#"
            UPDATE speed_results
            SET
                domain_key = ?3,
                domain_host = ?4,
                trace_url = ?5,
                download_url = ?6,
                https_latency_median_ms = ?7,
                https_latency_min_ms = ?8,
                https_latency_max_ms = ?9,
                https_latency_samples_json = ?10,
                partial_download_mbps = COALESCE(?11, partial_download_mbps),
                final_download_mbps = COALESCE(?12, final_download_mbps),
                status = ?13,
                client_ip = ?14,
                location = COALESCE(?15, location),
                isp = COALESCE(?16, isp),
                colo = COALESCE(?17, colo),
                updated_at = ?18
            WHERE id = ?1 AND update_token = ?2
            "#,
            params![
                id,
                token,
                input.domain_key,
                input.domain_host,
                input.trace_url,
                input.download_url,
                input.https_latency.median_ms,
                input.https_latency.min_ms,
                input.https_latency.max_ms,
                samples_json,
                input.partial_download_mbps,
                input.final_download_mbps,
                input.status.as_str(),
                input.client_ip,
                input.location,
                input.isp,
                input.colo,
                now,
            ],
        )?;

        if rows == 0 {
            return Err(StoreError::InvalidUpdateToken);
        }
        Ok(())
    }

    pub fn query_results(&self, filter: QueryFilter) -> Result<Vec<PublicSpeedRecord>, StoreError> {
        let mut sql = String::from(
            r#"
            SELECT
                id, domain_key, domain_host,
                https_latency_median_ms, https_latency_min_ms, https_latency_max_ms,
                partial_download_mbps, final_download_mbps, status, client_ip,
                location, isp, colo, created_at, updated_at
            FROM speed_results
            "#,
        );
        let mut clauses = Vec::new();
        let mut values: Vec<Box<dyn ToSql>> = Vec::new();

        if let Some(domain) = filter.domain {
            clauses.push("domain_key = ?".to_string());
            values.push(Box::new(domain));
        }

        if let Some(status) = filter.status {
            clauses.push("status = ?".to_string());
            values.push(Box::new(status.as_str().to_string()));
        }

        if let Some(q) = filter.q.filter(|q| !q.trim().is_empty()) {
            let pattern = format!("%{}%", q.trim());
            clauses.push(
                "(domain_host LIKE ? OR location LIKE ? OR isp LIKE ? OR colo LIKE ?)".to_string(),
            );
            values.push(Box::new(pattern.clone()));
            values.push(Box::new(pattern.clone()));
            values.push(Box::new(pattern.clone()));
            values.push(Box::new(pattern));
        }

        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }

        sql.push_str(" ORDER BY datetime(created_at) DESC, id DESC LIMIT ?");
        let limit = filter.limit.unwrap_or(50).clamp(1, 200) as i64;
        values.push(Box::new(limit));

        let conn = self.conn.lock().map_err(|_| StoreError::LockPoisoned)?;
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params_from_iter(values.iter().map(|value| value.as_ref())),
            |row| {
                let status: String = row.get(8)?;
                let client_ip: String = row.get(9)?;
                Ok(PublicSpeedRecord {
                    id: row.get(0)?,
                    domain_key: row.get(1)?,
                    domain_host: row.get(2)?,
                    https_latency_median_ms: row.get(3)?,
                    https_latency_min_ms: row.get(4)?,
                    https_latency_max_ms: row.get(5)?,
                    partial_download_mbps: row.get(6)?,
                    final_download_mbps: row.get(7)?,
                    status: ResultStatus::from_db(&status),
                    client_ip: mask_ip_for_public(&client_ip).unwrap_or_else(|| "未知".to_string()),
                    location: row.get(10)?,
                    isp: row.get(11)?,
                    colo: row.get(12)?,
                    created_at: row.get(13)?,
                    updated_at: row.get(14)?,
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
    use crate::models::{LatencyStats, ResultStatus};

    fn sample_input(status: ResultStatus) -> SaveResultInput {
        SaveResultInput {
            id: None,
            update_token: None,
            domain_key: "a1".to_string(),
            domain_host: "a1.steinsgate.eu.org".to_string(),
            trace_url: "https://a1.steinsgate.eu.org/cdn-cgi/trace".to_string(),
            download_url: "https://a1.steinsgate.eu.org/200mb.test".to_string(),
            https_latency: LatencyStats {
                median_ms: 42.0,
                min_ms: 38.0,
                max_ms: 51.0,
                samples_ms: vec![38.0, 42.0, 51.0],
            },
            partial_download_mbps: Some(80.0),
            final_download_mbps: None,
            status,
            client_ip: "1.2.3.4".to_string(),
            location: None,
            isp: None,
            colo: Some("SJC".to_string()),
        }
    }

    #[test]
    fn store_should_insert_running_record_and_query_masked_ip() {
        let store = SpeedStore::in_memory().expect("store should initialize");
        let saved = store
            .save_result(sample_input(ResultStatus::Running))
            .expect("save should work");

        let records = store
            .query_results(QueryFilter::default())
            .expect("query should work");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, saved.id);
        assert_eq!(records[0].client_ip, "1.2.3.*");
    }

    #[test]
    fn store_should_update_existing_record_with_token() {
        let store = SpeedStore::in_memory().expect("store should initialize");
        let saved = store
            .save_result(sample_input(ResultStatus::Running))
            .expect("save should work");
        let mut update = sample_input(ResultStatus::Completed);
        update.id = Some(saved.id);
        update.update_token = Some(saved.update_token);
        update.final_download_mbps = Some(120.0);

        store.save_result(update).expect("update should work");
        let records = store
            .query_results(QueryFilter::default())
            .expect("query should work");

        assert_eq!(records[0].status, ResultStatus::Completed);
        assert_eq!(records[0].final_download_mbps, Some(120.0));
    }

    #[test]
    fn store_should_reject_update_when_token_is_wrong() {
        let store = SpeedStore::in_memory().expect("store should initialize");
        let saved = store
            .save_result(sample_input(ResultStatus::Running))
            .expect("save should work");
        let mut update = sample_input(ResultStatus::Completed);
        update.id = Some(saved.id);
        update.update_token = Some("wrong-token".to_string());

        let err = store
            .save_result(update)
            .expect_err("wrong token should fail");

        assert!(err.to_string().contains("更新令牌无效"));
    }
}
