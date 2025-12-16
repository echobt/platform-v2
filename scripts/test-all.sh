#!/bin/bash
# Run all tests for mini-chain

set -e

echo "=== Mini-Chain Test Suite ==="
echo ""

cd "$(dirname "$0")/.."

# Build first
echo "[1/3] Building..."
cargo build --release 2>/dev/null

# Run all tests
echo "[2/3] Running unit tests..."
cargo test 2>&1 | tee /tmp/test-results.txt

# Count results
PASSED=$(grep -oP '\d+ passed' /tmp/test-results.txt | awk '{sum += $1} END {print sum}')
FAILED=$(grep -oP '\d+ failed' /tmp/test-results.txt | awk '{sum += $1} END {print sum}')
IGNORED=$(grep -oP '\d+ ignored' /tmp/test-results.txt | awk '{sum += $1} END {print sum}')

echo ""
echo "[3/3] Test Summary"
echo "===================="
echo "Passed:  $PASSED"
echo "Failed:  $FAILED"
echo "Ignored: $IGNORED"
echo ""

if [ "$FAILED" -gt 0 ]; then
    echo "TESTS FAILED!"
    exit 1
else
    echo "ALL TESTS PASSED!"
fi
