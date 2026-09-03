use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::{
    body::{Body, Bytes},
    extract::{
        multipart::Field, DefaultBodyLimit, FromRequest, Multipart, Request, State,
    },
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use axum::extract::Path as UrlPath;
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};
use tokio::{fs, io::AsyncWriteExt};
use tokio_util::io::ReaderStream;

const MAX_JSON_BYTES: usize = 1024 * 1024;
const MAX_BATCH_DOWNLOAD_FILES: usize = 1000;
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
        "upload server listening on http://127.0.0.1:{port}, root: {},\n         PUT/GET /file/{{ns...}}/{{filename}}, POST /files (multipart=batch upload, json=batch download zip)",
        config.root.display(),
    );

    let app = Router::new()
        .route("/files", axum::routing::post(post_files))
        .route("/file", get(list_all))
        .route("/file/{*rest}", get(file_get).put(file_put))
        .with_state(config)
        // 不用 axum 的 body 限制,由 handler 流式计数控制
        .layer(DefaultBodyLimit::disable());

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("failed to bind");
    axum::serve(listener, app).await.expect("server failed");
}

// ---------- POST /files:Content-Type 分派 ----------

async fn post_files(
    State(cfg): State<AppConfig>,
    headers: HeaderMap,
    req: Request,
) -> Result<Response, ApiError> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ct.starts_with("multipart/form-data") {
        let multipart = Multipart::from_request(req, &())
            .await
            .map_err(|e| ApiError(415, format!("invalid multipart request: {e}")))?;
        upload_batch(&cfg, multipart)
            .await
            .map(IntoResponse::into_response)
    } else if ct.starts_with("application/json") || ct.contains("+json") {
        let value = Json::<Value>::from_request(req, &())
            .await
            .map_err(|e| ApiError::bad(format!("invalid json body: {e}")))?;
        batch_download(&cfg, value).await
    } else {
        Err(ApiError(
            415,
            "unsupported content-type; use multipart/form-data (batch upload) or application/json (batch download)".to_string(),
        ))
    }
}

// ---------- 批量上传(multipart,含旧 /upload) ----------

async fn upload_batch(cfg: &AppConfig, mut multipart: Multipart) -> ApiResult {
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

// ---------- 单个上传 PUT /file/{ns...}/{filename} ----------

async fn file_put(
    State(cfg): State<AppConfig>,
    UrlPath(rest): UrlPath<String>,
    body: Body,
) -> ApiResult {
    let rest = rest.trim_end_matches('/').to_string();
    let segments = split_segments(&rest)?;
    let (filename, ns_segments) = segments
        .split_last()
        .ok_or_else(|| ApiError::bad("missing filename in path"))?;
    let filename = clean_filename(Some(filename))?;
    for seg in ns_segments {
        if !valid_namespace_segment(seg) {
            return Err(ApiError::bad(format!("invalid namespace segment: {seg:?}")));
        }
    }

    // 流式写入临时文件
    let tmp = cfg.root.join(format!(".upload-{}.tmp", uuid::Uuid::new_v4()));
    let mut dest = fs::File::create(&tmp)
        .await
        .map_err(|e| ApiError(500, format!("create tmp file failed: {e}")))?;
    let mut stream = body.into_data_stream();
    let mut written: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| ApiError(500, format!("read body failed: {e}")))?;
        written += chunk.len() as u64;
        if written > cfg.max_file_bytes {
            let _ = fs::remove_file(&tmp).await;
            return Err(ApiError(413, "upload exceeds size limit".to_string()));
        }
        if let Err(e) = dest.write_all(&chunk).await {
            let _ = fs::remove_file(&tmp).await;
            return Err(ApiError(500, format!("write file failed: {e}")));
        }
    }
    drop(dest);

    let rel = store_tmp(&cfg, ns_segments, &filename, &tmp).await?;
    Ok(Json(json!({ "ok": true, "path": rel })))
}

// ---------- 单个下载 / 目录树 GET /file/{*rest} ----------

async fn list_all(State(cfg): State<AppConfig>) -> Result<Response, ApiError> {
    list_tree(&cfg, &cfg.root).await
}

async fn file_get(
    State(cfg): State<AppConfig>,
    UrlPath(rest): UrlPath<String>,
) -> Result<Response, ApiError> {
    // `/file/` 的空 rest 视为整树列表
    let rest = rest.trim_end_matches('/').to_string();
    if rest.is_empty() {
        return list_tree(&cfg, &cfg.root).await;
    }
    let segments = split_segments(&rest)?;
    let mut target = cfg.root.clone();
    for seg in &segments {
        target.push(seg);
    }
    match fs::metadata(&target).await {
        Ok(m) if m.is_file() => download_file(&target, m.len()).await,
        Ok(m) if m.is_dir() => list_tree(&cfg, &target).await,
        Ok(_) => Err(ApiError(404, "not found".to_string())),
        Err(_) => Err(ApiError(404, "not found".to_string())),
    }
}

async fn list_tree(cfg: &AppConfig, dir: &Path) -> Result<Response, ApiError> {
    let mut files = Vec::new();
    collect_files(dir, &cfg.root, &mut files)
        .await
        .map_err(|e| ApiError(500, format!("list failed: {e}")))?;
    files.sort();
    Ok(Json(json!({ "ok": true, "files": files })).into_response())
}

async fn collect_files(dir: &Path, root: &Path, out: &mut Vec<String>) -> io::Result<()> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(cur) = stack.pop() {
        let mut rd = fs::read_dir(&cur).await?;
        while let Some(entry) = rd.next_entry().await? {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue; // 隐藏文件与上传临时文件
            }
            let path = entry.path();
            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                let rel = path
                    .strip_prefix(root)
                    .expect("path is always under root")
                    .to_string_lossy()
                    .to_string();
                out.push(rel);
            }
        }
    }
    Ok(())
}

async fn download_file(path: &Path, len: u64) -> Result<Response, ApiError> {
    let file = fs::File::open(path)
        .await
        .map_err(|e| ApiError(500, format!("open failed: {e}")))?;
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("download");
    let encoded = rfc5987_encode(filename);
    let disposition = format!("attachment; filename=\"download\"; filename*=UTF-8''{encoded}");
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition).map_err(|_| ApiError(500, "invalid content-disposition".to_string()))?,
    );
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&len.to_string()).expect("length is a valid header value"),
    );
    Ok((headers, Body::from_stream(ReaderStream::new(file))).into_response())
}

// ---------- 批量下载 POST /files (application/json) ----------

async fn batch_download(cfg: &AppConfig, body: Json<Value>) -> Result<Response, ApiError> {
    let arr = body
        .0
        .get("files")
        .and_then(|f| f.as_array())
        .ok_or_else(|| ApiError::bad("json must contain \"files\" array"))?;
    if arr.len() > MAX_BATCH_DOWNLOAD_FILES {
        return Err(ApiError::bad(format!(
            "too many files (max {MAX_BATCH_DOWNLOAD_FILES})"
        )));
    }
    let mut entries = Vec::with_capacity(arr.len());
    for item in arr {
        let s = item
            .as_str()
            .ok_or_else(|| ApiError::bad("files items must be strings"))?;
        split_segments(s)?; // 路径穿越校验
        entries.push(s.to_string());
    }

    // 打包前整体校验存在性,避免静默缺文件
    let mut missing = Vec::new();
    for rel in &entries {
        let abs = cfg.root.join(rel);
        match fs::metadata(&abs).await {
            Ok(m) if m.is_file() => {}
            _ => missing.push(rel.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(ApiError(
            404,
            format!("files not found: {}", missing.join(", ")),
        ));
    }

    let tmp = cfg.root.join(format!(".batch-{}.zip", uuid::Uuid::new_v4()));
    let zip_tmp = tmp.clone();
    let root = cfg.root.clone();
    let entries = entries.clone();
    tokio::task::spawn_blocking(move || -> io::Result<()> {
        let file = std::fs::File::create(&zip_tmp)?;
        let mut writer = zip::ZipWriter::new(io::BufWriter::new(file));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for rel in &entries {
            writer.start_file(rel, options)?;
            let mut src = std::fs::File::open(root.join(rel))?;
            io::copy(&mut src, &mut writer)?;
        }
        writer.finish()?;
        Ok(())
    })
    .await
    .map_err(|e| ApiError(500, format!("zip task failed: {e}")))?
    .map_err(|e| ApiError(500, format!("zip failed: {e}")))?;

    let len = fs::metadata(&tmp)
        .await
        .map_err(|e| ApiError(500, format!("zip stat failed: {e}")))?
        .len();
    let file = fs::File::open(&tmp)
        .await
        .map_err(|e| ApiError(500, format!("zip open failed: {e}")))?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"files.zip\""),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&len.to_string()).expect("length is a valid header value"),
    );
    let stream = CleanupStream {
        inner: ReaderStream::new(file),
        path: tmp,
    };
    Ok((headers, Body::from_stream(stream)).into_response())
}

/// 输出结束后自动删除临时 zip 的流包装
struct CleanupStream {
    inner: ReaderStream<fs::File>,
    path: PathBuf,
}

impl Stream for CleanupStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(None) => {
                let _ = std::fs::remove_file(&self.path);
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

impl Drop for CleanupStream {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------- 公共工具 ----------

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
            return Err(ApiError(413, "upload exceeds size limit".to_string()));
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

async fn store_tmp(
    cfg: &AppConfig,
    ns_segments: &[String],
    filename: &str,
    tmp: &Path,
) -> Result<String, ApiError> {
    let mut dir = cfg.root.clone();
    for item in ns_segments {
        dir.push(item);
    }
    if let Err(e) = fs::create_dir_all(&dir).await {
        let _ = fs::remove_file(tmp).await;
        return Err(ApiError(500, format!("create dir failed: {e}")));
    }
    let dest = dir.join(filename);
    if let Err(e) = fs::rename(tmp, &dest).await {
        let _ = fs::remove_file(tmp).await;
        return Err(ApiError(500, format!("store file failed: {e}")));
    }
    Ok(dest
        .strip_prefix(&cfg.root)
        .expect("dest is always under root")
        .to_string_lossy()
        .to_string())
}

/// 拆分 URL 路径段并拒绝任何可穿越/歧义段
fn split_segments(path: &str) -> Result<Vec<String>, ApiError> {
    let mut out = Vec::new();
    for seg in path.split('/') {
        if seg.is_empty() {
            return Err(ApiError::bad("empty path segment"));
        }
        if seg == "." || seg == ".." || seg.contains('\\') {
            return Err(ApiError::bad(format!("invalid path segment: {seg:?}")));
        }
        out.push(seg.to_string());
    }
    Ok(out)
}

fn valid_namespace_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
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
        if !valid_namespace_segment(s) {
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

/// RFC 5987(Content-Disposition filename*)属性字符编码
fn rfc5987_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}