# simple-upload-server

低内存占用的简易文件上传服务器,单文件 Rust 二进制,流式落盘,实测常驻内存约 2 MB。

用于通用上传场景:给远程服务器传文件、备份归档、跨机器搬运文件等。定位是简单、可靠、够用,**不适用于复杂业务场景**(无鉴权、无分片断点、无对象存储、无配额管理)。

## 特性

- 单二进制,无外部依赖,内存占用恒定(文件内容流式写盘,不整文件载入内存)
- 一次请求上传一个或多个文件,按 `namespace` 列表递归创建目录,天然做命名空间隔离
- 字符集校验 + 路径成分剥离,杜绝路径穿越
- 进程崩溃由外部(如 systemd)托管,`Restart=always` 即可自愈

## 接口契约

### `POST /upload`

`Content-Type: multipart/form-data`。part 结构与约束:

| part 名 | 数量 | 必填 | 说明 |
| --- | --- | --- | --- |
| `file` | >= 1 | 是 | 文件本体;文件名取 basename(客户端带路径会被剥离);同一批文件共享同一个 namespace |
| `json` | 1 | 是 | JSON 对象,必须包含 `namespace` 数组 |
| 其他 | 任意 | 否 | 忽略 |

`namespace` 数组约束:每项必须是非空字符串、非 `.` / `..`,字符集限 `[A-Za-z0-9._-]`。服务端按数组顺序递归创建目录,文件存放在最后一层,即 `{"namespace":["a","b","c"]}` 对应 `根目录/a/b/c/`。`namespace` 数组可以为空(文件直接存根目录)。

part 顺序不限,`file` 与 `json` 可任意混排。

```bash
# 多文件
curl -F "file=@a.txt" -F "file=@b.txt" \
     -F 'json={"namespace":["a","b"]}' \
     http://127.0.0.1:8765/upload
# => {"ok":true,"paths":["a/b/a.txt","a/b/b.txt"]}

# 单文件
curl -F "file=@a.txt" -F 'json={"namespace":["a","b","c"]}' \
     http://127.0.0.1:8765/upload
# => {"ok":true,"paths":["a/b/c/a.txt"]}
```

#### 响应

| 状态码 | 含义 | 响应体 |
| --- | --- | --- |
| 200 | 全部成功 | `{"ok":true,"paths":["a/b/a.txt",...]}` |
| 200 | 部分文件落盘失败(尽力而为,已成功文件保留) | `{"ok":false,"paths":[已成功], "errors":[{"name":"...","error":"..."}]}` |
| 400 | 协议或参数错误 | `{"ok":false,"error":"..."}` |
| 413 | 超过大小限制 | `{"ok":false,"error":"..."}` |
| 405 | 非 POST 方法 | - |

常见 400 错误:`missing required json part with "namespace" field`、`missing file part`、`invalid namespace item: ".."`、`too many files (max N)`、`duplicate filename in batch`(同批重名只保留首个)、`invalid json: ...`、`invalid filename`。

语义细节:

- 文件同名即覆盖(幂等,平台默认行为)
- 同批重名文件:只保留第一个,其余报 `duplicate filename in batch`
- 临时文件在一切失败路径上都会清理,失败请求不残留垃圾
- 单次请求上限(见下),超限整体拒绝并返回 413

### 配置(环境变量)

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `UPLOAD_ROOT` | `/data/home/nebula/upload-store` | 存储根目录 |
| `UPLOAD_PORT` | `8765` | 监听端口,仅绑定 127.0.0.1 |
| `UPLOAD_MAX_FILES` | `50` | 单次请求最大文件数 |
| `UPLOAD_MAX_FILE_MB` | `512` | 单文件大小上限 |
| `UPLOAD_MAX_TOTAL_MB` | `1024` | 单次请求总大小上限 |

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

- **流式写盘**:文件 part 逐 chunk 写入临时文件,全部解析并校验通过后 rename 到目标路径,内存占用与文件大小无关
- **路径安全**:namespace 项与文件名均做字符集白名单校验并剥离路径成分,最终路径必然落在存储根目录内
- **大小控制**:流式计数,不依赖代理层限制,超限即刻中止并清理
- **部分成功语义**:落盘阶段失败的文件记入 `errors`,已成功的保留,不整体回滚(覆盖语义下回滚不可安全实现)

## 不适合做什么

- 需要鉴权/多租户/配额的文件服务
- 大文件的断点续传、秒传、分片
- 需要浏览、删除、重命名等文件管理能力