FROM node:26-bookworm AS build
ARG TARGETARCH
ARG KITESURF_REV=e27607c93e5e09eb1e7f982292ed34639a462755
ARG BAZELISK_VERSION=v1.30.0
ARG BUILD_JOBS=2
ENV DO_NOT_TRACK=1 WRANGLER_SEND_METRICS=false
RUN curl -fsSL "https://github.com/bazelbuild/bazelisk/releases/download/${BAZELISK_VERSION}/bazelisk-linux-${TARGETARCH}" -o /usr/local/bin/bazel \
    && chmod 0755 /usr/local/bin/bazel
WORKDIR /src
RUN git init && git remote add origin https://github.com/cloudflare/kitesurf.git \
    && git fetch --depth 1 origin "$KITESURF_REV" && git checkout --detach FETCH_HEAD \
    && test "$(git rev-parse HEAD)" = "$KITESURF_REV"
RUN bazel --batch build --jobs="$BUILD_JOBS" //packages/engine:dist \
    && mkdir -p /out/engine /out/assets \
    && cp -aL bazel-bin/packages/engine/dist/. /out/engine/ \
    && cp -aL bazel-bin/packages/engine/public/. /out/assets/ \
    && cp LICENSE /out/LICENSE.kitesurf \
    && rm -f /out/engine/index.js.map \
    && test -s /out/engine/engine-mf.json && test -d /out/assets/fonts

FROM node:26-bookworm-slim AS runtime
ENV NODE_ENV=production HOME=/tmp TMPDIR=/tmp \
    MINIFLARE_WORKERD_V8_FLAGS=--expose-gc WRANGLER_SEND_METRICS=false
WORKDIR /app
COPY deploy/kitesurf/package.json deploy/kitesurf/package-lock.json ./
RUN npm ci --omit=dev --no-audit --no-fund
COPY deploy/kitesurf/serve.mjs ./
COPY --from=build /out/ /app/
USER 10001:10001
EXPOSE 8787
ENTRYPOINT ["node", "/app/serve.mjs"]
