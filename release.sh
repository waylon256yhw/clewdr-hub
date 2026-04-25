#!/bin/bash
set -euo pipefail

VERSION=${1:-}
if [ -z "$VERSION" ]; then
    echo "Usage: ./release.sh <version> (e.g., 1.0.0)"
    exit 1
fi

TAG="v$VERSION"

if [ -n "$(git status --porcelain)" ]; then
    echo "Working tree is not clean. Commit or stash changes before releasing."
    exit 1
fi

if [ "${RELEASE_UPDATE_DEPS:-0}" = "1" ]; then
    cargo update
fi

cargo set-version "$VERSION"
cargo test
(cd frontend && npm ci && npm run build)
cargo check

git add Cargo.toml Cargo.lock

if git diff --cached --quiet; then
    echo "Cargo version is already $VERSION; skipping version commit."
else
    git commit -m "Update to $TAG"
fi

git push

if ! git rev-parse -q --verify "refs/tags/$TAG" >/dev/null &&
    git ls-remote --exit-code --tags origin "refs/tags/$TAG" >/dev/null 2>&1; then
    echo "Tag $TAG already exists on origin; fetching it before validation."
    git fetch origin "refs/tags/$TAG:refs/tags/$TAG"
fi

if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
    TAG_COMMIT=$(git rev-list -n 1 "$TAG")
    HEAD_COMMIT=$(git rev-parse HEAD)

    if [ "$TAG_COMMIT" != "$HEAD_COMMIT" ]; then
        echo "Tag $TAG already exists but points to $TAG_COMMIT, not HEAD $HEAD_COMMIT."
        exit 1
    fi

    echo "Tag $TAG already points to HEAD; skipping tag creation."
else
    git tag -a "$TAG" -m "Release $TAG"
fi

git push origin "$TAG"
