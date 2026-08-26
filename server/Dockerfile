FROM node:22-alpine

RUN apk add --no-cache su-exec

# The `claude` CLI itself — Phase 2's merge shells out to it rather than
# calling the Anthropic API directly (see server/index.js and CLAUDE.md's
# no-API-key rule). Installing it is necessary but not sufficient: it also
# needs a one-time interactive `claude setup-token` run against the running
# container before merge actually works — see deploy/README.md. Until
# that's done (or if this RUN step or the login ever breaks), the server
# still starts and syncs fine — merge just degrades to last-write-wins,
# visible at GET /health.
RUN npm install -g @anthropic-ai/claude-code

WORKDIR /app
COPY package.json index.js ./
COPY docker-entrypoint.sh /usr/local/bin/
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

ARG GIT_COMMIT=unknown
ENV RECALL_GIT_COMMIT=$GIT_COMMIT
ENV RECALL_PORT=8787
ENV RECALL_DB_PATH=/data/recall.db
# The CLI's own login/config lives under the existing /data volume rather
# than a separate one, so `claude setup-token` only needs running once —
# it survives container rebuilds along with everything else in /data.
ENV CLAUDE_CONFIG_DIR=/data/claude-config
VOLUME /data

EXPOSE 8787
ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["node", "index.js"]
