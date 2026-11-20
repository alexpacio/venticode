.PHONY: help setup generate dev mock web build check test

MOCK_PORT ?= 4189
WEB_PORT ?= 4175

help:
	@printf '%s\n' \
	  'make setup      install web dependencies' \
	  'make generate   regenerate web/src/api/schema.ts from api/openapi.yaml' \
	  'make dev        run the synthetic mock API and the web app with HMR' \
	  'make build      production build of the web app into web/dist' \
	  'make test       mock, palette and font tests' \
	  'make check      generate, type check, tests and build' \
	  'make demo-tui-web  serve the WebAssembly terminal and browser demos on loopback' \
	  'make check-tui  format, lint and test the terminal client and its WebAssembly build' \
	  'make build-tui-windows  build the Windows x64 binary with the MSVC toolchain'

setup:
	npm --prefix web ci

generate:
	npm --prefix web run generate

mock:
	VENTICODE_MOCK_PORT=$(MOCK_PORT) VENTICODE_ORIGIN=http://127.0.0.1:$(WEB_PORT) node --watch dev/mock.ts

web:
	VENTICODE_API=http://127.0.0.1:$(MOCK_PORT) npm --prefix web run dev -- --port $(WEB_PORT)

dev:
	@trap 'kill 0' INT TERM EXIT; \
	VENTICODE_MOCK_PORT=$(MOCK_PORT) VENTICODE_ORIGIN=http://127.0.0.1:$(WEB_PORT) node --watch dev/mock.ts & \
	VENTICODE_API=http://127.0.0.1:$(MOCK_PORT) npm --prefix web run dev -- --port $(WEB_PORT)

build:
	npm --prefix web run build

test:
	node --test dev/mock.test.ts web/src/palette.test.ts web/src/font.test.ts

check:
	@before=$$(sha256sum web/src/api/schema.ts); $(MAKE) --no-print-directory generate >/dev/null; \
	[ "$$before" = "$$(sha256sum web/src/api/schema.ts)" ] || { echo 'api/openapi.yaml changed: review the regenerated web/src/api/schema.ts'; exit 1; }
	npm --prefix web run typecheck
	$(MAKE) --no-print-directory test
	npm --prefix web run build

.PHONY: check-service check-connector
check-service:
	cargo fmt --all --check
	cargo clippy --workspace --release --locked --all-targets -- -D warnings
	cargo test --workspace --release --locked
	cargo build --workspace --release --locked

check-connector:
	mvn -B -f connector/pom.xml package dependency:copy-dependencies
	python3 connector/tests/transport.py

.PHONY: check-tui build-tui-windows
check-tui:
	cd tui && cargo fmt --check
	cd tui && cargo clippy --release --locked --all-targets -- -D warnings
	cd tui && cargo test --release --locked
	cd tui/browser && cargo fmt --check
	cd tui/browser && cargo clippy --target wasm32-unknown-unknown --release --locked -- -D warnings

build-tui-windows:
	cd tui && cargo build --release --locked --target x86_64-pc-windows-msvc

.PHONY: build-tui-web demo-tui-web
build-tui-web: build

demo-tui-web: build-tui-web
	node dev/tui-demo.ts
