FROM rust:1.99.1-bookworm AS terminal
WORKDIR /build/tui
COPY tui/rust-toolchain.toml ./
RUN rustup show active-toolchain && rustup target add wasm32-unknown-unknown && cargo install wasm-bindgen-cli --version 0.2.129 --locked
COPY tui/browser/Cargo.toml tui/browser/Cargo.lock browser/
COPY tui/browser/src browser/src
COPY tui/src/app.rs tui/src/editor.rs tui/src/nav.rs tui/src/screens.rs tui/src/view.rs src/
COPY web/build-terminal.sh /build/web/
RUN --mount=type=cache,target=/usr/local/cargo/registry sh /build/web/build-terminal.sh

FROM node:26-bookworm-slim AS browser
WORKDIR /build/web
COPY web/package*.json ./
RUN --mount=type=cache,target=/root/.npm npm ci
COPY web/index.html web/vite.config.ts web/tsconfig.json ./
COPY web/public public
COPY web/src src
COPY --from=terminal /build/web/src/generated/terminal src/generated/terminal
RUN npm run build:bundle

FROM rust:1.99.1-bookworm AS service
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml LICENSE ./
ARG CARGO_BUILD_JOBS=2
COPY server server
COPY harness harness
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/usr/local/cargo/git --mount=type=cache,target=/build/target,sharing=locked \
    find server harness -type f -exec touch '{}' + && cargo build --workspace --locked --release -j "$CARGO_BUILD_JOBS" && cp target/release/venticode /venticode && cp target/release/venticode-harness /venticode-harness

FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=service /venticode /venticode
COPY --from=service /venticode-harness /venticode-harness
COPY --from=service /build/LICENSE /LICENSE
COPY --from=browser /build/web/dist /web
ENV WEB_ROOT=/web LISTEN=0.0.0.0:8080
ENTRYPOINT ["/venticode"]
