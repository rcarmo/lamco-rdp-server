#!/usr/bin/env bash
set -euo pipefail

# lamco-rdp-server Debian package build script
# Uses Docker builder image and bind-mounts ONLY this project directory at /src.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

DOCKER_IMAGE="lamco-rdp-builder:reuse"
CONTAINER_NAME="rdp-build"
OUT_DIR="$SCRIPT_DIR/dist"

echo "=== lamco-rdp-server deb builder ==="
echo "Working dir: $SCRIPT_DIR"
echo "Docker image: $DOCKER_IMAGE"
echo "Mount: $SCRIPT_DIR -> /src"
echo "Output dir: $OUT_DIR"
echo ""

if ! docker image inspect "$DOCKER_IMAGE" >/dev/null 2>&1; then
    echo "ERROR: Docker image not found: $DOCKER_IMAGE" >&2
    exit 1
fi

mkdir -p "$OUT_DIR"

# Always recreate the builder so stale containers/mounts cannot survive.
if docker ps -a --format '{{.Names}}' | grep -qx "$CONTAINER_NAME"; then
    echo "Removing old builder container..."
    docker rm -f "$CONTAINER_NAME" >/dev/null
fi

echo "Starting builder container with current project bind mount..."
# Network is not needed because dpkg/cargo run with local source and Cargo.lock.
# --network none also avoids host Docker bridge/veth failures.
docker run -d \
    --name "$CONTAINER_NAME" \
    --network none \
    -v "$SCRIPT_DIR":/src:rw \
    "$DOCKER_IMAGE" \
    sleep infinity >/dev/null

cleanup() {
    # debian/ is generated from packaging/debian and contains root-owned build
    # artifacts because the container writes through the bind mount. Remove it
    # from inside the container before deleting the container.
    docker exec "$CONTAINER_NAME" rm -rf /src/debian >/dev/null 2>&1 || true
    docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# Verify source mount points at the project root, not the workspace parent.
docker exec "$CONTAINER_NAME" test -f /src/Cargo.toml

echo ""
echo "=== Preparing debian/ directory from packaging/debian/ ==="
docker exec "$CONTAINER_NAME" bash -c '
    set -euo pipefail
    cd /src
    test -d packaging/debian
    rm -rf debian
    cp -a packaging/debian debian
    test -f debian/changelog
    test -f debian/control
    test -x debian/rules || chmod +x debian/rules

    # The upstream docs file may reference vendored NOTICE files that are absent
    # in this checkout. Filter only the generated debian/ copy so packaging/debian
    # remains unchanged.
    if [ -f debian/lamco-rdp-server.docs ]; then
        tmp=$(mktemp)
        while IFS= read -r path; do
            [ -z "$path" ] && continue
            if [ -e "$path" ]; then
                printf "%s\n" "$path" >> "$tmp"
            else
                echo "Skipping missing docs entry: $path" >&2
            fi
        done < debian/lamco-rdp-server.docs
        mv "$tmp" debian/lamco-rdp-server.docs
    fi

    echo "Debian metadata ready:"
    find debian -maxdepth 2 -type f | sort
'

echo ""
echo "=== Building Debian package ==="
# -d skips dpkg build-dependency checks because rust/cargo are installed via rustup in the builder, not apt.
# -nc avoids debian/rules clean (cargo clean), preserving target/ for retries.
docker exec "$CONTAINER_NAME" bash -c '
    set -euo pipefail
    export PATH=/usr/local/cargo/bin:$PATH
    cd /src
    dpkg-buildpackage -us -uc -b -d -nc
'

echo ""
echo "=== Copying package artifacts from container ==="
ARTIFACTS=$(docker exec "$CONTAINER_NAME" bash -c 'find / -maxdepth 1 -type f \( -name "*.deb" -o -name "*.buildinfo" -o -name "*.changes" \) -printf "%f\n" | sort')
if [ -z "$ARTIFACTS" ]; then
    echo "ERROR: No package artifacts produced in container" >&2
    exit 1
fi
while IFS= read -r artifact; do
    [ -n "$artifact" ] || continue
    docker cp "$CONTAINER_NAME:/$artifact" "$OUT_DIR/$artifact"
done <<< "$ARTIFACTS"

echo ""
echo "=== Build complete ==="
DEB_FILE=$(ls -t "$OUT_DIR"/*.deb 2>/dev/null | head -1 || true)
if [ -z "$DEB_FILE" ]; then
    echo "ERROR: No .deb file found in $OUT_DIR" >&2
    exit 1
fi

echo "Package: $DEB_FILE"
sha256sum "$DEB_FILE"

echo ""
echo "=== Package contents ==="
dpkg-deb -c "$DEB_FILE" | grep -E '(lamco-rdp-server|\.service|\.socket)$' || true

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"; cleanup' EXIT
dpkg-deb -x "$DEB_FILE" "$TMPDIR"
if [ -f "$TMPDIR/usr/bin/lamco-rdp-server" ]; then
    echo ""
    echo "=== Binary info ==="
    ls -lh "$TMPDIR/usr/bin/lamco-rdp-server"
    sha256sum "$TMPDIR/usr/bin/lamco-rdp-server"
    "$TMPDIR/usr/bin/lamco-rdp-server" --version 2>/dev/null || true
fi
rm -rf "$TMPDIR"

echo ""
echo "To install, run:"
echo "  sudo dpkg -i $DEB_FILE"
echo "  systemctl --user restart lamco-rdp-server.socket"
