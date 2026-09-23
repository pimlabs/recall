# The Recall server image.
#
# Its binary comes from one of two places, picked by RECALL_SOURCE:
#
#   release (the default)  the recall-server a GitHub Release published,
#                          checked against that release's checksums. This is
#                          what a deployment runs: the exact bytes the
#                          release built, not a rebuild that merely shares a
#                          commit with it.
#   source                 built here from the checkout. For CI, which has
#                          no release for an unmerged change, and for trying
#                          a branch by hand.
#
# RECALL_VERSION names the release. Left empty it is read from the checkout's
# Cargo.toml, so `docker compose up -d --build` in a checkout of tag v0.4.0
# deploys 0.4.0.
ARG RECALL_SOURCE=release

# ---- source: build the binary from this checkout.
FROM rust:1-alpine AS source

# rusqlite's `bundled` feature compiles the SQLite amalgamation from C, so
# the build stage needs a C toolchain even though nothing here is C. musl
# specifically, so the result runs on the Alpine-based runtime below.
#
# perl, make and linux-headers are for OpenSSL, which the passkey sign-in on
# /admin needs (webauthn-rs verifies with it). It is vendored: built from
# source here and linked in statically, so the runtime image needs no
# OpenSSL of its own. Its Configure script is perl, its build is make, and
# it includes a few kernel headers.
RUN apk add --no-cache musl-dev perl make linux-headers

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# Not a release, and the discovery document says so. The context has no
# .git, which on its own would read as a crates.io release build, so this is
# set rather than inferred.
ENV RECALL_BUILD_CHANNEL=dev
RUN cargo build --release --locked -p recall-server \
 && mkdir -p /out && cp target/release/recall-server /out/recall-server

# ---- release: fetch the published binary.
FROM alpine:3 AS release
RUN apk add --no-cache curl
ARG RECALL_VERSION=
# Where the v<version>/ release directories are. Only CI changes it, to a
# local copy of a release it assembled itself.
ARG RECALL_RELEASES_URL=https://github.com/pimlabs/recall/releases/download
# Set by BuildKit to the platform being built for (amd64, arm64).
ARG TARGETARCH
WORKDIR /fetch
COPY Cargo.toml ./
COPY deploy/fetch-release.sh ./
RUN set -eu; \
    version="${RECALL_VERSION:-$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)}"; \
    arch="${TARGETARCH:-$(uname -m | sed 's/x86_64/amd64/; s/aarch64/arm64/')}"; \
    sh fetch-release.sh "$version" "$arch" /out/recall-server "$RECALL_RELEASES_URL"

# ---- whichever of the two was asked for. BuildKit builds only this one.
FROM ${RECALL_SOURCE} AS binary

# ---- the runtime, which still has Node in it.
#
# Worth being explicit about: rewriting the server did NOT make this image
# small. The semantic merge shells out to the `claude` CLI, which is a Node
# package, so the runtime needs Node regardless of what language the server
# is written in.
FROM node:22-alpine

RUN apk add --no-cache su-exec

# The `claude` CLI, used only for the semantic merge. Installing it is
# necessary but not sufficient: it also needs a one-time interactive
# `claude setup-token` run against the running container — see
# deploy/README.md. Until then the server still runs and syncs; merge just
# degrades to last-write-wins, visible at GET /health.
RUN npm install -g @anthropic-ai/claude-code

COPY --from=binary /out/recall-server /usr/local/bin/recall-server
COPY deploy/docker-entrypoint.sh /usr/local/bin/
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# Empty unless the deploy passes it. A release binary already knows the
# commit it was built from, and an empty variable leaves that in place;
# a source build has no stamp, so the deploy names the checkout's commit.
ARG GIT_COMMIT=
ENV RECALL_GIT_COMMIT=$GIT_COMMIT
ARG RECALL_VERSION=
LABEL org.opencontainers.image.title="recall-server" \
      org.opencontainers.image.description="Recall's sync server" \
      org.opencontainers.image.source="https://github.com/pimlabs/recall" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="$RECALL_VERSION" \
      org.opencontainers.image.revision="$GIT_COMMIT"
ENV RECALL_PORT=8787
ENV RECALL_DB_PATH=/data/recall.db
# The CLI's own login lives under the existing /data volume rather than a
# separate one, so `claude setup-token` survives rebuilds.
ENV CLAUDE_CONFIG_DIR=/data/claude-config
VOLUME /data

EXPOSE 8787
ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["recall-server"]
