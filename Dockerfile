# syntax=docker/dockerfile:1.27@sha256:bde3983e9c939224420ddaf6b784cc30e09b035a4dea01f581230c50809f372e

# renovate: datasource=github-tags depName=rust-lang/rust
ARG RUST_VERSION=1.96.0

FROM rust:${RUST_VERSION}-slim-bookworm@sha256:4732ca96fd086cb9be682050c3f0176288eebaac2b80aa2bcefccfaf198e1950 AS builder
WORKDIR /app

# Optional source mirror. Omit this argument to build from crates.io.
ARG CRATES_INDEX_URL

RUN apt-get update \
    && apt-get install --yes --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*
COPY . .

# Preserve the public crates.io identity recorded in Cargo.lock.
RUN if [ -n "${CRATES_INDEX_URL}" ]; then \
      mkdir -p .cargo && \
      printf '\n[source.crates-io]\nreplace-with = "mirror"\n\n[source.mirror]\nregistry = "%s"\n' \
        "${CRATES_INDEX_URL}" >> .cargo/config.toml; \
    fi

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked --bin mcp-gitea-rs \
    && cp target/release/mcp-gitea-rs /usr/local/bin/mcp-gitea-rs

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
LABEL org.opencontainers.image.source="https://github.com/chrisbennight/mcp-gitea-rs"
LABEL org.opencontainers.image.licenses="MIT"
COPY --from=builder /usr/local/bin/mcp-gitea-rs /mcp-gitea-rs
EXPOSE 8000
HEALTHCHECK --interval=30s --timeout=3s --retries=3 CMD ["/mcp-gitea-rs", "--healthcheck"]
ENTRYPOINT ["/mcp-gitea-rs"]
