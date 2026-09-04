//! simple - simple-upload-server 文件存取命令行客户端。
//!
//! 子命令:file upload / file ls / file download / file pull,
//! 通过 HTTP 调用服务端 /files、/file 接口,默认连接 https://jr.devcloud.woa.com。

use std::env;
use std::fs;
use std::io::{self, Cursor, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use reqwest::Url;
use reqwest::blocking::{Client, multipart};
use serde_json::{Value, json};

const DEFAULT_SERVER: &str = "https://jr.devcloud.woa.com";

const USAGE: &str = "\
simple - simple-upload-server 文件存取 CLI

用法:
  simple [--server <URL>] [--insecure] file upload   <NAMESPACE> <FILE>...
  simple [--server <URL>] [--insecure] file ls       [NAMESPACE]
  simple [--server <URL>] [--insecure] file download <NAMESPACE> <FILENAME> [-o <OUT>]
  simple [--server <URL>] [--insecure] file pull     [NAMESPACE] [-o <DIR>]

参数:
  NAMESPACE   目录路径,按 / 分层(如 a/b/c);写 . 或留空表示根目录
  FILE        要上传的本地文件,可多个
  -o OUT      下载保存到 OUT 文件(仅 download);pull 时保存到 DIR 目录
  --server    服务器地址,默认读环境变量 SIMPLE_SERVER,再默认 https://jr.devcloud.woa.com
  --insecure  跳过 TLS 证书校验(私有 CA/自签名证书时使用)

示例:
  simple file upload jr/lxy ./a.pdf ./b.png
  simple file ls jr/lxy
  simple file download jr/lxy a.pdf -o a.pdf
  simple file pull jr/lxy -o ./backup";

fn main() -> ExitCode {
    let mut it = env::args().skip(1);
    let mut server = env::var("SIMPLE_SERVER").unwrap_or_else(|_| DEFAULT_SERVER.to_string());
    let mut insecure = false;
    let mut args: Vec<String> = Vec::new();
    while let Some(a) = it.next() {
        if a == "--server" {
            server = it
                .next()
                .unwrap_or_else(|| usage_err("--server 缺少 URL 参数"));
        } else if let Some(v) = a.strip_prefix("--server=") {
            server = v.to_string();
        } else if a == "--insecure" {
            insecure = true;
        } else {
            args.push(a);
        }
    }

    let client = Client::builder()
        .danger_accept_invalid_certs(insecure)
        .build()
        .unwrap_or_else(|e| {
            eprintln!("初始化 HTTP 客户端失败: {e}");
            std::process::exit(1)
        });

    match args.first().map(String::as_str) {
        Some("file") => {
            let (sub, rest) = split_args(&args);
            match sub {
                Some("upload") => cmd_upload(&client, &server, rest),
                Some("ls") => cmd_ls(&client, &server, rest),
                Some("download") => cmd_download(&client, &server, rest),
                Some("pull") => cmd_pull(&client, &server, rest),
                Some(other) => usage_err(&format!("未知子命令 file {other}")),
                None => usage_err("缺少子命令(file upload / ls / download / pull)"),
            }
        }
        _ => usage_err("未知命令,目前仅支持 file"),
    }
}

fn split_args(args: &[String]) -> (Option<&str>, &[String]) {
    if args.len() < 2 {
        (None, &[])
    } else {
        (Some(&args[1]), &args[2..])
    }
}

/// 提取形如 `-o <值>` 的选项,剩余参数原序返回。
fn take_flag(args: &[String], flag: &str) -> (Vec<String>, Option<String>) {
    let mut rest = Vec::new();
    let mut value = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            value = args.get(i + 1).cloned().or_else(|| usage_err(&format!("{flag} 缺少值")));
            i += 2;
        } else {
            rest.push(args[i].clone());
            i += 1;
        }
    }
    (rest, value)
}

fn usage_err(msg: &str) -> ! {
    eprintln!("错误: {msg}\n");
    eprintln!("{USAGE}");
    std::process::exit(2)
}

/// NAMESPACE 文本转路径段:"." / 空串表示根目录,否则按 / 分层。
fn parse_ns(ns: &str) -> Vec<String> {
    if ns.is_empty() || ns == "." {
        Vec::new()
    } else {
        ns.split('/').filter(|s| !s.is_empty()).map(str::to_string).collect()
    }
}

fn file_url(server: &str, ns: &[String], filename: Option<&str>) -> Url {
    let mut url = Url::parse(server).unwrap_or_else(|e| usage_err(&format!("服务器地址无效: {e}")));
    {
        let mut seg = match url.path_segments_mut() {
            Ok(s) => s,
            Err(()) => usage_err("服务器地址不能带查询参数(query)"),
        };
        seg.extend(["file"]).extend(ns);
        if let Some(f) = filename {
            seg.push(f);
        }
    }
    url
}

fn http_err(resp: reqwest::blocking::Response) -> Result<reqwest::blocking::Response, String> {
    if resp.status().is_success() {
        Ok(resp)
    } else {
        let status = resp.status();
        Err(resp
            .json::<Value>()
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or_else(|| format!("HTTP {status}")))
    }
}

fn cmd_upload(client: &Client, server: &str, args: &[String]) -> ExitCode {
    if args.len() < 2 {
        usage_err("file upload 需要 <NAMESPACE> <FILE>...");
    }
    let ns = parse_ns(&args[0]);

    let mut form = multipart::Form::new();
    for f in &args[1..] {
        let part = match multipart::Part::file(f) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("读取 {} 失败: {e}", f);
                return ExitCode::from(1);
            }
        };
        form = form.part("file", part);
    }
    form = form.text("json", json!({ "namespace": ns }).to_string());

    let resp = match client.post(format!("{server}/files")).multipart(form).send() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("请求失败: {e}");
            return ExitCode::from(1);
        }
    };
    let resp = match http_err(resp) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("上传失败: {e}");
            return ExitCode::from(1);
        }
    };

    let v: Value = resp.json().unwrap_or(Value::Null);
    let mut failed = false;
    if let Some(paths) = v.get("paths").and_then(|p| p.as_array()) {
        for p in paths {
            println!("{}", p.as_str().unwrap_or("?"));
        }
    }
    if let Some(errs) = v.get("errors").and_then(|e| e.as_array()) {
        failed = true;
        for e in errs {
            let name = e.get("name").and_then(|n| n.as_str()).unwrap_or("?");
            let msg = e.get("error").and_then(|m| m.as_str()).unwrap_or("?");
            eprintln!("{name}: {msg}");
        }
    }
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn cmd_ls(client: &Client, server: &str, args: &[String]) -> ExitCode {
    if args.len() > 1 {
        usage_err("file ls 最多接受一个 NAMESPACE 参数");
    }
    let ns = args.first().map(|s| parse_ns(s)).unwrap_or_default();
    let url = file_url(server, &ns, None);

    let resp = match client.get(url).send() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("请求失败: {e}");
            return ExitCode::from(1);
        }
    };
    let resp = match http_err(resp) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ls 失败: {e}");
            return ExitCode::from(1);
        }
    };
    let v: Value = resp.json().unwrap_or(Value::Null);
    if let Some(files) = v.get("files").and_then(|f| f.as_array()) {
        for f in files {
            println!("{}", f.as_str().unwrap_or("?"));
        }
    }
    ExitCode::SUCCESS
}

fn cmd_download(client: &Client, server: &str, args: &[String]) -> ExitCode {
    let (pos, out) = take_flag(args, "-o");
    if pos.len() < 2 {
        usage_err("file download 需要 <NAMESPACE> <FILENAME> [-o <OUT>]");
    }
    let ns = parse_ns(&pos[0]);
    let url = file_url(server, &ns, Some(&pos[1]));

    let resp = match client.get(url).send() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("请求失败: {e}");
            return ExitCode::from(1);
        }
    };
    let resp = match http_err(resp) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("下载失败: {e}");
            return ExitCode::from(1);
        }
    };

    // 服务端对"路径指向目录"返回 JSON 列表而非文件流,给出提示而非写脏数据。
    let is_json = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("application/json"))
        .unwrap_or(false);
    if is_json {
        eprintln!("{} 是一个目录而非文件,请指定文件名,或用 ls 查看内容", pos[1]);
        return ExitCode::from(1);
    }

    let bytes = match resp.bytes() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("读取响应失败: {e}");
            return ExitCode::from(1);
        }
    };
    if let Some(out) = &out {
        fs::write(out, &bytes).unwrap_or_else(|e| {
            eprintln!("写入 {} 失败: {e}", out);
            std::process::exit(1)
        });
        eprintln!("已保存到 {} ({} 字节)", out, bytes.len());
    } else {
        io::stdout().write_all(&bytes).unwrap_or_else(|e| {
            eprintln!("写入标准输出失败: {e}");
            std::process::exit(1)
        });
        io::stdout().flush().ok();
    }
    ExitCode::SUCCESS
}

fn cmd_pull(client: &Client, server: &str, args: &[String]) -> ExitCode {
    let (pos, dir) = take_flag(args, "-o");
    if pos.len() > 1 {
        usage_err("file pull 最多接受一个 NAMESPACE 参数");
    }
    let ns = pos.first().map(|s| parse_ns(s)).unwrap_or_default();
    let dir = PathBuf::from(dir.unwrap_or_else(|| ".".to_string()));

    let list_url = file_url(server, &ns, None);
    let resp = match client.get(list_url).send() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("请求失败: {e}");
            return ExitCode::from(1);
        }
    };
    let resp = match http_err(resp) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("pull 失败: {e}");
            return ExitCode::from(1);
        }
    };
    let v: Value = resp.json().unwrap_or(Value::Null);
    let files: Vec<String> = v
        .get("files")
        .and_then(|f| f.as_array())
        .map(|a| a.iter().filter_map(|f| f.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    if files.is_empty() {
        println!("该目录为空,无文件可拉取");
        return ExitCode::SUCCESS;
    }

    let resp = match client
        .post(format!("{server}/files"))
        .json(&json!({ "files": files }))
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("请求失败: {e}");
            return ExitCode::from(1);
        }
    };
    let resp = match http_err(resp) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("批量下载失败: {e}");
            return ExitCode::from(1);
        }
    };
    let bytes = match resp.bytes() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("读取响应失败: {e}");
            return ExitCode::from(1);
        }
    };

    let mut archive = match zip::ZipArchive::new(Cursor::new(bytes)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("解压失败: {e}");
            return ExitCode::from(1);
        }
    };
    let mut count = 0usize;
    for i in 0..archive.len() {
        let mut zf = match archive.by_index(i) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("读取 zip 条目失败: {e}");
                return ExitCode::from(1);
            }
        };
        let Some(rel) = zf.enclosed_name() else {
            eprintln!("跳过不安全的 zip 条目: {}", zf.name());
            continue;
        };
        let dst = dir.join(&rel);
        if zf.is_dir() {
            fs::create_dir_all(&dst).unwrap_or_else(|e| {
                eprintln!("创建目录 {} 失败: {e}", dst.display());
                std::process::exit(1)
            });
            continue;
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).unwrap_or_else(|e| {
                eprintln!("创建目录 {} 失败: {e}", parent.display());
                std::process::exit(1)
            });
        }
        let mut out = match fs::File::create(&dst) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("创建 {} 失败: {e}", dst.display());
                return ExitCode::from(1);
            }
        };
        if let Err(e) = io::copy(&mut zf, &mut out) {
            eprintln!("写入 {} 失败: {e}", dst.display());
            return ExitCode::from(1);
        }
        count += 1;
    }
    println!("已拉取 {count} 个文件到 {}", dir.display());
    ExitCode::SUCCESS
}