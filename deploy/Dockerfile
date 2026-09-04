# Build the binary...
FROM rust:1-alpine AS build

# rusqlite's `bundled` feature compiles the SQLite amalgamation from C, so
# the build stage needs a C toolchain even though nothing here is C. musl
# specifically, so the result runs on the Alpine-based runtime below.
RUN apk add --no-cache musl-dev

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p recall-sync

# ...then assemble a runtime that still has Node in it.
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

COPY --from=build /src/target/release/recall /usr/local/bin/recall
COPY deploy/docker-entrypoint.sh /usr/local/bin/
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

ARG GIT_COMMIT=unknown
ENV RECALL_GIT_COMMIT=$GIT_COMMIT
ENV RECALL_PORT=8787
ENV RECALL_DB_PATH=/data/recall.db
# The CLI's own login lives under the existing /data volume rather than a
# separate one, so `claude setup-token` survives rebuilds.
ENV CLAUDE_CONFIG_DIR=/data/claude-config
VOLUME /data

EXPOSE 8787
ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["recall", "serve"]
