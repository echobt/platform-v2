#!/bin/bash
set -e

echo "Building chain-runtime WASM..."

# Ensure wasm32 target is installed
rustup target add wasm32-unknown-unknown 2>/dev/null || true

# Build the WASM runtime
cargo build --release --target wasm32-unknown-unknown \
    -p mini-chain-chain-runtime \
    --no-default-features

WASM_PATH="target/wasm32-unknown-unknown/release/platform_chain_chain_runtime.wasm"

if [ -f "$WASM_PATH" ]; then
    SIZE=$(du -h "$WASM_PATH" | cut -f1)
    echo "WASM built successfully: $WASM_PATH ($SIZE)"
    
    # Optimize with wasm-opt if available
    if command -v wasm-opt &> /dev/null; then
        echo "Optimizing WASM with wasm-opt..."
        wasm-opt -Oz -o "${WASM_PATH%.wasm}_optimized.wasm" "$WASM_PATH"
        OPT_SIZE=$(du -h "${WASM_PATH%.wasm}_optimized.wasm" | cut -f1)
        echo "Optimized WASM: ${WASM_PATH%.wasm}_optimized.wasm ($OPT_SIZE)"
    else
        echo "wasm-opt not found. Install with: cargo install wasm-opt"
    fi
else
    echo "ERROR: WASM build failed"
    exit 1
fi
