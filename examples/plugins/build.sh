#!/bin/sh
# Rebuild the example plugins and pin each manifest's sha256 to its new
# .wasm. Needs the wasm32-unknown-unknown target:
#   rustup target add wasm32-unknown-unknown
# The .wasm files are committed, so tests and CI never run this.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
target=${CARGO_TARGET_DIR:-$here/target}
root=$(cd "$here/../.." && pwd)
cargo_home=${CARGO_HOME:-$HOME/.cargo}
# No builder's paths in the committed modules (panic locations name source
# files): the same sources build to the same bytes anywhere.
export RUSTFLAGS="--remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$root=/ferrule"
for dir in unit-convert github-repo; do
    crate=$(echo "$dir" | tr - _)
    cargo build --quiet --release --locked --target wasm32-unknown-unknown \
        --manifest-path "$here/$dir/Cargo.toml" --target-dir "$target"
    cp "$target/wasm32-unknown-unknown/release/$crate.wasm" "$here/$dir/$crate.wasm"
    if command -v sha256sum >/dev/null; then
        sum=$(sha256sum "$here/$dir/$crate.wasm" | cut -d' ' -f1)
    else
        sum=$(shasum -a 256 "$here/$dir/$crate.wasm" | cut -d' ' -f1)
    fi
    sed -i.bak "s/\"sha256\": \"[0-9a-f]*\"/\"sha256\": \"$sum\"/" "$here/$dir/plugin.json"
    rm -f "$here/$dir/plugin.json.bak"
    echo "$dir: $(wc -c < "$here/$dir/$crate.wasm" | tr -d ' ') bytes, sha256 $sum"
done
