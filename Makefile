.PHONY: help setup generate dev mock web build check test

MOCK_PORT ?= 4189
WEB_PORT ?= 4175

help:
	@printf '%s\n' \
	  'make setup      install web dependencies' \
	  'make generate   regenerate web/src/api/schema.ts from api/openapi.yaml' \
	  'make dev        run the synthetic mock API and the web app with HMR' \
	  'make build      production build of the web app into web/dist' \
	  'make test       mock, transcript and translation tests' \
	  'make check      generate, type check, tests and build'

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
	node --test dev/mock.test.ts web/src/transcript.test.ts web/src/i18n/catalog.test.ts

check:
	@before=$$(sha256sum web/src/api/schema.ts); $(MAKE) --no-print-directory generate >/dev/null; \
	[ "$$before" = "$$(sha256sum web/src/api/schema.ts)" ] || { echo 'api/openapi.yaml changed: review the regenerated web/src/api/schema.ts'; exit 1; }
	npm --prefix web run typecheck
	$(MAKE) --no-print-directory test
	npm --prefix web run build
