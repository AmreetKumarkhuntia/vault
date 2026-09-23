# Development commands. `make help` lists everything.

PG_URL    ?= postgres://$(USER)@localhost:5432/vault_demo
REDIS_URL ?= redis://127.0.0.1:6379/15
MOCK_BASE ?= http://127.0.0.1:9095
PORT      ?= 8091
ARGS      ?=

PID  := target/demo-target.pid
BIN  := ./target/debug

.PHONY: help build test validate list env suite negative test-all demo \
        target-start target-stop db-setup stores-up stores-down clean \
        npm-pack release-dry-run

help:
	@echo "make build         build the whole workspace"
	@echo "make test          Rust tests for the harness code (tests/code)"
	@echo "make suite         start demo-target, run the YAML flow suite, stop it"
	@echo "make negative      run the deliberately-failing showcase tests (exit 1 is the point)"
	@echo "make test-all      test + suite + negative"
	@echo "make validate      static-check every YAML file, no execution"
	@echo "make list          resolved run plan"
	@echo "make env           print the env the target should be started with"
	@echo "make demo          full guided demo (scripts/demo.sh)"
	@echo "make target-start  start demo-target in the background (logs: demo-target.log)"
	@echo "make target-stop   stop it"
	@echo "make db-setup      create the vault_demo database"
	@echo "make stores-up     docker compose Postgres+Redis (ports 5433/6380)"
	@echo "make clean         cargo clean + logs"
	@echo ""
	@echo "Pass suite args with ARGS: make suite ARGS='-t smoke -v'"

build:
	cargo build --workspace

test:
	cargo test --workspace

validate: build
	$(BIN)/vault validate

list: build
	$(BIN)/vault list

env: build
	$(BIN)/vault env

db-setup:
	createdb vault_demo 2>/dev/null || true

target-start: build db-setup
	@mkdir -p target
	@if [ -f $(PID) ] && kill -0 $$(cat $(PID)) 2>/dev/null; then \
		echo "demo-target already running (pid $$(cat $(PID)))"; \
	else \
		PORT=$(PORT) DATABASE_URL=$(PG_URL) REDIS_URL=$(REDIS_URL) \
		PAYMENTS_URL=$(MOCK_BASE)/payments EMAIL_URL=$(MOCK_BASE)/email \
		$(BIN)/demo-target > demo-target.log 2>&1 & echo $$! > $(PID); \
		sleep 1; \
		echo "demo-target started (pid $$(cat $(PID)), logs: demo-target.log)"; \
	fi

target-stop:
	@if [ -f $(PID) ]; then kill $$(cat $(PID)) 2>/dev/null || true; rm -f $(PID); echo "demo-target stopped"; fi

suite: target-start
	@$(BIN)/vault run $(ARGS); code=$$?; $(MAKE) -s target-stop; exit $$code

negative: target-start
	@VAULT_RUN_NEGATIVE= $(BIN)/vault run -t negative $(ARGS); code=$$?; \
	$(MAKE) -s target-stop; \
	if [ $$code -eq 1 ]; then echo "exit 1 as designed — that's the showcase"; exit 0; \
	else echo "expected exit 1, got $$code"; exit 1; fi

test-all: test suite negative

demo:
	./scripts/demo.sh

npm-pack:
	cd npm && npm pack

release-dry-run:
	gh workflow run release.yaml

stores-up:
	docker compose up -d

stores-down:
	docker compose down

clean: target-stop
	cargo clean
	rm -f demo-target.log
