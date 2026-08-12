FROM node:22-alpine

WORKDIR /app
COPY package.json index.js ./

ENV RECALL_PORT=8787
ENV RECALL_DB_PATH=/data/recall.db
VOLUME /data

EXPOSE 8787
CMD ["node", "index.js"]
