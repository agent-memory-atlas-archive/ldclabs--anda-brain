BUILD_ENV := rust

.PHONY: lint fix test

lint:
	@cargo fmt
	@cargo clippy --all-targets --all-features

fix:
	@cargo fmt --all
	@cargo clippy --fix --workspace --tests

test:
	@RUST_MIN_STACK=16777216 cargo test --workspace --all-features -- --nocapture
