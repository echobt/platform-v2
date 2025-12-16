#!/bin/bash
# Run a local 8-validator network for testing

set -e

echo "=== Mini-Chain Local Network ==="
echo ""

# Build first
echo "[1/3] Building validators..."
cargo build --release --bin validator-node

# Create data directories
echo "[2/3] Creating data directories..."
mkdir -p data/validator-{1..8}

# Generate deterministic keys for testing
KEYS=(
    "0000000000000000000000000000000000000000000000000000000000000001"
    "0000000000000000000000000000000000000000000000000000000000000002"
    "0000000000000000000000000000000000000000000000000000000000000003"
    "0000000000000000000000000000000000000000000000000000000000000004"
    "0000000000000000000000000000000000000000000000000000000000000005"
    "0000000000000000000000000000000000000000000000000000000000000006"
    "0000000000000000000000000000000000000000000000000000000000000007"
    "0000000000000000000000000000000000000000000000000000000000000008"
)

echo "[3/3] Starting validators..."
echo ""

# Start validator 1 (bootstrap node)
echo "Starting validator 1 (bootstrap)..."
RUST_LOG=info,platform_chain=debug ./target/release/validator-node \
    --secret-key ${KEYS[0]} \
    --listen /ip4/127.0.0.1/tcp/9001 \
    --data-dir ./data/validator-1 \
    --stake 100 &
PIDS[1]=$!

sleep 2

# Start validators 2-8
for i in {2..8}; do
    PORT=$((9000 + i))
    echo "Starting validator $i on port $PORT..."
    RUST_LOG=info,platform_chain=info ./target/release/validator-node \
        --secret-key ${KEYS[$i-1]} \
        --listen /ip4/127.0.0.1/tcp/$PORT \
        --bootstrap /ip4/127.0.0.1/tcp/9001 \
        --data-dir ./data/validator-$i \
        --stake 100 &
    PIDS[$i]=$!
    sleep 0.5
done

echo ""
echo "=== Network Running ==="
echo "Validators: 8"
echo "Bootstrap: /ip4/127.0.0.1/tcp/9001"
echo ""
echo "Press Ctrl+C to stop all validators"
echo ""

# Wait for Ctrl+C
trap 'echo "Stopping..."; for pid in ${PIDS[@]}; do kill $pid 2>/dev/null; done; exit 0' INT

wait
