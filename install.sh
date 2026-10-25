#!/bin/bash
# Build a release binary and copy it to ~/Applications/AudioRouter/.
# No desktop entry is created.
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release
dir="$HOME/Applications/AudioRouter"
mkdir -p "$dir"
# Copy then rename, so a running instance (e.g. started at login) does not block the update.
cp ./target/release/audiorouter "$dir/audiorouter.new"
mv -f "$dir/audiorouter.new" "$dir/audiorouter"
cp ./data/io.github.audiorouter.AudioRouter.svg "$dir/audiorouter.svg"
echo "Installed to $dir/audiorouter (a running instance keeps the old version until it is restarted)."
echo "App icon: $dir/audiorouter.svg"
