FROM node:24-bookworm AS worker-build

WORKDIR /build/worker
COPY worker/package.json worker/package-lock.json ./
RUN npm ci
COPY worker/tsconfig.json ./tsconfig.json
COPY worker/src ./src
RUN npm run build && npm prune --omit=dev

FROM rust:1.93.1-trixie AS infer-build

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates python3 && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY api/Cargo.toml api/Cargo.lock ./api/
COPY api/download_models.py api/models.json ./api/
COPY api/src ./api/src
COPY src-tauri/src/onnx_integration/protection ./src-tauri/src/onnx_integration/protection
RUN cargo build --manifest-path api/Cargo.toml --release --locked --bin hope-infer
RUN python3 api/download_models.py --model-dir /models
RUN mkdir -p /stage/bin /stage/models && install -m 0755 api/target/release/hope-infer /stage/bin/hope-infer && install -m 0644 /models/*.onnx /stage/models/

FROM node:24-trixie-slim

RUN apt-get update && apt-get install -y --no-install-recommends libstdc++6 && rm -rf /var/lib/apt/lists/*
WORKDIR /app/worker
COPY --from=infer-build /stage/bin/hope-infer /app/bin/hope-infer
COPY --from=infer-build /stage/models/ /app/models/
COPY --from=worker-build /build/worker/package.json ./package.json
COPY --from=worker-build /build/worker/dist ./dist
COPY --from=worker-build /build/worker/node_modules ./node_modules
ENV HOME=/home/hope \
    HOPE_INFER_BIN=/app/bin/hope-infer \
    HOPE_MODEL_DIR=/app/models \
    HOPE_INFERENCE_LOCK=/tmp/hope-inference.lock
RUN mkdir -p /home/hope /tmp && chown 10001:10001 /home/hope && chmod 1777 /tmp && ldd /app/bin/hope-infer | tee /tmp/hope-infer-linkage && ! grep -q 'not found' /tmp/hope-infer-linkage
USER 10001:10001
STOPSIGNAL SIGTERM
ENTRYPOINT ["node", "dist/main.js"]
