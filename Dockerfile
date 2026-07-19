# syntax=docker/dockerfile:1.7@sha256:a57df69d0ea827fb7266491f2813635de6f17269be881f696fbfdf2d83dda33e

FROM node:22-bookworm-slim@sha256:6c74791e557ce11fc957704f6d4fe134a7bc8d6f5ca4403205b2966bd488f6b3 AS web-builder
WORKDIR /src
COPY package.json package-lock.json tsconfig.base.json ./
COPY apps/web/package.json apps/web/package.json
COPY packages/sdk/package.json packages/sdk/package.json
RUN npm ci
COPY scripts/prepare-web-assets.mjs scripts/prepare-web-assets.mjs
COPY apps/web apps/web
COPY packages/sdk packages/sdk
RUN npm run build

FROM rust:1.88.0-bookworm@sha256:af306cfa71d987911a781c37b59d7d67d934f49684058f96cf72079c3626bfe0 AS rust-builder
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
COPY release.toml release-channel.json UNLICENSE ./
RUN cargo build --locked --release -p osb-server -p osb-cli -p osb-mcp

FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818 AS runtime
ARG OSB_VERSION=development
ARG OSB_REVISION=unknown
LABEL org.opencontainers.image.title="OpenSoverignBlog" \
      org.opencontainers.image.description="Unlicense self-hosted publishing engine" \
      org.opencontainers.image.url="https://github.com/studyreadbook4ever/OpenSoverignBlog" \
      org.opencontainers.image.source="https://github.com/studyreadbook4ever/OpenSoverignBlog" \
      org.opencontainers.image.licenses="Unlicense" \
      org.opencontainers.image.version="${OSB_VERSION}" \
      org.opencontainers.image.revision="${OSB_REVISION}"
# Bootstrap through authenticated Debian Release/Packages signatures rather
# than assuming the pinned slim base already contains a usable CA bundle.
# The immutable snapshot plus exact direct versions makes this layer repeatable.
RUN rm -f /etc/apt/sources.list /etc/apt/sources.list.d/* \
    && printf '%s\n' \
      'Types: deb' \
      'URIs: http://snapshot.debian.org/archive/debian/20260719T000000Z/' \
      'Suites: bookworm bookworm-updates' \
      'Components: main' \
      'Check-Valid-Until: no' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      '' \
      'Types: deb' \
      'URIs: http://snapshot.debian.org/archive/debian-security/20260719T000000Z/' \
      'Suites: bookworm-security' \
      'Components: main' \
      'Check-Valid-Until: no' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      > /etc/apt/sources.list.d/osb-snapshot.sources \
    && apt-get update \
    && apt-get install --yes --no-install-recommends \
      ca-certificates=20230311+deb12u1 \
      curl=7.88.1-10+deb12u15 \
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
COPY AI2AI.md UNLICENSE README.md SECURITY.md THIRD_PARTY_NOTICES.md THIRD_PARTY_LICENSES.txt release.toml release-channel.json ./
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
