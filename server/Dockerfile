FROM node:22-alpine

WORKDIR /app
COPY package.json index.js ./

ARG GIT_COMMIT=unknown
ENV RECALL_GIT_COMMIT=$GIT_COMMIT
ENV RECALL_PORT=8787
ENV RECALL_DB_PATH=/data/recall.db
VOLUME /data

EXPOSE 8787
CMD ["node", "index.js"]
