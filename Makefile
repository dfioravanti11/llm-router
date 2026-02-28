.PHONY: help build test fmt fmt-check lint check fetch-model run-mock run bench co-demo policy-compare policy-matrix overhead clean

help:
	@echo "build      compile the workspace"
	@echo "test       run the test suite"
	@echo "fmt        format the workspace"
	@echo "fmt-check  fail if the workspace is unformatted"
	@echo "lint       clippy with warnings denied"
	@echo "check      fmt-check + lint + test, the same gate CI runs"
	@echo "fetch-model  download the model tokenizer and chat template"
	@echo "run-mock   start one mock worker on 127.0.0.1:8001"
	@echo "run        start the router with config/warmpath.toml"
	@echo "bench      three open-loop runs against a running router"
	@echo "co-demo    the coordinated-omission comparison, start to finish"
	@echo "policy-compare  routing policies on one workload shape"
	@echo "policy-matrix   every policy against every workload shape"
	@echo "overhead   what the router itself costs, against one worker"

build:
	cargo build --workspace

test:
	cargo test --workspace --all-targets

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all --check

lint:
	cargo clippy --workspace --all-targets -- -D warnings

check: fmt-check lint test

fetch-model:
	./scripts/fetch-model.sh

run-mock:
	cargo run -p warmpath-mock -- --bind 127.0.0.1:8001 --cache-blocks 4096

run:
	cargo run -p warmpath -- --config config/warmpath.toml

# Expects a router already listening on 127.0.0.1:8080.
bench:
	cargo run --release -p warmpath-bench -- run \
	  --target http://127.0.0.1:8080 \
	  --rate 50 --duration 30 --warmup 5 --runs 3 \
	  --out results/baseline

co-demo:
	./scripts/co-demo.sh

policy-compare:
	./scripts/policy-compare.sh

policy-matrix:
	./scripts/policy-matrix.sh

overhead:
	./scripts/overhead.sh

clean:
	cargo clean
