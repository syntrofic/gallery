# Gallery

个人自托管图库。Vue 3 前端使用 Syntrofic UI，由 Rust 同源提供。后端提供 HTTP API、SQLite、本地/S3/WebDAV 存储、任务队列、下载打包、存储迁移、备份恢复和 Codex Agent。

公开接口在 `/api/v1`。设计里的冒号动作路径会在路由前改成斜杠，例如 `images/{id}:trash` 与 `images/{id}/trash` 等价。OpenAPI 3.1 契约在 `openapi/gallery-v1.yaml`，包含请求、响应、错误、二进制下载和事件，样例在 `contracts/examples/`。

## 公开画廊

首页展示已公开的相册，`/photos` 展示同一批公开相册中的全部作品。访客无需登录。所有者可从「管理」进入后台，在「设置 → 概览」填写画廊名称和简介，再进入相册管理打开「允许访客浏览」。私有相册和未加入公开相册的原图不会进入展示页。

作品瀑布流、公开相册和大图查看器默认隐藏文件名。相册、作品和公开相册链接可在手机和桌面浏览。

## 本地运行

发布页提供 Linux Docker 镜像压缩包。导入与启动：

```sh
docker load -i gallery-v0.2.1-linux-amd64.docker.tar.gz
cp .env.example .env
# 将 .env 中的 GALLERY_IMAGE 设置为 gallery:v0.2.1
docker compose up -d
```

ARM64 主机选择对应的镜像包。默认浏览器地址为 `http://127.0.0.1:8080`。对外提供服务时，将 `GALLERY_PUBLIC_URL` 设置为最终访问地址。

### 从源码运行

前端依赖由 `frontend/pnpm-lock.yaml` 锁定，构建需要 Node 24 和 pnpm 11。

```sh
pnpm --dir frontend install --frozen-lockfile --ignore-scripts
pnpm --dir frontend build
```

需要 Rust stable。二进制不在默认 `PATH` 时先执行：

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

```sh
export GALLERY_ADMIN_PASSWORD='change-me-now'
cargo run -- serve
```

数据目录默认是 `./gallery-data`。健康检查是 `/health/live` 和 `/health/ready`。在仓库根运行时会自动使用 `frontend/dist`。二进制分发也可通过 `GALLERY_WEB_ROOT` 指定前端目录。

如需自己构建容器镜像，运行 `docker build -t gallery:local .`。复制 `.env.example` 为 `.env`，设置 `GALLERY_IMAGE` 和管理员密码后再启动：

```sh
docker compose up
```

密钥只放在本机 `secrets/`，该目录已被 git 忽略。DeepSeek 测试读取 `secrets/openrouter.key` 或 `GALLERY_OPENROUTER_KEY_FILE`，不会把 key 写进仓库。

## 测试

本地回归使用临时 SQLite、合成图库，以及本地 S3、WebDAV 和 Caddy 测试服务。

```sh
cargo test --locked -- \
  --skip codex_cli_replies_through_openrouter_deepseek \
  --skip openrouter_deepseek_answers \
  --skip deepseek_flash_captions_synthetic_image
```

在 `secrets/openrouter.key` 和 `.tools/codex` 已配置的开发环境中，运行 `cargo test --locked` 可同时验证真实 DeepSeek 的文字、图像和 Codex app-server 路径。

前后端浏览器联调使用临时图库运行。后端启用 `GALLERY_ADMIN_PASSWORD` 并监听 `127.0.0.1:48143` 后，在本机运行 `GALLERY_TEST_PASSWORD=<测试密码> pnpm --dir frontend test`。Playwright 配置覆盖桌面 Chrome 和手机 WebKit。

## 已经实现

- 会话、所有者引导、协作者、密码修改、CSRF、幂等键和问题 JSON。
- 相册、排序、回收站、权限、搜索和未归档图库。
- 上传、原图下载、桌面 ZIP、手机选图下载。
- 任务队列：租约、公平调度、暂停、时钟跳变和关闭排空。
- 本地、S3、WebDAV 存储。S3 会配置未完成分片的过期规则，并清理过期分片。
- 存储迁移：复制、切换、冒烟、回滚、降级和退役。
- 备份与恢复：快照清单、保护引用、签名、隔离恢复、恢复报告、只读回退。
- Agent：本地工具、停止、跨进程停止标记，以及经 OpenRouter 调用 DeepSeek 的可选测试。

## 备份和演练

- `GALLERY_DAILY_BACKUP=1` 时按 UTC 日期安排每日全量备份。运行中的备份和当天已成功的备份会去重。
- 新备份用 age 加密 SQLite、配置、清单及原图。Ed25519 完成标记覆盖备份 ID、格式、密文清单和封存摘要。读取时验证签名、密文摘要和明文原图摘要。已有 v1 备份可通过原有恢复接口读取。
- `GALLERY_RESTORE_DRILLS=1` 时按 UTC 自然月及成功切换存储后的恢复点安排隔离演练。同一时段或切换复用成功记录。演练检查数据库和全量原图，再用合成数据检查会话、缩略图、下载和权限。成功记录进入备份健康状态。
- Compose 默认打开每日备份和隔离恢复演练。开启演练、异地副本验证或主动恢复的环境需要解密密钥。独立保管的 `backup-age.key` 和可信 `backup-ed25519.pub` 应随恢复材料保留。签名公钥文件使用原始 32 字节 Ed25519 公钥。备份主机可以使用 `backup-age.pub` 作为收件人，并保留独立的签名私钥和本机续跑密钥。
- 将独立的备份卷挂载到另一台设备或远端文件系统后，设置 `GALLERY_BACKUP_REPLICA_MOUNT` 并执行 `docker compose -f compose.yaml -f compose.replica.yaml up`。独立盘需要允许容器 UID 10001 写入。直接运行可使用 `GALLERY_BACKUP_REPLICA_DIR`。服务会拒绝与主数据或任一本地媒体后端同文件系统的目录。副本先核验再发布，主仓库过期清理不会删除副本。
- 新格式的切换后备份需要已核验的独立副本与恢复演练，旧媒体端才可退役。

## 换机和 Caddy 切换

先在来源端执行 `POST /api/v1/admin/source/freeze`，保存返回的 `ack`，再通过备份接口生成最终备份。来源端继续提供只读访问。把已签名备份和独立保管的恢复密钥带到目标端，指定尚不存在的数据目录。

```sh
export GALLERY_DATA_DIR=/srv/gallery-new
gallery recover --repository /mnt/gallery-replica --backup-id "$GALLERY_BACKUP_ID" --keys-dir /secure/gallery-keys
gallery accept-recovery --source-ack "$GALLERY_SOURCE_ACK"
```

启动目标服务并在目标端成功生成一份新备份。先让目标端只通过管理网络可达，确认两台后端均通过 readiness。运行切换命令的主机需要有目标端数据目录的访问权限，并能访问本机 Caddy 管理端口。用 Caddy JSON 中对应 Gallery 的单上游 `reverse_proxy` 对象路径替换以下示例。

```sh
gallery handoff --source http://gallery-old:8080 --target http://gallery-new:8080 \
  --source-ack "$GALLERY_SOURCE_ACK" \
  --config-path /config/apps/http/servers/gallery/routes/0/handle/0
```

命令会直接核对源端的冻结回执和目标端的验收记录，并使用 Caddy 的 ETag 更新唯一的上游。完成后保留源端只读入口。上游变更写入 Caddy 的持久状态，Caddy 应以 `--resume` 或部署管理器保存的等价配置恢复。

## 安全

不要把 `.env`、`secrets/`、图库数据和 SQLite 文件提交到仓库。日志和接口响应不回显密码、会话令牌和对象存储密钥。
