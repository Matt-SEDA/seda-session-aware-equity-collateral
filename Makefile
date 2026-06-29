.PHONY: build check clean fmt

clean:
	cargo clean

fmt:
	cargo +nightly fmt --all

build:
	cargo build --target wasm32-wasip1 --profile release-wasm
	wasm-strip target/wasm32-wasip1/release-wasm/session-aware-equity-collateral.wasm;
	wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int target/wasm32-wasip1/release-wasm/session-aware-equity-collateral.wasm -o target/wasm32-wasip1/release-wasm/session-aware-equity-collateral.wasm;

install-tools:
	bun install
