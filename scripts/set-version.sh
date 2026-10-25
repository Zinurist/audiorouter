#!/usr/bin/env bash
# Sets the app version X.Y in Cargo.toml and Cargo.lock. Cargo needs three numbers, so the files
# say X.Y.0; tags and releases use vX.Y.
#
#   scripts/set-version.sh X.Y
set -euo pipefail

version=${1:?usage: $0 X.Y}
if ! [[ $version =~ ^[0-9]+\.[0-9]+$ ]]; then
  echo "error: the version must be X.Y (e.g. 1.4), got: $version" >&2
  exit 1
fi
cd "$(dirname "$0")/.."

sed -i.bak "0,/^version = \".*\"/s//version = \"$version.0\"/" Cargo.toml && rm Cargo.toml.bak
cargo update --workspace --offline --quiet
echo "Version set to $version. Commit it, then tag v$version on main and push the tag."
