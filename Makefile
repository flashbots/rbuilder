# Heavily inspired by Lighthouse: https://github.com/sigp/lighthouse/blob/stable/Makefile
# and Reth: https://github.com/paradigmxyz/reth/blob/main/Makefile
.DEFAULT_GOAL := help

GIT_VER ?= $(shell git describe --tags --always --dirty="-dev")
GIT_TAG ?= $(shell git describe --tags --abbrev=0)

FEATURES ?=

##@ Help

.PHONY: help
help: ## Display this help.
	@awk 'BEGIN {FS = ":.*##"; printf "Usage:\n  make \033[36m<target>\033[0m\n"} /^[a-zA-Z_0-9-]+:.*?##/ { printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2 } /^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) } ' $(MAKEFILE_LIST)

.PHONY: v
v: ## Show the current version
	@echo "Version: ${GIT_VER}"

##@ Build

.PHONY: clean
clean: ## Clean up
	cargo clean

# Detect the current architecture
ARCH := $(shell uname -m)

# Determine if we're on x86_64
ifeq ($(ARCH),x86_64)
    IS_X86_64 = 1
else
    IS_X86_64 = 0
endif

# Set build profile and flags based on architecture
ifeq ($(IS_X86_64),1)
    # x86_64: Use reproducible profile with reproducible build flags
    BUILD_PROFILE = reproducible
    BUILD_TARGET = x86_64-unknown-linux-gnu

    # Environment variables for reproducible builds
    # Initialize RUSTFLAGS
    RUST_BUILD_FLAGS =
    # Optimize for modern CPUs
    RUST_BUILD_FLAGS += -C target-cpu=x86-64-v3
    # Remove build ID from the binary to ensure reproducibility across builds
    RUST_BUILD_FLAGS += -C link-arg=-Wl,--build-id=none
    # Remove metadata hash from symbol names to ensure reproducible builds
    RUST_BUILD_FLAGS += -C metadata=''
    # Remap paths to ensure reproducible builds
    RUST_BUILD_FLAGS += --remap-path-prefix $(shell pwd)=.
    # Set timestamp from last git commit for reproducible builds
    SOURCE_DATE ?= $(shell git log -1 --pretty=%ct)
    # Set C locale for consistent string handling and sorting
    LOCALE_VAL = C
    # Set UTC timezone for consistent time handling across builds
    TZ_VAL = UTC

    # Environment setup for reproducible builds
    BUILD_ENV = SOURCE_DATE_EPOCH=$(SOURCE_DATE) \
                RUSTFLAGS="${RUST_BUILD_FLAGS}" \
                LC_ALL=${LOCALE_VAL} \
                TZ=${TZ_VAL} \
                JEMALLOC_OVERRIDE=/usr/lib/x86_64-linux-gnu/libjemalloc.a
else
    # Non-x86_64: Use release profile without reproducible build flags
    BUILD_PROFILE = release
    BUILD_TARGET =
    RUST_BUILD_FLAGS =
    BUILD_ENV =
endif

.PHONY: build
build: ## Build (release version)
	$(BUILD_ENV) cargo build --features "$(FEATURES) jemalloc-unprefixed" --locked $(if $(BUILD_TARGET),--target $(BUILD_TARGET)) --profile $(BUILD_PROFILE) --workspace

.PHONY: build-bid-scraper
build-bid-scraper: ## Build the bid-scraper binary (release version)
	$(BUILD_ENV) cargo build --features "$(FEATURES)" --locked $(if $(BUILD_TARGET),--target $(BUILD_TARGET)) --bin bid-scraper --profile $(BUILD_PROFILE)

.PHONY: build-rbuilder-operator
build-rbuilder-operator: ## Build the rbuilder-operator binary (release version)
	$(BUILD_ENV) cargo build --features "$(FEATURES) jemalloc-unprefixed" --locked $(if $(BUILD_TARGET),--target $(BUILD_TARGET)) --bin rbuilder-operator --profile $(BUILD_PROFILE)

.PHONY: build-rbuilder-rebalancer
build-rbuilder-rebalancer: ## Build the rbuilder-rebalancer binary (release version)
	$(BUILD_ENV) cargo build --features "$(FEATURES) jemalloc-unprefixed" --locked $(if $(BUILD_TARGET),--target $(BUILD_TARGET)) --bin rbuilder-rebalancer --profile $(BUILD_PROFILE)

.PHONY: build-dev
build-dev: ## Build (debug version)
	cargo build --features "$(FEATURES)"

.PHONY: docker-image-rbuilder
docker-image-rbuilder: ## Build a rbuilder Docker image
	docker build --platform linux/amd64 --target rbuilder-runtime --build-arg FEATURES="$(FEATURES)" -t rbuilder -f docker/Dockerfile.rbuilder .

.PHONY: docker-image-rbuilder-operator
docker-image-rbuilder-operator: ## Build a rbuilder-operator Docker image
	docker build --platform linux/amd64 --target rbuilder-runtime --build-arg FEATURES="$(FEATURES) jemalloc-unprefixed" -t rbuilder-operator -f docker/Dockerfile.rbuilder-operator .

.PHONY: docker-image-test-relay
docker-image-test-relay: ## Build a test relay Docker image
	docker build --platform linux/amd64 --target test-relay-runtime --build-arg FEATURES="$(FEATURES)" . -t test-relay

##@ Debian Packages

# Define binary paths for smart dependencies
BID_SCRAPER_BIN := target/$(if $(BUILD_TARGET),$(BUILD_TARGET)/)$(BUILD_PROFILE)/bid-scraper
RBUILDER_OPERATOR_BIN := target/$(if $(BUILD_TARGET),$(BUILD_TARGET)/)$(BUILD_PROFILE)/rbuilder-operator
RBUILDER_REBALANCER_BIN := target/$(if $(BUILD_TARGET),$(BUILD_TARGET)/)$(BUILD_PROFILE)/rbuilder-rebalancer

.PHONY: install-cargo-deb
install-cargo-deb:
	@command -v cargo-deb >/dev/null 2>&1 || cargo install cargo-deb@3.6.0 --locked

# Build individual binaries only if they don't exist - delegate to existing build targets
$(BID_SCRAPER_BIN): build-bid-scraper
	@# Binary built by build-bid-scraper target

$(RBUILDER_OPERATOR_BIN): build-rbuilder-operator
	@# Binary built by build-rbuilder-operator target

$(RBUILDER_REBALANCER_BIN): build-rbuilder-rebalancer
	@# Binary built by build-rbuilder-rebalancer target

.PHONY: build-deb-bid-scraper
build-deb-bid-scraper: install-cargo-deb $(BID_SCRAPER_BIN) ## Build bid-scraper Debian package
	cargo deb --profile $(BUILD_PROFILE) --no-build --no-dbgsym --no-strip \
		-p bid-scraper \
		$(if $(BUILD_TARGET),--target $(BUILD_TARGET)) \
		$(if $(VERSION),--deb-version "1~$(VERSION)")

.PHONY: build-deb-rbuilder-operator
build-deb-rbuilder-operator: install-cargo-deb $(RBUILDER_OPERATOR_BIN) ## Build rbuilder-operator Debian package
	cargo deb --profile $(BUILD_PROFILE) --no-build --no-dbgsym --no-strip \
		-p rbuilder-operator \
		$(if $(BUILD_TARGET),--target $(BUILD_TARGET)) \
		$(if $(VERSION),--deb-version "1~$(VERSION)")

.PHONY: build-deb-rbuilder-rebalancer
build-deb-rbuilder-rebalancer: install-cargo-deb $(RBUILDER_REBALANCER_BIN) ## Build rbuilder-rebalancer Debian package
	cargo deb --profile $(BUILD_PROFILE) --no-build --no-dbgsym --no-strip \
		-p rbuilder-rebalancer \
		$(if $(BUILD_TARGET),--target $(BUILD_TARGET)) \
		$(if $(VERSION),--deb-version "1~$(VERSION)")

.PHONY: build-deb
build-deb: build-deb-bid-scraper build-deb-rbuilder-operator build-deb-rbuilder-rebalancer ## Build all Debian packages

##@ Dev

.PHONY: lint
lint: ## Run the linters
	cargo fmt -- --check
	cargo clippy --workspace --features "$(FEATURES)" -- -D warnings

.PHONY: test
test: ## Run the tests for rbuilder. At reth 1.8.2 we started getting some memory errors (when creating the tmp dbs) so we had to limit the number of threads.
	cargo test --verbose --features "$(FEATURES)" -- --test-threads=10

.PHONY: lt
lt: lint test ## Run "lint" and "test"

.PHONY: fmt
fmt: ## Format the code
	cargo fmt
	cargo fix --allow-staged
	cargo clippy --features "$(FEATURES)" --fix --allow-staged

.PHONY: bench
bench: ## Run benchmarks
	cargo bench --features "$(FEATURES)" --workspace

.PHONY: bench-report-open
bench-report-open: ## Open last benchmark report in the browser
	open "target/criterion/report/index.html"

.PHONY: bench-in-ci
bench-in-ci: ## Run benchmarks in CI (adds timestamp and version to the report, customizes Criterion output)
	./scripts/ci/benchmark-in-ci.sh

.PHONY: bench-clean
bench-clean: ## Remove previous benchmark data
	rm -rf target/criterion
	rm -rf target/benchmark-in-ci
	rm -rf target/benchmark-html-dev

.PHONY: bench-prettify
bench-prettify: ## Prettifies the latest Criterion report
	rm -rf target/benchmark-html-dev
	./scripts/ci/criterion-prettify-report.sh target/criterion target/benchmark-html-dev
	@echo "\nopen target/benchmark-html-dev/report/index.html"

.PHONY: validate-config
validate-config: ## Validate the correctness of the configuration files
	@for CONFIG in $(shell ls config-*.toml); do \
		cargo run --bin validate-config -- --config $$CONFIG; \
	done
