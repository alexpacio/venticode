FROM node:26-bookworm-slim AS browser
WORKDIR /build/web
COPY web/package*.json ./
RUN --mount=type=cache,target=/root/.npm npm ci
COPY web/index.html web/vite.config.ts web/tsconfig.json ./
COPY web/src src
RUN npm run build

FROM rust:1.99.1-bookworm AS service
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
ARG CARGO_BUILD_JOBS=2
COPY server server
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/usr/local/cargo/git --mount=type=cache,target=/build/target,sharing=locked \
    find server -type f -exec touch '{}' + && cargo build --locked --release -j "$CARGO_BUILD_JOBS" && cp target/release/venticode /venticode

FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=service /venticode /venticode
COPY --from=browser /build/web/dist /web
ENV WEB_ROOT=/web LISTEN=0.0.0.0:8080
ENTRYPOINT ["/venticode"]
