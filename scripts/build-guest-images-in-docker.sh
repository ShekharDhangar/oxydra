#!/usr/bin/env bash
set -euo pipefail
# Builds both guest Docker images from source.
# Usage: ./scripts/build-guest-images-in-docker.sh [amd64|arm64] [TAG]

ARCH="${1:-}"
TAG="${2:-latest}"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

if [[ -z "$ARCH" ]]; then
  case "$(uname -m)" in
    x86_64|amd64) ARCH="amd64" ;;
    aarch64|arm64) ARCH="arm64" ;;
    *)
      echo "Error: unsupported host architecture: $(uname -m)" >&2
      echo "Usage: $0 [amd64|arm64] [TAG]" >&2
      exit 1
      ;;
  esac
fi

case "$ARCH" in
  amd64) PLATFORM="linux/amd64" ;;
  arm64) PLATFORM="linux/arm64" ;;
  *)
    echo "Error: ARCH must be amd64 or arm64" >&2
    echo "Usage: $0 [amd64|arm64] [TAG]" >&2
    exit 1
    ;;
esac

docker build --platform "$PLATFORM" --target oxydra-vm -t "oxydra-vm:$TAG" -f "$REPO_ROOT/docker/Dockerfile" "$REPO_ROOT"
docker build --platform "$PLATFORM" --target shell-vm -t "shell-vm:$TAG" -f "$REPO_ROOT/docker/Dockerfile" "$REPO_ROOT"

echo "Built oxydra-vm:$TAG and shell-vm:$TAG for $PLATFORM"
