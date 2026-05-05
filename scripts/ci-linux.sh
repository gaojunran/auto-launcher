#!/usr/bin/env bash
set -e

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
TMP_SCRIPT=$(mktemp)

echo "=== Running Linux CI in Docker (rust:latest) ==="

# Write inner script using quoted heredoc to prevent host expansion
cat > "$TMP_SCRIPT" << 'INNER'
set -e
apt-get update -qq
apt-get install -y -qq curl pkg-config libssl-dev
curl -fsSL https://mise.run | sh
export PATH="$HOME/.local/bin:$PATH"
mise install
eval "$(mise env)"
mise run ci
INNER'

docker run --rm \
  -v "$PROJECT_DIR:/workspace" \
  -v "$TMP_SCRIPT:/inner.sh" \
  -w /workspace \
  -e CARGO_INCREMENTAL=0 \
  -e RUST_BACKTRACE=short \
  rust:latest \
  bash /inner.sh

rm -f "$TMP_SCRIPT"
echo "=== Linux CI passed ==="
