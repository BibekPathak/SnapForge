# SnapForge Makefile
#
# The workspace target directory is redirected to /tmp because the home
# filesystem on the development host is nearly full. Override with:
#   make TARGET_DIR=target ...

TARGET_DIR ?= /tmp/snapforge-target
export CARGO_TARGET_DIR := $(TARGET_DIR)

CARGO ?= cargo

.PHONY: all build release test lint fmt fmt-check clippy check doctor workloads clean

all: check

build:
	$(CARGO) build --workspace

release:
	$(CARGO) build --workspace --release

test:
	$(CARGO) test --workspace

lint: fmt-check clippy

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

clippy:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

check: fmt-check clippy test

# Convenience targets
doctor:
	$(CARGO) run --release -p snapforge-cli -- doctor

workloads:
	$(CARGO) build --workspace
	@echo "Workload binaries built under $(CARGO_TARGET_DIR)/debug/"

clean:
	$(CARGO) clean
