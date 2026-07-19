# syntax=docker/dockerfile:1.7

FROM node:22-bookworm-slim AS web-builder
WORKDIR /src
COPY package.json package-lock.json tsconfig.base.json ./
COPY apps/web/package.json apps/web/package.json
COPY packages/sdk/package.json packages/sdk/package.json
RUN npm ci
COPY scripts/prepare-web-assets.mjs scripts/prepare-web-assets.mjs
COPY apps/web apps/web
COPY packages/sdk packages/sdk
RUN npm run build

FROM rust:1.88.0-bookworm AS rust-builder
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY apps/cli apps/cli
COPY apps/mcp apps/mcp
COPY apps/server apps/server
COPY crates crates
COPY features features
COPY plugins plugins
COPY openapi openapi
COPY deploy/custom.css deploy/custom.css
RUN cargo build --locked --release -p osb-server -p osb-cli -p osb-mcp

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 65532 osb \
    && useradd --system --uid 65532 --gid osb --home-dir /app osb
WORKDIR /app
COPY --from=rust-builder /src/target/release/osb-server /usr/local/bin/osb-server
COPY --from=rust-builder /src/target/release/osb /usr/local/bin/osb
COPY --from=rust-builder /src/target/release/osb-mcp /usr/local/bin/osb-mcp
COPY --from=web-builder /src/apps/web/dist apps/web/dist
# The project license and generated application-dependency notices travel with
# the binaries and web bundle. Base-image package notices remain under
# /usr/share/doc; see THIRD_PARTY_NOTICES.md for the SBOM boundary.
COPY AI2AI.md LICENSE README.md SECURITY.md THIRD_PARTY_NOTICES.md THIRD_PARTY_LICENSES.txt ./
COPY docs docs
COPY providers providers
COPY schemas schemas
RUN ln -s /usr/local/bin/osb /usr/local/bin/osb-cli \
    && mkdir -p /data /backups \
    && chown -R osb:osb /data /backups /app
USER 65532:65532
ENV OSB_BIND=0.0.0.0:8787 \
    OSB_DATABASE=/data/open-soverign-blog.db \
    OSB_BLOB_DIRECTORY=/data/blobs \
    OSB_WEB_DIST=/app/apps/web/dist \
    OSB_FEATURES=seo
EXPOSE 8787
VOLUME ["/data", "/backups"]
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
  CMD ["curl", "--fail", "--silent", "http://127.0.0.1:8787/readyz"]
ENTRYPOINT ["/usr/local/bin/osb-server"]
