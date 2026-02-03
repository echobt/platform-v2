#!/bin/bash
# Spin up the multi-validator sandbox environment

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
COMPOSE_FILE="docker/docker-compose.sandbox.yml"
HEALTH_TIMEOUT=60
POLL_INTERVAL=5

echo "=== Platform Sandbox Environment ==="
echo ""

# Check for required tools
echo "[1/5] Checking prerequisites..."

if ! command -v docker &> /dev/null; then
    echo "ERROR: docker is not installed or not in PATH"
    exit 1
fi

if ! docker compose version &> /dev/null; then
    echo "ERROR: docker compose is not available"
    echo "Hint: Install Docker Compose V2 or ensure 'docker compose' command works"
    exit 1
fi

echo "  ✓ docker found"
echo "  ✓ docker compose found"
echo ""

# Change to project root
cd "$PROJECT_ROOT"
echo "[2/5] Working directory: $PROJECT_ROOT"
echo ""

# Verify compose file exists
if [ ! -f "$COMPOSE_FILE" ]; then
    echo "ERROR: Compose file not found: $COMPOSE_FILE"
    echo "Hint: Ensure docker/docker-compose.sandbox.yml exists"
    exit 1
fi

echo "[3/5] Starting sandbox containers..."
docker compose -f "$COMPOSE_FILE" up -d --build

echo ""
echo "[4/5] Waiting for validators to be healthy (timeout: ${HEALTH_TIMEOUT}s)..."

elapsed=0
while [ $elapsed -lt $HEALTH_TIMEOUT ]; do
    # Check if all containers are healthy or running
    unhealthy_count=$(docker compose -f "$COMPOSE_FILE" ps --format json 2>/dev/null | \
        grep -c '"Health":"starting"' || true)
    
    if [ "$unhealthy_count" -eq 0 ]; then
        # Double-check by getting actual health status
        all_healthy=true
        while IFS= read -r container; do
            if [ -n "$container" ]; then
                health=$(docker inspect --format='{{.State.Health.Status}}' "$container" 2>/dev/null || echo "none")
                if [ "$health" = "starting" ]; then
                    all_healthy=false
                    break
                fi
            fi
        done < <(docker compose -f "$COMPOSE_FILE" ps -q 2>/dev/null)
        
        if [ "$all_healthy" = true ]; then
            echo "  ✓ All containers are healthy"
            break
        fi
    fi
    
    echo "  Waiting... (${elapsed}s elapsed)"
    sleep $POLL_INTERVAL
    elapsed=$((elapsed + POLL_INTERVAL))
done

if [ $elapsed -ge $HEALTH_TIMEOUT ]; then
    echo "  ⚠ Warning: Health check timeout reached"
    echo "  Some containers may still be starting up"
fi

echo ""
echo "[5/5] Container Status:"
echo "========================"
docker compose -f "$COMPOSE_FILE" ps

echo ""
echo "=== Connection Information ==="
echo ""

# Extract peer IDs and connection info from running containers
for container in $(docker compose -f "$COMPOSE_FILE" ps -q 2>/dev/null); do
    name=$(docker inspect --format='{{.Name}}' "$container" | sed 's/^\///')
    
    # Get exposed ports
    ports=$(docker port "$container" 2>/dev/null || echo "none")
    
    echo "Container: $name"
    if [ -n "$ports" ] && [ "$ports" != "none" ]; then
        echo "  Ports:"
        echo "$ports" | sed 's/^/    /'
    fi
    
    # Try to get peer ID from logs if available
    peer_id=$(docker logs "$container" 2>&1 | grep -oP 'Peer ID: \K[a-zA-Z0-9]+' | tail -1 || true)
    if [ -n "$peer_id" ]; then
        echo "  Peer ID: $peer_id"
    fi
    
    echo ""
done

echo "=== Sandbox is running ==="
echo ""
echo "To view logs:        docker compose -f $COMPOSE_FILE logs -f"
echo "To stop:             ./scripts/sandbox-down.sh"
echo "To run tests:        ./scripts/run-sandbox-tests.sh"
