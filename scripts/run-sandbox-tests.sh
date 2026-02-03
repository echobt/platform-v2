#!/bin/bash
# Run sandbox integration tests

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
COMPOSE_FILE="docker/docker-compose.sandbox.yml"
CLEANUP=false
TEST_EXIT_CODE=0

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --cleanup)
            CLEANUP=true
            shift
            ;;
        -h|--help)
            echo "Usage: $0 [OPTIONS]"
            echo ""
            echo "Options:"
            echo "  --cleanup      Tear down sandbox after tests complete"
            echo "  -h, --help     Show this help message"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            echo "Use --help for usage information"
            exit 1
            ;;
    esac
done

echo "=== Platform Sandbox Integration Tests ==="
echo ""

# Check for required tools
echo "[1/5] Checking prerequisites..."

if ! command -v docker &> /dev/null; then
    echo "ERROR: docker is not installed or not in PATH"
    exit 1
fi

if ! docker compose version &> /dev/null; then
    echo "ERROR: docker compose is not available"
    exit 1
fi

if ! command -v cargo &> /dev/null; then
    echo "ERROR: cargo is not installed or not in PATH"
    exit 1
fi

echo "  ✓ docker found"
echo "  ✓ docker compose found"
echo "  ✓ cargo found"
echo ""

# Change to project root
cd "$PROJECT_ROOT"
echo "[2/5] Working directory: $PROJECT_ROOT"
echo ""

# Check if sandbox is running
echo "[3/5] Checking sandbox status..."

sandbox_running=false
if [ -f "$COMPOSE_FILE" ]; then
    running_containers=$(docker compose -f "$COMPOSE_FILE" ps -q 2>/dev/null | wc -l)
    if [ "$running_containers" -gt 0 ]; then
        sandbox_running=true
        echo "  ✓ Sandbox is running ($running_containers containers)"
    fi
fi

if [ "$sandbox_running" = false ]; then
    echo "  Sandbox is not running. Starting it..."
    echo ""
    
    if [ -x "$SCRIPT_DIR/sandbox-up.sh" ]; then
        "$SCRIPT_DIR/sandbox-up.sh"
    else
        bash "$SCRIPT_DIR/sandbox-up.sh"
    fi
    
    echo ""
    echo "  ✓ Sandbox started"
fi

echo ""
echo "[4/5] Running sandbox integration tests..."
echo "=========================================="
echo ""

# Run cargo tests with sandbox filter
# Using set +e to capture exit code without failing immediately
set +e
cargo test --package platform-e2e-tests sandbox 2>&1 | tee /tmp/sandbox-test-results.txt
TEST_EXIT_CODE=${PIPESTATUS[0]}
set -e

echo ""
echo "=========================================="
echo ""

# Parse test results
echo "[5/5] Test Summary"
echo "===================="

PASSED=$(grep -oP '\d+ passed' /tmp/sandbox-test-results.txt 2>/dev/null | awk '{sum += $1} END {print sum+0}')
FAILED=$(grep -oP '\d+ failed' /tmp/sandbox-test-results.txt 2>/dev/null | awk '{sum += $1} END {print sum+0}')
IGNORED=$(grep -oP '\d+ ignored' /tmp/sandbox-test-results.txt 2>/dev/null | awk '{sum += $1} END {print sum+0}')

echo "Passed:  ${PASSED:-0}"
echo "Failed:  ${FAILED:-0}"
echo "Ignored: ${IGNORED:-0}"
echo ""

# Cleanup if requested
if [ "$CLEANUP" = true ]; then
    echo "Cleaning up sandbox environment (--cleanup specified)..."
    if [ -x "$SCRIPT_DIR/sandbox-down.sh" ]; then
        "$SCRIPT_DIR/sandbox-down.sh"
    else
        bash "$SCRIPT_DIR/sandbox-down.sh"
    fi
    echo ""
fi

# Report final status
if [ "$TEST_EXIT_CODE" -eq 0 ]; then
    echo "=== SANDBOX TESTS PASSED ==="
else
    echo "=== SANDBOX TESTS FAILED ==="
    echo "Exit code: $TEST_EXIT_CODE"
fi

exit $TEST_EXIT_CODE
