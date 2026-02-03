#!/bin/bash
# Tear down the sandbox environment

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
COMPOSE_FILE="docker/docker-compose.sandbox.yml"
KEEP_DATA=false

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --keep-data)
            KEEP_DATA=true
            shift
            ;;
        -h|--help)
            echo "Usage: $0 [OPTIONS]"
            echo ""
            echo "Options:"
            echo "  --keep-data    Preserve volumes (do not remove data)"
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

echo "=== Stopping Sandbox Environment ==="
echo ""

# Check for required tools
echo "[1/4] Checking prerequisites..."

if ! command -v docker &> /dev/null; then
    echo "ERROR: docker is not installed or not in PATH"
    exit 1
fi

if ! docker compose version &> /dev/null; then
    echo "ERROR: docker compose is not available"
    exit 1
fi

echo "  ✓ docker found"
echo ""

# Change to project root
cd "$PROJECT_ROOT"
echo "[2/4] Working directory: $PROJECT_ROOT"

# Check if compose file exists
if [ ! -f "$COMPOSE_FILE" ]; then
    echo "WARNING: Compose file not found: $COMPOSE_FILE"
    echo "Attempting to clean up any orphan containers anyway..."
fi

echo ""
echo "[3/4] Stopping containers..."

if [ -f "$COMPOSE_FILE" ]; then
    # Stop all containers defined in compose file
    docker compose -f "$COMPOSE_FILE" down --remove-orphans || true
    
    if [ "$KEEP_DATA" = false ]; then
        echo ""
        echo "[4/4] Removing volumes..."
        docker compose -f "$COMPOSE_FILE" down -v --remove-orphans || true
        echo "  ✓ Volumes removed"
    else
        echo ""
        echo "[4/4] Keeping volumes (--keep-data specified)"
        echo "  ✓ Data preserved"
    fi
else
    echo "[4/4] Skipping compose-based cleanup (no compose file)"
fi

# Clean up any orphan containers with sandbox-related names
echo ""
echo "Cleaning up orphan containers..."
orphans=$(docker ps -a --filter "name=sandbox" --filter "name=platform-validator" -q 2>/dev/null || true)
if [ -n "$orphans" ]; then
    echo "$orphans" | xargs -r docker rm -f 2>/dev/null || true
    echo "  ✓ Orphan containers removed"
else
    echo "  ✓ No orphan containers found"
fi

# Clean up sandbox network if it exists
echo ""
echo "Cleaning up networks..."
sandbox_networks=$(docker network ls --filter "name=sandbox" -q 2>/dev/null || true)
if [ -n "$sandbox_networks" ]; then
    echo "$sandbox_networks" | xargs -r docker network rm 2>/dev/null || true
    echo "  ✓ Sandbox networks removed"
else
    echo "  ✓ No sandbox networks to remove"
fi

echo ""
echo "=== Sandbox environment stopped ==="
if [ "$KEEP_DATA" = true ]; then
    echo "Note: Data volumes were preserved. Run without --keep-data to remove them."
fi
