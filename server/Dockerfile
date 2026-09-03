# Build the binary...
FROM golang:1.24-alpine AS build

WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY cmd ./cmd
COPY internal ./internal

ARG GIT_COMMIT=unknown
# CGO off: the SQLite driver is pure Go, so this needs no C toolchain and
# the result runs on a bare image.
RUN CGO_ENABLED=0 go build -trimpath \
    -ldflags "-s -w -X main.version=docker -X main.commit=${GIT_COMMIT}" \
    -o /out/recall ./cmd/recall

# ...then assemble a runtime that still has Node in it.
#
# Worth being explicit about: moving the server to Go did NOT make this
# image small. The semantic merge shells out to the `claude` CLI, which is
# a Node package, so the runtime needs Node regardless of what language the
# server itself is written in. What Go bought here is the build, the tests,
# and one implementation shared with the client — not image size.
FROM node:22-alpine

RUN apk add --no-cache su-exec

# The `claude` CLI, used only for the semantic merge. Installing it is
# necessary but not sufficient: it also needs a one-time interactive
# `claude setup-token` run against the running container — see
# deploy/README.md. Until then the server still runs and syncs; merge just
# degrades to last-write-wins, visible at GET /health.
RUN npm install -g @anthropic-ai/claude-code

COPY --from=build /out/recall /usr/local/bin/recall
COPY server/docker-entrypoint.sh /usr/local/bin/
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
