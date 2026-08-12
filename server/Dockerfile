FROM node:22-alpine

RUN apk add --no-cache su-exec

WORKDIR /app
COPY package.json index.js ./
COPY docker-entrypoint.sh /usr/local/bin/
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

ARG GIT_COMMIT=unknown
ENV RECALL_GIT_COMMIT=$GIT_COMMIT
ENV RECALL_PORT=8787
ENV RECALL_DB_PATH=/data/recall.db
VOLUME /data

EXPOSE 8787
ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["node", "index.js"]
