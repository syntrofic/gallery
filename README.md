# Gallery

Gallery 是一个可自托管的照片画廊，适合发布个人作品和分享相册。[查看示例站](https://gallery.syntrofic.com/)

## 技术栈

前端使用 Vue 3、TypeScript、Vite 和 Syntrofic UI。后端使用 Rust、Axum 和 SQLite，同源提供网页、API 和图片。支持用 Docker Compose 部署。

## 功能

- 公开相册和照片瀑布流，按屏幕选择缩略图或预览。全屏查看支持跟手滑动、键盘翻页和缩放，访客可以下载预览图。明暗主题跟随系统，暗色模式下自动调暗照片。
- 管理相册、上传照片、分配协作者权限，以及下载原图或整册 ZIP。
- 支持本地磁盘、S3 兼容存储和 WebDAV，提供存储迁移、备份与恢复。

## 自托管

在 Linux 主机上，用仓库里的 `compose.yaml` 启动服务。下载对应架构的 Docker 镜像和 SHA-256 校验文件。 [GitHub Releases](https://github.com/syntrofic/gallery/releases/latest)

```sh
docker load -i gallery-v0.2.6-linux-amd64.docker.tar.gz
cp .env.example .env
# 在 .env 中设置 GALLERY_IMAGE=gallery:v0.2.6 和 GALLERY_ADMIN_PASSWORD
mkdir -p gallery-data
sudo chown 10001:10001 gallery-data
docker compose up -d
```

默认入口为 `http://127.0.0.1:8080`，管理入口为 `/admin`。对外部署时，将 `GALLERY_PUBLIC_URL` 设为实际地址。请持久化 `gallery-data` 目录。

API 契约见 [OpenAPI](openapi/gallery-v1.yaml)。
