# LoKAI build recipes

version   := `sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1`
bin       := "target/release/lokai"
# Newest glibc the packages may require: Debian 13 has 2.41, Ubuntu 24.04 and Fedora 40 have 2.39.
max_glibc := "2.39"

# Keep local paths out of the binary: source paths in panic messages become ~/.cargo/... and ./...
export RUSTFLAGS := "--remap-path-prefix=" + env_var("HOME") + "=~ --remap-path-prefix=" + justfile_directory() + "=."

# List the available recipes.
default:
    @just --list

# Build the release binary into target/release/lokai (thin LTO, stripped).
build:
    cargo build --release --locked

# Build and run, e.g. `just run --settings`.
run *args: build
    ./{{bin}} {{args}}

# Run the unit tests and clippy.
test:
    cargo test
    cargo clippy --release --all-targets -- -D warnings

# Agent runs against a real server, e.g. `just e2e http://127.0.0.1:8080`.
e2e server:
    LOKAI_TEST_SERVER={{server}} cargo test -- --ignored --nocapture agent_

# Render the launcher into PNGs for a visual check.
snapshots dir="target/snapshots":
    LOKAI_SNAPSHOT_DIR={{dir}} cargo test -- --ignored render_snapshots

# Render the README screenshots into assets/ (screenshot.png, screenshot-plot.png).
screenshot:
    cargo test -- --ignored readme_screenshot

# Fail if the binary needs a newer glibc than max_glibc (the packages must run on Debian 13).
check-glibc: build
    #!/usr/bin/env bash
    set -euo pipefail
    need=$(objdump -T {{bin}} | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -n1)
    if [ "$(printf '%s\n%s\n' "$need" "{{max_glibc}}" | sort -V | tail -n1)" != "{{max_glibc}}" ]; then
        echo "error: {{bin}} needs glibc $need, newer than {{max_glibc}}. Symbols:" >&2
        objdump -T {{bin}} | grep -F "GLIBC_$need" >&2
        exit 1
    fi
    echo "glibc requirement: $need (limit {{max_glibc}})"

# Stage the app, desktop entry and icon into DESTDIR, e.g. `just stage build/root`.
stage destdir: build
    #!/usr/bin/env bash
    set -euo pipefail
    d="{{destdir}}"
    install -Dm755 {{bin}}                 "$d/usr/bin/lokai"
    install -Dm644 packaging/lokai.desktop  "$d/usr/share/applications/lokai.desktop"
    install -Dm644 assets/lokai.svg         "$d/usr/share/icons/hicolor/scalable/apps/lokai.svg"
    install -Dm644 README.md               "$d/usr/share/doc/lokai/README.md"
    install -Dm644 LICENSE                 "$d/usr/share/licenses/lokai/LICENSE"
    install -Dm644 assets/lucide/LICENSE   "$d/usr/share/licenses/lokai/LICENSE.lucide"
    install -Dm644 packaging/licenses/NOTICE-latex-rust.txt "$d/usr/share/licenses/lokai/NOTICE.latex-rust"
    install -Dm644 packaging/licenses/OFL-STIX-Two-Math.txt "$d/usr/share/licenses/lokai/OFL.stix-two-math"
    echo "staged lokai into $d"

# Build distro packages (deb, rpm, archlinux) into dist/ with nfpm.
package: check-glibc
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p dist
    export VERSION="{{version}}"
    export GLIBC=$(objdump -T {{bin}} | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -n1)
    for pkg in deb rpm archlinux; do
        nfpm package --packager "$pkg" --config nfpm.yaml --target dist/
    done
    echo "packages written to dist/"
    ls -1 dist/

# Download the Lucide icons again from lucide.dev.
icons:
    ./assets/lucide/fetch.sh

# Format all sources in place.
format:
    cargo fmt

# Remove build artifacts.
clean:
    cargo clean
    rm -rf dist
