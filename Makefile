.PHONY: help build test fmt fmt-check lint check fetch-model run-mock run bench bench-smoke bench-one co-demo policy-compare policy-matrix overhead validate-hit-rate clean

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
	@echo "bench      regenerate every number in RESULTS.md, about an hour"
	@echo "bench-smoke  the same pipeline at toy settings, a few minutes,"
	@echo "             into results/smoke and not publishable"
	@echo "bench-one  three open-loop runs against a router you started yourself"
	@echo ""
	@echo "the pieces make bench runs, if you want one on its own:"
	@echo "co-demo    the coordinated-omission comparison, start to finish"
	@echo "policy-compare  routing policies on one workload shape"
	@echo "policy-matrix   every policy against every workload shape"
	@echo "overhead   what the router itself costs, against one worker"
	@echo "validate-hit-rate  the router prediction against the workers own counters"

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

# The name the spec's exit criterion uses, so it does what the exit criterion
# says: every published number, regenerated, charts included.
bench:
	./scripts/reproduce.sh

# Same pipeline, seconds instead of minutes per arm, into results/smoke. Run
# this first to find out whether the machine can do it at all.
bench-smoke:
	./scripts/reproduce.sh --smoke

# One configuration against a router you started yourself, for when you are
# changing something and want a number in two minutes. Feeds no published table.
bench-one:
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

# Needs a fleet already serving traffic. See docs/GPU-RUNBOOK.md.
validate-hit-rate:
	./scripts/validate-hit-rate.sh

clean:
	cargo clean
