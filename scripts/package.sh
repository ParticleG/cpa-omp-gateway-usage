#!/usr/bin/env bash
set -euo pipefail

version="${1:?Usage: bash scripts/package.sh <major.minor.patch>}"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  printf 'Expected a numeric major.minor.patch version, got %s\n' "$version" >&2
  exit 2
fi

cd "$(dirname "$0")/.."
plugin=omp-gateway-usage
image="local/cpa-omp-gateway-usage:${version}"
archive="${plugin}_${version}_linux_amd64.zip"

docker build --platform linux/amd64 --build-arg GOPROXY \
  --build-arg "VERSION=${version}" --tag "$image" .
mkdir -p dist
container="$(docker create "$image" /unused)"
trap 'docker rm "$container" >/dev/null' EXIT
docker cp "${container}:/${plugin}.so" "dist/${plugin}.so"

# Rebuilding a local archive must not retain entries from an older ZIP.
rm -f "dist/${archive}"
zip -j -X "dist/${archive}" "dist/${plugin}.so" LICENSE THIRD_PARTY_NOTICES.md
(
  cd dist
  sha256sum "$archive" > checksums.txt
)
printf 'Release assets: dist/%s and dist/checksums.txt\n' "$archive"
