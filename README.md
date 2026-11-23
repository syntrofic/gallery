# Gallery 后端

个人自托管图库的 Rust 后端。一个二进制提供 HTTP API、SQLite、本地/S3/WebDAV 存储、任务队列、下载打包、存储迁移、备份恢复和 Codex Agent。

公开接口在 `/api/v1`。设计里的冒号动作路径会在路由前改成斜杠，例如 `images/{id}:trash` 与 `images/{id}/trash` 等价。契约样例在 `contracts/examples/`，路径索引在 `openapi/gallery-v1.yaml`。

## 本地运行

需要 Rust stable。二进制不在默认 `PATH` 时先执行：

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

```sh
export GALLERY_ADMIN_PASSWORD='change-me-now'
cargo run -- serve
```

数据目录默认是 `./gallery-data`。健康检查是 `/health/live` 和 `/health/ready`。

Compose 使用已经构建好的镜像。复制 `.env.example` 为 `.env`，设置 `GALLERY_IMAGE` 和管理员密码后再启动：

```sh
docker compose up
```

密钥只放在本机 `secrets/`，该目录已被 git 忽略。DeepSeek 测试读取 `secrets/openrouter.key` 或 `GALLERY_OPENROUTER_KEY_FILE`，不会把 key 写进仓库。

## 测试

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo test --test local_gallery -- --test-threads=8 --skip codex_cli_replies_through_openrouter_deepseek
```

最近一次完整结果：187 通过，1 条过滤。被跳过的是调用真实 DeepSeek 的 `codex_cli_replies_through_openrouter_deepseek`。

这次提交之后又补了每日备份调度和每日对账去重，对应测试已单独通过。完整套件还没有在这两处改动之后重跑。

## 已经实现

- 会话、所有者引导、协作者、密码修改、CSRF、幂等键和问题 JSON。
- 相册、排序、回收站、权限、搜索和未归档图库。
- 上传、原图下载、桌面 ZIP、手机选图下载。
- 任务队列：租约、公平调度、暂停、时钟跳变和关闭排空。
- 本地、S3、WebDAV 存储。S3 会配置未完成分片的过期规则，并清理过期分片。
- 存储迁移：复制、切换、冒烟、回滚、降级和退役。
- 备份与恢复：快照清单、保护引用、签名、隔离恢复、恢复报告、只读回退。
- Agent：本地工具、停止、跨进程停止标记，以及经 OpenRouter 调用 DeepSeek 的可选测试。

`GALLERY_DAILY_BACKUP=1` 时，进程启动会安排当天的一份全量备份。当天已经有成功快照，或已有排队中的备份，就不会再开一份。Compose 默认打开这个开关。直接 `cargo test` 不设置该变量，测试进程不会自动开备份。

## 还没写完

这些还没有对应实现，不应当成已经上线的能力：

- 备份清单和配置仍是明文 `manifest.json`。设计中的 `manifest.json.age` 和 `config.json.age` 还没有写。SQLite 快照和原图已经加密。
- 没有每月自动恢复演练，也没有切换存储后自动演练。
- 没有把已验证备份复制到另一个故障域。
- 换机做到了源端冻结、只读回退和隔离恢复。反向代理或 DNS 切换没有代码。
- 备份复制原图、生成衍生图、视觉识别、上传复用检查，以及迁移冒烟和部分回滚核对，仍会一次读入整个对象。
- `openapi/gallery-v1.yaml` 只列了路径和响应说明，不是完整的请求响应模型。

## 安全

不要把 `.env`、`secrets/`、图库数据和 SQLite 文件提交到仓库。日志和接口响应不回显密码、会话令牌和对象存储密钥。
