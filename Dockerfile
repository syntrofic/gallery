FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
COPY migrations ./migrations
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN useradd --uid 10001 --system --home /var/lib/gallery gallery \
    && mkdir -p /var/lib/gallery && chown 10001:10001 /var/lib/gallery
COPY --from=build /src/target/release/gallery /usr/local/bin/gallery
USER 10001:10001
ENTRYPOINT ["gallery"]
CMD ["serve"]
