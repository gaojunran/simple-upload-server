# simple-upload-server

低内存占用的简易文件上传服务器,单文件 Rust 二进制,流式落盘,实测常驻内存约 3 MB。

用于通用上传场景:给远程服务器传文件、备份归档、跨机器搬运文件等。定位是简单、可靠、够用,**不适用于复杂业务场景**(无鉴权、无分片断点、无对象存储、无配额管理)。

## 特性

- 单二进制,无外部依赖,内存占用恒定(文件内容流式写盘,不整文件载入内存)
- namespace 目录树:按 namespace 列表逐级建目录,天然做命名空间隔离;目录树可递归列出、按路径直接下载
- 批量上传(多文件一次请求)与批量下载(打包 zip)对称设计
- 字符集校验 + 路径成分剥离,杜绝路径穿越;失败路径清理临时文件
- 进程崩溃由外部(如 systemd)托管,`Restart=always` 即可自愈

## 接口契约

所有请求的存储位置由 URL 中的 `namespace` 段决定:URL 路径即存储目录树,`{"namespace":["a","b"]}` 与 `/file/a/b/` 指向同一个目录。

### 端点总览

| 方法 | 端点 | 语义 |
| --- | --- | --- |
| `PUT` | `/file/{ns...}/{filename}` | 单个上传,请求体即文件原始内容 |
| `GET` | `/file/{ns...}/{filename}` | 单个下载(流式) |
| `GET` | `/file/{ns...}` 或 `/file` | 递归文件列表(扁平相对路径数组) |
| `POST` | `/files` `multipart/form-data` | 批量上传 |
| `POST` | `/files` `application/json` | 批量下载(打包 zip) |

### PUT /file/{ns...}/{filename} — 单个上传

请求体是文件的原始字节(非 multipart),`Content-Type` 随意。URL 已包含完整存储位置。

```bash
curl -X PUT --data-binary @local.txt http://host:8765/file/a/b/local.txt
# => {"ok":true,"path":"a/b/local.txt"}
```

约束:namespace 段限 `[A-Za-z0-9._-]`;文件名取最后一段(不可含 `/` `\`)。中文等非 ASCII 文件名需 URL 百分号编码。

### GET /file/{ns...}/{filename} — 单个下载

流式返回文件内容,头:`Content-Type: application/octet-stream`(一律下载,不按扩展名猜类型)、`Content-Disposition: attachment; filename*=UTF-8''...`(RFC 5987,中文文件名不乱码)、`Content-Length`。

```bash
curl -OJ http://host:8765/file/a/b/local.txt   # 以 local.txt 保存
```

### GET /file/{ns...} — 目录树列表

`/file` 返回存储根下全部文件的递归列表,`/file/a/b` 返回该命名空间下的递归列表。扁平数组,路径相对根,可直接回填给批量下载。

```bash
curl http://host:8765/file
# => {"ok":true,"files":["a/b/local.txt","c.log",...]}
```

按"存在性"决定语义:路径指向文件 → 下载;指向目录 → 列表;都不存在 → 404。隐藏文件(`.` 开头,含上传临时文件)不列入。

### POST /files — 批量上传 / 批量下载

同一个端点按 `Content-Type` 分派:

**批量上传**(`multipart/form-data`):

| part 名 | 数量 | 必填 | 说明 |
| --- | --- | --- | --- |
| `file` | >= 1 | 是 | 文件本体;文件名取 basename;同一批共享同一个 namespace |
| `json` | 1 | 是 | JSON 对象,必须包含 `namespace` 数组 |
| 其他 | 任意 | 否 | 忽略 |

`namespace` 数组约束:每项必须是非空字符串、非 `.` / `..`,字符集限 `[A-Za-z0-9._-]`,可以为空(文件直接存根目录)。part 顺序不限。

```bash
curl -F "file=@a.txt" -F "file=@b.txt" \
     -F 'json={"namespace":["a","b"]}' \
     http://host:8765/files
# => {"ok":true,"paths":["a/b/a.txt","a/b/b.txt"]}
```

**批量下载**(`application/json`),body 为文件列表(可用上传响应的 `paths` 直接回填),响应为 zip(条目保留目录结构,解压即还原):

```bash
curl -X POST http://host:8765/files \
     -H 'Content-Type: application/json' \
     -d '{"files":["a/b/a.txt","a/b/b.txt"]}' -OJ
```

批量下载打包前整体校验存在性,任一文件缺失返回 404 并列出缺失项,不会静默缺文件。

### 响应与错误

| 状态码 | 含义 | 响应体 |
| --- | --- | --- |
| 200 | 成功 | `{"ok":true,...}` |
| 200 | 部分文件落盘失败(批量上传,尽力而为,已成功保留) | `{"ok":false,"paths":[...已成功], "errors":[{"name":"...","error":"..."}]}` |
| 400 | 协议或参数错误(缺字段、非法 namespace、路径穿越、超文件数、批内重名) | `{"ok":false,"error":"..."}` |
| 404 | 下载/列表目标不存在;批量下载存在缺失文件 | `{"ok":false,"error":"not found"}` |
| 413 | 超过大小限制 | `{"ok":false,"error":"..."}` |
| 415 | 非 POST /files 的错误 Content-Type | `{"ok":false,"error":"..."}` |

语义细节:

- 文件同名即覆盖(幂等);批量上传同批重名只保留首个,其余报 `duplicate filename in batch`
- 临时文件在一切失败路径上都会清理,失败请求不残留垃圾
- 批量下载 zip 为服务端临时文件,流式输出完毕后自动删除

### 配置(环境变量)

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `UPLOAD_ROOT` | `/data/home/nebula/upload-store` | 存储根目录 |
| `UPLOAD_PORT` | `8765` | 监听端口,仅绑定 127.0.0.1 |
| `UPLOAD_MAX_FILES` | `50` | 单次批量上传最大文件数 |
| `UPLOAD_MAX_FILE_MB` | `512` | 单文件大小上限 |
| `UPLOAD_MAX_TOTAL_MB` | `1024` | 单次批量上传总大小上限 |

## 构建与运行

需要 Rust 工具链(2021 edition):

```bash
cargo build --release
./target/release/upload-server
```

systemd user 服务示例(`~/.config/systemd/user/upload-server.service`):

```ini
[Unit]
Description=simple-upload-server

[Service]
Type=simple
ExecStart=/abs/path/to/upload-server
Restart=always
RestartSec=3

[Install]
WantedBy=default.target
```

## 设计说明

- **流式写盘**:任何写入端(PUT / 批量上传)都逐 chunk 写临时文件,校验通过后 rename 到目标,内存占用与文件大小无关
- **路径安全**:namespace 段字符集白名单,文件名剥路径成分,URL 解码后的段逐段校验(`%2F`、`%2e%2e` 等编码绕过无效),任何路径必然落在存储根内
- **大小控制**:流式计数,不依赖代理层限制,超限即刻中止并清理
- **部分成功语义**:批量上传落盘阶段失败的文件记入 `errors`,已成功的保留,不整体回滚(覆盖语义下回滚不可安全实现)
- **低内存下载**:单文件与批量下载(zip)均流式输出,整文件不驻留内存

## 不适合做什么

- 需要鉴权/多租户/配额的文件服务
- 大文件的断点续传、秒传、分片
- 需要删除、重命名、覆盖保护等文件管理能力