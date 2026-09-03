# simple-upload-server

低内存占用的简易文件上传 + 日志存储服务,单文件 Rust 二进制,流式落盘,实测常驻内存约 3-5 MB。

用于通用场景:给远程服务器传文件、备份归档、跨机器搬运文件、收集/查询命名空间下的日志等。定位是简单、可靠、够用,**不适用于复杂业务场景**(无鉴权、无分片断点、无对象存储、无配额管理)。

## 特性

- 单二进制,无外部依赖(system sqlite3 已静态编入),内存占用恒定(文件与日志均流式/批量写入,不整文件载入内存)
- namespace 目录树:按 namespace 列表逐级建目录,天然做命名空间隔离;目录树可递归列出、按路径直接下载
- 批量上传(多文件一次请求)与批量下载(打包 zip)对称设计
- SQLite 日志库:单条/批量写入,按时间范围、关键词、namespace 及 and/or 组合查询
- 字符集校验 + 路径成分剥离,杜绝路径穿越;失败路径清理临时文件
- 进程崩溃由外部(如 systemd)托管,`Restart=always` 即可自愈

## 接口契约

文件接口的存储位置由 URL 中的 `namespace` 段决定:URL 路径即存储目录树,`{"namespace":["a","b"]}` 与 `/file/a/b/` 指向同一个目录。日志接口的 namespace 是日志条自身的一个字段(与文件 namespace 同规则、同语义空间)。

### 端点总览

| 方法 | 端点 | 语义 |
| --- | --- | --- |
| `PUT` | `/file/{ns...}/{filename}` | 单个上传,请求体即文件原始内容 |
| `GET` | `/file/{ns...}/{filename}` | 单个下载(流式) |
| `GET` | `/file/{ns...}` 或 `/file` | 递归文件列表(扁平相对路径数组) |
| `POST` | `/files` `multipart/form-data` | 批量上传 |
| `POST` | `/files` `application/json` | 批量下载(打包 zip) |
| `POST` | `/logs` | 写入日志(单条对象或数组) |
| `GET` | `/logs` | 查询日志(条件组合) |

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

### POST /logs — 写入日志

body 为单个日志对象或对象数组,每条必须含:

| 字段 | 必填 | 规则 |
| --- | --- | --- |
| `timestamp` | 是 | RFC3339 字符串(如 `2026-09-01T12:00:00Z`,支持 `+08:00` 时区偏移与小数秒)或 unix 秒整数 |
| `namespace` | 是 | 非空字符串,限 `[A-Za-z0-9._-]`,长度 <= 128 |
| `message` | 是 | 非空字符串,最多 64 KiB |
| 其他 | 否 | 任意额外字段,原样保留并返回 |

```bash
curl -X POST http://host:8765/logs -H 'Content-Type: application/json' \
     -d '{"timestamp":"2026-09-01T12:00:00Z","namespace":"app","message":"boot ok"}'
# => {"ok":true,"count":1}
```

批量(单次最多 1000 条)为**全有或全无**:任一条校验失败整批不落库,返回 400 并指出出错条目。写入走事务 + WAL。

### GET /logs — 查询日志

条件全部可选,组合时按 `op` 连接:

| 参数 | 语义 |
| --- | --- |
| `namespace` | 精确匹配 |
| `keyword` | message 子串匹配(LIKE,`%`/`_`/`\` 自动转义)` |
| `start` / `end` | 时间范围,闭区间,格式同 timestamp |
| `op` | 条件间连接符:`and`(默认)/ `or` |
| `order` | `desc`(默认,最新在前)/ `asc` |
| `limit` / `offset` | 分页,默认 100,上限 1000 |

```bash
curl 'http://host:8765/logs?namespace=app&keyword=error&start=2026-09-01T00:00:00Z&op=and'
# => {"ok":true,"total":N,"logs":[<原始日志对象>,...]}
```

`logs` 为存库时的原始 JSON 对象(含额外字段),按时间倒序返回,同刻按写入序稳定。

### 响应与错误

| 状态码 | 含义 | 响应体 |
| --- | --- | --- |
| 200 | 成功 | `{"ok":true,...}` |
| 200 | 部分文件落盘失败(批量上传,尽力而为,已成功保留) | `{"ok":false,"paths":[...已成功], "errors":[{"name":"...","error":"..."}]}` |
| 400 | 协议或参数错误(缺字段、非法 namespace、路径穿越、超文件数、批内重名、日志条校验失败、非法 JSON body) | `{"ok":false,"error":"..."}` |
| 404 | 下载/列表目标不存在;批量下载存在缺失文件 | `{"ok":false,"error":"not found"}` |
| 413 | 超过大小限制 | `{"ok":false,"error":"..."}` |
| 415 | Content-Type 不符(/files 非 multipart 或非 application/json、/logs 非 application/json) | `{"ok":false,"error":"..."}` |
| 500 | 存储层内部错误(日志写入/查询失败) | `{"ok":false,"error":"..."}` |

语义细节:

- 文件同名即覆盖(幂等);批量上传同批重名只保留首个,其余报 `duplicate filename in batch`
- 临时文件在一切失败路径上都会清理,失败请求不残留垃圾
- 批量下载 zip 为服务端临时文件,流式输出完毕后自动删除

### 配置(环境变量)

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `UPLOAD_ROOT` | `~/upload-store` | 存储根目录,默认在用户主目录下,实际以 `UPLOAD_ROOT` 为准 |
| `UPLOAD_PORT` | `8765` | 监听端口,仅绑定 127.0.0.1 |
| `UPLOAD_MAX_FILES` | `50` | 单次批量上传最大文件数 |
| `UPLOAD_MAX_FILE_MB` | `512` | 单文件大小上限 |
| `UPLOAD_MAX_TOTAL_MB` | `1024` | 单次批量上传总大小上限 |
| `UPLOAD_LOG_DB` | `~/upload-logs.db` | SQLite 日志库文件路径,默认在用户主目录下 |

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
- **日志存储**:SQLite(WAL)存原始 JSON 整条 + `ts`/`namespace`/`message` 索引列,查询即走索引,批量写入走事务(全有或全无);单连接互斥,日志库独立于文件存储根,不进入文件列表

## 不适合做什么

- 需要鉴权/多租户/配额的文件服务
- 大文件的断点续传、秒传、分片
- 需要删除、重命名、覆盖保护等文件管理能力