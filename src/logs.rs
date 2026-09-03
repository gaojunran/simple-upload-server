use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::{
    extract::{FromRequest, Query, Request, State},
    Json,
};
use rusqlite::{params, params_from_iter, types::Value as SqlValue, Connection};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{valid_namespace_segment, ApiError, AppConfig};

const MAX_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_BATCH: usize = 1000;

#[derive(Clone)]
pub struct LogStore {
    conn: Arc<Mutex<Connection>>,
}

impl LogStore {
    pub fn open(db_path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS logs (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 ts INTEGER NOT NULL,
                 namespace TEXT NOT NULL,
                 message TEXT NOT NULL,
                 raw TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_logs_ts ON logs(ts);
             CREATE INDEX IF NOT EXISTS idx_logs_namespace ON logs(namespace);",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn insert_batch(&self, entries: &[ParsedLog]) -> Result<(), rusqlite::Error> {
        let mut conn = self.conn.lock().expect("log store poisoned");
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO logs (ts, namespace, message, raw) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for e in entries {
                stmt.execute(params![e.ts, e.namespace, e.message, e.raw])?;
            }
        }
        tx.commit()
    }

    fn query(&self, args: &QueryArgs) -> Result<(i64, Vec<Value>), rusqlite::Error> {
        let mut values: Vec<SqlValue> = Vec::new();
        let mut where_sql = String::new();
        compile_where(args, &mut where_sql, &mut values);

        let conn = self.conn.lock().expect("log store poisoned");
        let mut count_stmt = conn.prepare(&format!("SELECT COUNT(*) FROM logs{where_sql}"))?;
        let mut count_rows = count_stmt.query(params_from_iter(values.iter().cloned()))?;
        let total: i64 = count_rows
            .next()?
            .map(|r| r.get(0))
            .transpose()?
            .unwrap_or(0);

        let dir = if args.asc { "ASC" } else { "DESC" };
        let sql = format!(
            "SELECT raw FROM logs{where_sql} ORDER BY ts {dir}, id {dir} LIMIT {} OFFSET {}",
            args.limit, args.offset
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(values))?;
        let mut logs = Vec::new();
        while let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                logs.push(v);
            }
        }
        Ok((total, logs))
    }
}

/// 统一 Json 提取失败响应的 body 提取器:错误 Content-Type 回 415、非法 body 回 400,
/// 均返回与其余接口一致的 {"ok":false,...} JSON 错误体。
pub struct JsonBody(pub Value);

impl<S> FromRequest<S> for JsonBody
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(v) = Json::<Value>::from_request(req, state)
            .await
            .map_err(|e| match e {
                axum::extract::rejection::JsonRejection::MissingJsonContentType(_) => {
                    ApiError(415, "expected request with Content-Type: application/json".into())
                }
                other => ApiError::bad(format!("invalid json body: {other}")),
            })?;
        Ok(Self(v))
    }
}

pub async fn post_logs(
    State(cfg): State<AppConfig>,
    body: JsonBody,
) -> Result<Json<Value>, ApiError> {
    let body = body.0;    let entries: Vec<&Value> = match &body {
        Value::Array(arr) => {
            if arr.is_empty() {
                return Err(ApiError::bad("empty log batch"));
            }
            if arr.len() > MAX_BATCH {
                return Err(ApiError::bad(
                    "too many logs in one request (max 1000)",
                ));
            }
            arr.iter().collect()
        }
        Value::Object(_) => vec![&body],
        _ => {
            return Err(ApiError::bad(
                "body must be a log object or an array of log objects",
            ))
        }
    };
    let parsed: Vec<ParsedLog> = entries
        .iter()
        .enumerate()
        .map(|(i, v)| parse_log_entry(i, v))
        .collect::<Result<_, _>>()?;
    cfg.log_store
        .insert_batch(&parsed)
        .map_err(|e| ApiError::internal(format!("store failed: {e}")))?;
    Ok(Json(json!({ "ok": true, "count": parsed.len() })))
}

#[derive(Deserialize)]
pub struct LogQuery {
    namespace: Option<String>,
    keyword: Option<String>,
    start: Option<String>,
    end: Option<String>,
    op: Option<String>,
    order: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

pub async fn get_logs(
    State(cfg): State<AppConfig>,
    Query(q): Query<LogQuery>,
) -> Result<Json<Value>, ApiError> {
    let is_or = match q.op.as_deref() {
        None | Some("and") => false,
        Some("or") => true,
        Some(other) => {
            return Err(ApiError::bad(format!(
                "op must be \"and\" or \"or\", got {other:?}"
            )))
        }
    };
    let asc = match q.order.as_deref() {
        None | Some("desc") => false,
        Some("asc") => true,
        Some(other) => {
            return Err(ApiError::bad(format!(
                "order must be \"asc\" or \"desc\", got {other:?}"
            )))
        }
    };
    if let Some(ns) = &q.namespace {
        if !valid_namespace_segment(ns) {
            return Err(ApiError::bad(format!("invalid namespace: {ns:?}")));
        }
    }
    let start = match &q.start {
        Some(s) => Some(parse_ts_param(s).map_err(ApiError::bad)?),
        None => None,
    };
    let end = match &q.end {
        Some(s) => Some(parse_ts_param(s).map_err(ApiError::bad)?),
        None => None,
    };
    let args = QueryArgs {
        namespace: q.namespace,
        keyword: q.keyword,
        start,
        end,
        is_or,
        asc,
        limit: q.limit.unwrap_or(100).min(1000),
        offset: q.offset.unwrap_or(0),
    };
    let (total, logs) = cfg
        .log_store
        .query(&args)
        .map_err(|e| ApiError::internal(format!("query failed: {e}")))?;
    Ok(Json(json!({ "ok": true, "total": total, "logs": logs })))
}

struct QueryArgs {
    namespace: Option<String>,
    keyword: Option<String>,
    start: Option<i64>,
    end: Option<i64>,
    is_or: bool,
    asc: bool,
    limit: usize,
    offset: usize,
}

fn compile_where(args: &QueryArgs, sql: &mut String, values: &mut Vec<SqlValue>) {
    let mut conds: Vec<&str> = Vec::new();
    if let Some(ns) = &args.namespace {
        conds.push("namespace LIKE ? ESCAPE '\\'");
        values.push(SqlValue::Text(format!("{}%", like_escape(ns))));
    }
    if let Some(kw) = &args.keyword {
        conds.push("message LIKE ? ESCAPE '\\'");
        values.push(SqlValue::Text(format!("%{}%", like_escape(kw))));
    }
    if let Some(start) = args.start {
        conds.push("ts >= ?");
        values.push(SqlValue::Integer(start));
    }
    if let Some(end) = args.end {
        conds.push("ts <= ?");
        values.push(SqlValue::Integer(end));
    }
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(if args.is_or { " OR " } else { " AND " }));
    }
}

fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

struct ParsedLog {
    ts: i64,
    namespace: String,
    message: String,
    raw: String,
}

fn parse_log_entry(index: usize, v: &Value) -> Result<ParsedLog, ApiError> {
    let obj = v
        .as_object()
        .ok_or_else(|| ApiError::bad(format!("log #{index}: must be a json object")))?;
    let ts = obj
        .get("timestamp")
        .ok_or_else(|| ApiError::bad(format!("log #{index}: missing \"timestamp\"")))?;
    let ts = parse_timestamp(ts)
        .map_err(|e| ApiError::bad(format!("log #{index}: timestamp: {e}")))?;
    let namespace = obj
        .get("namespace")
        .and_then(|x| x.as_str())
        .ok_or_else(|| ApiError::bad(format!("log #{index}: missing \"namespace\" string")))?;
    if !valid_namespace_segment(namespace) {
        return Err(ApiError::bad(format!(
            "log #{index}: invalid namespace: {namespace:?}"
        )));
    }
    let message = obj
        .get("message")
        .and_then(|x| x.as_str())
        .ok_or_else(|| ApiError::bad(format!("log #{index}: missing \"message\" string")))?;
    if message.len() > MAX_MESSAGE_BYTES {
        return Err(ApiError::bad(format!(
            "log #{index}: message too long (max {MAX_MESSAGE_BYTES} bytes)"
        )));
    }
    Ok(ParsedLog {
        ts,
        namespace: namespace.to_string(),
        message: message.to_string(),
        raw: v.to_string(),
    })
}

fn parse_timestamp(v: &Value) -> Result<i64, String> {
    match v {
        Value::Number(n) => {
            let secs = n
                .as_i64()
                .ok_or_else(|| "must be an integer count of seconds".to_string())?;
            secs.checked_mul(1000)
                .ok_or_else(|| "timestamp out of range".to_string())
        }
        Value::String(s) => parse_rfc3339_millis(s),
        _ => Err("must be an RFC3339 string or unix seconds number".to_string()),
    }
}

fn parse_ts_param(s: &str) -> Result<i64, String> {
    if let Ok(secs) = s.parse::<i64>() {
        return secs
            .checked_mul(1000)
            .ok_or_else(|| "timestamp out of range".to_string());
    }
    parse_rfc3339_millis(s)
}

fn parse_rfc3339_millis(s: &str) -> Result<i64, String> {
    let b = s.as_bytes();
    let err = || format!("invalid RFC3339 timestamp: {s:?}");
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return Err(err());
    }
    let field = |r: std::ops::Range<usize>| -> Result<i64, String> {
        std::str::from_utf8(&b[r])
            .ok()
            .and_then(|x| x.parse::<i64>().ok())
            .ok_or_else(err)
    };
    let year = field(0..4)?;
    let month = field(5..7)?;
    let day = field(8..10)?;
    let hour = field(11..13)?;
    let minute = field(14..16)?;
    let second = field(17..19)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(err());
    }
    let days = days_from_civil(year, month, day);
    if civil_from_days(days) != (year as i32, month as u32, day as u32) {
        return Err(err()); // 非法日历日,如 2 月 30 日
    }
    let mut millis = (days * 86_400 + hour * 3600 + minute * 60 + second) * 1000;

    let mut i = 19;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let mut frac: i64 = 0;
        let mut scale: i64 = 100;
        let mut digits = 0;
        while i < b.len() && b[i].is_ascii_digit() && digits < 9 {
            if digits < 3 {
                frac += (b[i] - b'0') as i64 * scale;
                scale /= 10;
            }
            digits += 1;
            i += 1;
        }
        if digits == 0 {
            return Err(err());
        }
        millis += frac;
    }
    if i >= b.len() {
        return Err(err());
    }
    let (offset_secs, consumed) = match b[i] {
        b'Z' | b'z' => (0, i + 1),
        b'+' | b'-' => {
            if i + 6 > b.len() || b[i + 3] != b':' {
                return Err(err());
            }
            let oh = field(i + 1..i + 3)?;
            let om = field(i + 4..i + 6)?;
            if oh > 23 || om > 59 {
                return Err(err());
            }
            let sign = if b[i] == b'-' { -1 } else { 1 };
            (sign * (oh * 3600 + om * 60), i + 6)
        }
        _ => return Err(err()),
    };
    if consumed != b.len() {
        return Err(err());
    }
    Ok(millis - offset_secs * 1000)
}

// Howard Hinnant 的 civil date 换算,校验 3.1 节
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}