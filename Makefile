.DEFAULT_GOAL := help

# Cargo profile for builds. Default is for local builds, CI uses an override.
PROFILE ?= release
FEATURES ?= jemalloc,asm-keccak
##@ Help

.PHONY: help
help: ## Display this help.
	@awk 'BEGIN {FS = ":.*##"; printf "Usage:\n  make \033[36m<target>\033[0m\n"} /^[a-zA-Z_0-9-]+:.*?##/ { printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2 } /^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) } ' $(MAKEFILE_LIST)

.PHONY: build
build: ## Build the reth binary into `target` directory.
	cargo build --bin reth-bsc --features "$(FEATURES)" --profile "$(PROFILE)"

.PHONY: maxperf
maxperf: ## Builds `reth-bsc` with the most aggressive optimisations.
	RUSTFLAGS="-C target-cpu=native" cargo build --bin reth-bsc --profile maxperf --features jemalloc,asm-keccak

.PHONY: bench-test
bench-test: ## Builds `reth-bsc` with the bench-test feature.
	RUSTFLAGS="-C target-cpu=native" cargo build --profile maxperf --features jemalloc,asm-keccak,bench-test

.PHONY: reth-bench
reth-bench: ## Build the reth-bench binary into the `target` directory.
	cargo build --manifest-path bin/reth-bench/Cargo.toml --features "$(FEATURES)" --profile "$(PROFILE)"
	@echo "reth-bench-bsc built successfully"
	@echo "Location: bin/reth-bench/target/$(PROFILE)/reth-bench-bsc"
	@echo ""

check-features:
	cargo hack check \
		--package reth-codecs \
		--package reth-primitives-traits \
		--package reth-primitives \
		--feature-powerset

##@ EVM Tests

# Execution spec tests configuration
EEST_VERSION ?= v5.4.0
EEST_URL := https://github.com/ethereum/execution-spec-tests/releases/download/$(EEST_VERSION)/fixtures_stable.tar.gz
EEST_DIR := ./testing/bsc-ef-tests/execution-spec-tests

$(EEST_DIR):
	@echo "Downloading execution spec tests $(EEST_VERSION)..."
	mkdir -p $(EEST_DIR)
	curl -sL $(EEST_URL) | tar -xzf - --strip-components=1 -C $(EEST_DIR)

.PHONY: download-eest
download-eest: $(EEST_DIR) ## Download execution spec test fixtures

.PHONY: ef-tests
ef-tests: $(EEST_DIR) ## Run BSC EVM execution spec tests
	cargo test -p bsc-ef-tests --release --features ef-tests,jemalloc,asm-keccak

.PHONY: ef-tests-nextest
ef-tests-nextest: $(EEST_DIR) ## Run BSC EVM execution spec tests with nextest (faster)
	cargo nextest run -p bsc-ef-tests --release --features ef-tests,jemalloc,asm-keccak

.PHONY: clean-eest
clean-eest: ## Remove downloaded execution spec test fixtures
	rm -rf $(EEST_DIR)
