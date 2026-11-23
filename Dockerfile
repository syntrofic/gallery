FROM node:24-bookworm-slim AS web
WORKDIR /src/frontend
RUN npm install --global pnpm@11.25.0
COPY frontend/package.json frontend/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile --ignore-scripts
COPY frontend/ ./
RUN pnpm build

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
COPY migrations ./migrations
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --system --home /var/lib/gallery gallery \
    && mkdir -p /var/lib/gallery && chown 10001:10001 /var/lib/gallery
COPY --from=build /src/target/release/gallery /usr/local/bin/gallery
COPY --from=web /src/frontend/dist/ /opt/gallery/web/
ENV GALLERY_WEB_ROOT=/opt/gallery/web
USER 10001:10001
ENTRYPOINT ["gallery"]
CMD ["serve"]
