use std::collections::HashSet;
use std::path::{Path, PathBuf};

use axum::{
    extract::{multipart::Field, DefaultBodyLimit, Multipart, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use tokio::{fs, io::AsyncWriteExt};

const MAX_JSON_BYTES: usize = 1024 * 1024;
const DEFAULT_MAX_FILE_MB: u64 = 512;
const DEFAULT_MAX_TOTAL_MB: u64 = 1024;
const DEFAULT_MAX_FILES: usize = 50;

#[derive(Clone)]
struct AppConfig {
    root: PathBuf,
    max_file_bytes: u64,
    max_total_bytes: u64,
    max_files: usize,
}

struct ApiError(u16, String);

impl ApiError {
    fn bad(msg: impl Into<String>) -> Self {
        Self(400, msg.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = StatusCode::from_u16(self.0).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (code, Json(json!({ "ok": false, "error": self.1 }))).into_response()
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() {
    let root = std::env::var("UPLOAD_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/data/home/nebula/upload-store"));
    let port: u16 = env_parse("UPLOAD_PORT", 8765);
    fs::create_dir_all(&root)
        .await
        .expect("failed to create upload root");
    let root = root.canonicalize().expect("failed to canonicalize root");
    let config = AppConfig {
        max_file_bytes: env_parse("UPLOAD_MAX_FILE_MB", DEFAULT_MAX_FILE_MB) * 1024 * 1024,
        max_total_bytes: env_parse("UPLOAD_MAX_TOTAL_MB", DEFAULT_MAX_TOTAL_MB) * 1024 * 1024,
        max_files: env_parse("UPLOAD_MAX_FILES", DEFAULT_MAX_FILES),
        root,
    };

    println!(
        "upload server listening on http://127.0.0.1:{port}, root: {}, max {} files / {}MB each / {}MB total",
        config.root.display(),
        config.max_files,
        config.max_file_bytes / 1024 / 1024,
        config.max_total_bytes / 1024 / 1024,
    );

    let app = Router::new()
        .route("/upload", post(upload))
        .with_state(config)
        // 不用 axum 的 body 限制,由 handler 流式计数控制
        .layer(DefaultBodyLimit::disable());

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("failed to bind");
    axum::serve(listener, app).await.expect("server failed");
}

async fn upload(State(cfg): State<AppConfig>, mut multipart: Multipart) -> ApiResult {
    let mut namespace: Option<Vec<String>> = None;
    let mut files: Vec<(String, PathBuf)> = Vec::new(); // (clean filename, tmp path)
    let mut total_written: u64 = 0;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(e) => {
                abort_upload(&files).await;
                return Err(ApiError::bad(format!("invalid multipart: {e}")));
            }
        };

        match field.name().unwrap_or("") {
            "json" => {
                if namespace.is_some() {
                    abort_upload(&files).await;
                    return Err(ApiError::bad("duplicate json part"));
                }
                let text = match read_json_field(field).await {
                    Ok(text) => text,
                    Err(e) => {
                        abort_upload(&files).await;
                        return Err(e);
                    }
                };
                let parsed: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        abort_upload(&files).await;
                        return Err(ApiError::bad(format!("invalid json: {e}")));
                    }
                };
                namespace = match parse_namespace(&parsed) {
                    Ok(ns) => Some(ns),
                    Err(e) => {
                        abort_upload(&files).await;
                        return Err(e);
                    }
                };
            }
            "file" => {
                if files.len() >= cfg.max_files {
                    abort_upload(&files).await;
                    return Err(ApiError::bad(format!(
                        "too many files (max {})",
                        cfg.max_files
                    )));
                }
                let filename = match clean_filename(field.file_name()) {
                    Ok(name) => name,
                    Err(e) => {
                        abort_upload(&files).await;
                        return Err(e);
                    }
                };
                let tmp = cfg.root.join(format!(".upload-{}.tmp", uuid::Uuid::new_v4()));
                let budget = cfg
                    .max_file_bytes
                    .min(cfg.max_total_bytes.saturating_sub(total_written));
                match stream_file(field, &tmp, budget).await {
                    Ok(written) => {
                        total_written += written;
                        files.push((filename, tmp));
                    }
                    Err(e) => {
                        abort_upload(&files).await;
                        return Err(e);
                    }
                }
            }
            // 未知字段:消费掉内容后忽略
            _ => {
                let mut field = field;
                while let Ok(Some(_)) = field.chunk().await {}
            }
        }
    }

    let namespace = match namespace {
        Some(ns) => ns,
        None => {
            abort_upload(&files).await;
            return Err(ApiError::bad(
                "missing required json part with \"namespace\" field",
            ));
        }
    };
    if files.is_empty() {
        return Err(ApiError::bad("missing file part"));
    }

    // 按 namespace 列表递归创建目录,整批文件共享
    let mut dir = cfg.root.clone();
    for item in &namespace {
        dir.push(item);
    }
    if let Err(e) = fs::create_dir_all(&dir).await {
        abort_upload(&files).await;
        return Err(ApiError(500, format!("create dir failed: {e}")));
    }

    // 逐个落到目标;失败的文件记录 error,已成功的保留(尽力而为)
    let mut paths = Vec::new();
    let mut errors = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (filename, tmp) in &files {
        if !seen.insert(filename.clone()) {
            errors.push(json!({ "name": filename, "error": "duplicate filename in batch" }));
            let _ = fs::remove_file(tmp).await;
            continue;
        }
        let dest = dir.join(filename);
        match fs::rename(tmp, &dest).await {
            Ok(()) => {
                let rel = dest
                    .strip_prefix(&cfg.root)
                    .expect("dest is always under root")
                    .to_string_lossy()
                    .to_string();
                paths.push(rel);
            }
            Err(e) => {
                let _ = fs::remove_file(tmp).await;
                errors.push(json!({ "name": filename, "error": e.to_string() }));
            }
        }
    }

    if errors.is_empty() {
        Ok(Json(json!({ "ok": true, "paths": paths })))
    } else {
        Ok(Json(json!({ "ok": false, "paths": paths, "errors": errors })))
    }
}

async fn read_json_field(field: Field<'_>) -> Result<String, ApiError> {
    // json 字段内容量小(1MB 上限),允许全量载入内存
    let text = field
        .text()
        .await
        .map_err(|e| ApiError::bad(format!("read json failed: {e}")))?;
    if text.len() > MAX_JSON_BYTES {
        return Err(ApiError::bad("json part too large"));
    }
    Ok(text)
}

async fn stream_file(mut field: Field<'_>, tmp: &Path, budget: u64) -> Result<u64, ApiError> {
    let mut dest = fs::File::create(tmp)
        .await
        .map_err(|e| ApiError(500, format!("create tmp file failed: {e}")))?;
    let mut written: u64 = 0;
    loop {
        let chunk = field.chunk().await.map_err(|e| {
            let _ = fs::remove_file(tmp);
            ApiError::bad(format!("read file failed: {e}"))
        })?;
        let Some(data) = chunk else { break };
        written += data.len() as u64;
        if written > budget {
            let _ = fs::remove_file(tmp).await;
            return Err(ApiError(
                413,
                "upload exceeds size limit (single file or total)".to_string(),
            ));
        }
        if let Err(e) = dest.write_all(&data).await {
            let _ = fs::remove_file(tmp).await;
            return Err(ApiError(500, format!("write file failed: {e}")));
        }
    }
    Ok(written)
}

async fn abort_upload(files: &[(String, PathBuf)]) {
    for (_, tmp) in files {
        let _ = fs::remove_file(tmp).await;
    }
}

fn parse_namespace(v: &Value) -> Result<Vec<String>, ApiError> {
    let arr = v
        .get("namespace")
        .and_then(|n| n.as_array())
        .ok_or_else(|| ApiError::bad("json must contain \"namespace\" array"))?;
    let mut items = Vec::with_capacity(arr.len());
    for item in arr {
        let s = item
            .as_str()
            .ok_or_else(|| ApiError::bad("namespace items must be strings"))?;
        let valid = !s.is_empty()
            && s != "."
            && s != ".."
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !valid {
            return Err(ApiError::bad(format!("invalid namespace item: {s:?}")));
        }
        items.push(s.to_string());
    }
    Ok(items)
}

fn clean_filename(name: Option<&str>) -> Result<String, ApiError> {
    let raw = name.ok_or_else(|| ApiError::bad("file part missing filename"))?;
    // 只取 basename,去除客户端可能带上的目录部分
    let base = Path::new(raw)
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| ApiError::bad("invalid filename"))?;
    let valid = !base.is_empty()
        && base != "."
        && base != ".."
        && !base.contains('/')
        && !base.contains('\\');
    if !valid {
        return Err(ApiError::bad(format!("invalid filename: {raw:?}")));
    }
    Ok(base.to_string())
}