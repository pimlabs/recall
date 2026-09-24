# The Recall server image, and (target `worker`) the merge worker's.
#
# Each binary comes from one of two places, picked by RECALL_SOURCE:
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
#
# The server is the last stage, so a build that names no target builds it,
# as every build did before the worker existed. The worker is
# `--target worker`: the same Node and `claude` CLI, and recall-worker in
# place of recall-server. Only the worker's image needs a release that
# publishes recall-worker.
ARG RECALL_SOURCE=release

# The `claude` CLI both images carry, pinned, so a rebuild installs the
# version the merge's flags were checked against rather than whatever npm
# resolves that day. The merge passes `--tools ""` and `--max-turns 1`
# (recall-worker's merge.rs), both checked against this version; move the
# pin only after checking them again against the new one's `claude --help`
# (`--max-turns` is a hidden option there, so search the CLI for it).
ARG CLAUDE_CODE_VERSION=2.1.280

# cargo-chef, pinned for the same reason as CLAUDE_CODE_VERSION above: a
# rebuild has to install the tool version the recipe/cook flags below were
# actually checked against, not whatever crates.io serves that day, so move
# this pin deliberately (check https://crates.io/api/v1/crates/cargo-chef
# for the current version, and re-check `cargo chef cook --help` for the
# new one before relying on any flag used below).
ARG CARGO_CHEF_VERSION=0.1.78

# ---- chef: rust-alpine plus cargo-chef, the base both `planner` and
# `source` build from. Its own layers (the apk packages, the cargo install)
# only change when this Dockerfile does, so they are cache hits on nearly
# every CI run regardless of what a PR touches.
FROM rust:1-alpine AS chef

# rusqlite's `bundled` feature compiles the SQLite amalgamation from C, so
# the build stage needs a C toolchain even though nothing here is C. musl
# specifically, so the result runs on the Alpine-based runtime below.
#
# perl, make and linux-headers are for OpenSSL, which the passkey sign-in on
# /admin needs (webauthn-rs verifies with it). It is vendored: built from
# source here and linked in statically, so the runtime image needs no
# OpenSSL of its own. Its Configure script is perl, its build is make, and
# linux-headers is not optional: crypto/mem_sec.c includes <linux/mman.h>
# whenever <sys/syscall.h> defines SYS_mlock2, which musl's does.
RUN apk add --no-cache musl-dev perl make linux-headers

WORKDIR /src
ARG CARGO_CHEF_VERSION
RUN cargo install cargo-chef --locked --version "${CARGO_CHEF_VERSION}"

# ---- planner: cargo-chef looks at the manifests alone (Cargo.toml,
# Cargo.lock, and each crate directory's shape) and writes recipe.json, the
# minimum needed to compile just the dependencies. Editing a .rs file
# without touching a manifest or adding/removing a crate does not change
# recipe.json, which is what lets `source` below cache the dependency
# compile across an ordinary source change.
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo chef prepare --recipe-path recipe.json

# ---- source: build the binary from this checkout.
FROM chef AS source
COPY --from=planner /src/recipe.json recipe.json
# Cooks recall-server's dependency tree, then recall-worker's, into one
# layer keyed only on recipe.json, so source-worker below reuses it instead
# of compiling its own copy of the deps they share. This has to mirror the
# real `cargo build` calls below flag for flag and package for package:
# cargo fingerprints a dependency by its exact feature set, and a `cook`
# that asks for a different one leaves the cache with nothing the real
# build can reuse. Two `cook` calls rather than one `-p recall-server -p
# recall-worker`, because recall-worker's default `client` feature is off
# when recall-server merely depends on it (its own Cargo.toml turns that
# off) but on when recall-worker is cooked as its own package; asking for
# both in a single cargo invocation would unify the two into one feature
# set, which is wrong for whichever side wanted it off. `--release
# --locked` match the real builds below; checked against cargo-chef
# v0.1.78's own `cook` flags (`-p`/`--package` takes a pkgid and can repeat).
RUN cargo chef cook --release --locked -p recall-server --recipe-path recipe.json \
 && cargo chef cook --release --locked -p recall-worker --recipe-path recipe.json

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# Not a release, and the discovery document says so. The context has no
# .git, which on its own would read as a crates.io release build, so this is
# set rather than inferred.
ENV RECALL_BUILD_CHANNEL=dev
RUN cargo build --release --locked -p recall-server \
 && mkdir -p /out && cp target/release/recall-server /out/recall-server

# ---- source, for the worker: the same build directory, one more binary.
# A stage of its own so the server's image does not wait for it: this only
# compiles recall-worker itself, since its dependency tree was already
# cooked into the layer above, alongside recall-server's.
FROM source AS source-worker
RUN cargo build --release --locked -p recall-worker \
 && cp target/release/recall-worker /out/recall-worker

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

# ---- release, for the worker: its own archive from the same release.
FROM alpine:3 AS release-worker
RUN apk add --no-cache curl
ARG RECALL_VERSION=
ARG RECALL_RELEASES_URL=https://github.com/pimlabs/recall/releases/download
ARG TARGETARCH
WORKDIR /fetch
COPY Cargo.toml ./
COPY deploy/fetch-release.sh ./
RUN set -eu; \
    version="${RECALL_VERSION:-$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)}"; \
    arch="${TARGETARCH:-$(uname -m | sed 's/x86_64/amd64/; s/aarch64/arm64/')}"; \
    sh fetch-release.sh "$version" "$arch" /out/recall-worker "$RECALL_RELEASES_URL" recall-worker

# ---- whichever of the two was asked for. BuildKit builds only the stages
# the target it was asked for needs.
FROM ${RECALL_SOURCE} AS binary
FROM ${RECALL_SOURCE}-worker AS worker-binary

# ---- the worker: an enrolled device that merges queued conflicts with the
# `claude` CLI. It opens no port, so there is no EXPOSE, and the compose
# files give it none. Its key and the CLI's login live on its own /data
# volume, never the server's.
FROM node:22-alpine AS worker

RUN apk add --no-cache su-exec

# The CLI the merge shells out to. Like the server's, it needs a one-time
# `claude setup-token`, run against this container; see deploy/README.md.
# Until then the worker takes no jobs, and they wait in the queue.
ARG CLAUDE_CODE_VERSION
RUN npm install -g "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}"

COPY --from=worker-binary /out/recall-worker /usr/local/bin/recall-worker
COPY deploy/docker-entrypoint.sh /usr/local/bin/
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

ARG GIT_COMMIT=
ARG RECALL_VERSION=
LABEL org.opencontainers.image.title="recall-worker" \
      org.opencontainers.image.description="Recall's merge worker" \
      org.opencontainers.image.source="https://github.com/pimlabs/recall" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="$RECALL_VERSION" \
      org.opencontainers.image.revision="$GIT_COMMIT"
ENV RECALL_WORKER_DIR=/data
ENV CLAUDE_CONFIG_DIR=/data/claude-config
VOLUME /data

ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["recall-worker"]

# ---- the server's runtime, which still has Node in it.
#
# Worth being explicit about: rewriting the server did NOT make this image
# small. The semantic merge shells out to the `claude` CLI, which is a Node
# package, so the runtime needs Node regardless of what language the server
# is written in. A deployment with a worker merges there instead; until
# every one has, the server keeps the CLI for the inline merge.
FROM node:22-alpine AS server

RUN apk add --no-cache su-exec

# The `claude` CLI, used only for the semantic merge. Installing it is
# necessary but not sufficient: it also needs a one-time interactive
# `claude setup-token` run against the running container — see
# deploy/README.md. Until then the server still runs and syncs; merge just
# degrades to last-write-wins, visible at GET /health.
ARG CLAUDE_CODE_VERSION
RUN npm install -g "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}"

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
