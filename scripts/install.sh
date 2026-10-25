#!/bin/sh
# Build Audio Router and install it for the current user (~/.local).
set -eu
cd "$(dirname "$0")/.."
cargo build --release
install -Dm755 target/release/audiorouter "$HOME/.local/bin/audiorouter"
install -Dm644 data/io.github.audiorouter.AudioRouter.desktop "$HOME/.local/share/applications/io.github.audiorouter.AudioRouter.desktop"
echo "Installed to ~/.local/bin/audiorouter."
echo "Enable 'Start at Login' from the app menu to apply your setup automatically."
