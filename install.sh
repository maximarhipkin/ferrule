#!/bin/sh
# Install ferrule on Linux or macOS, then set it up.
#
#   curl -fsSL https://raw.githubusercontent.com/maximarhipkin/ferrule/main/install.sh | sh
#
# It downloads the release binary for this machine, checks its sha256, puts
# it in ~/.local/bin and starts `ferrule setup` — the wizard that asks for
# the provider and key, Telegram, the sandbox and the background service.
# Running it again upgrades in place and keeps your settings.
#
# Optional environment:
#   FERRULE_VERSION      a release tag such as v0.3.0 (default: the latest)
#   FERRULE_INSTALL_DIR  where the binary goes (default: ~/.local/bin; as root
#                        on Linux /usr/local/bin, where the system service,
#                        which can't see /root, runs it from)
#   FERRULE_NO_SETUP=1   install only, don't start the wizard
#   GITHUB_TOKEN         optional: download through the API (a private fork)
#
# On Windows, use install.ps1 instead.

set -eu

REPO=maximarhipkin/ferrule
VERSION=${FERRULE_VERSION:-latest}
if [ "$(id -u)" = 0 ] && [ "$(uname -s)" = Linux ]; then
    DIR=${FERRULE_INSTALL_DIR:-/usr/local/bin}
else
    DIR=${FERRULE_INSTALL_DIR:-$HOME/.local/bin}
fi

say() { printf '%s\n' "$*"; }
die() { printf 'ferrule install: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

target() {
    arch=$(uname -m)
    case $arch in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) die "no prebuilt binary for $arch; build from source: https://github.com/$REPO#install" ;;
    esac
    case $(uname -s) in
        Linux) echo "$arch-unknown-linux-musl" ;;
        Darwin)
            # A shell running under Rosetta reports x86_64 on Apple silicon.
            if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
                arch=aarch64
            fi
            echo "$arch-apple-darwin"
            ;;
        MINGW* | MSYS* | CYGWIN*) die "this is Windows: in PowerShell, run  irm https://raw.githubusercontent.com/$REPO/main/install.ps1 | iex" ;;
        *) die "no prebuilt binary for $(uname -s); build from source: https://github.com/$REPO#install" ;;
    esac
}

# fetch URL FILE: public downloads.
fetch() {
    if have curl; then
        curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"
    elif have wget; then
        wget -q --https-only -O "$2" "$1"
    else
        die "needs curl or wget"
    fi
}

# fetch_private ASSET FILE: a release asset through the API, for a private
# repository. curl drops the token on the redirect to the storage host,
# which is what that host wants; wget would resend it, so curl only.
fetch_private() {
    have curl || die "GITHUB_TOKEN downloads need curl"
    if [ "$VERSION" = latest ]; then
        release=latest
    else
        release=tags/$VERSION
    fi
    # Each asset object starts with its API url, then id and name; split the
    # JSON at "{" so an asset's url and name share a line.
    url=$(curl -fsSL -H "Authorization: Bearer $GITHUB_TOKEN" \
        "https://api.github.com/repos/$REPO/releases/$release" |
        tr -d ' \n\r\t' | tr '{' '\n' | grep "\"name\":\"$1\"" |
        sed 's/.*"url":"\([^"]*\)".*/\1/') ||
        die "no release asset $1 (is GITHUB_TOKEN allowed to read $REPO?)"
    [ -n "$url" ] || die "no release asset $1"
    curl -fsSL -H "Authorization: Bearer $GITHUB_TOKEN" \
        -H 'Accept: application/octet-stream' -o "$2" "$url"
}

download() {
    if [ -n "${GITHUB_TOKEN:-}" ]; then
        fetch_private "$1" "$2"
    elif [ "$VERSION" = latest ]; then
        fetch "https://github.com/$REPO/releases/latest/download/$1" "$2"
    else
        fetch "https://github.com/$REPO/releases/download/$VERSION/$1" "$2"
    fi
}

sha256() {
    if have sha256sum; then
        sha256sum "$1" | cut -d' ' -f1
    elif have shasum; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        die "needs sha256sum or shasum to check the download"
    fi
}

TARGET=$(target)
ASSET=ferrule-$TARGET.tar.gz
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM

say "Downloading ferrule ($VERSION, $TARGET)…"
download "$ASSET" "$TMP/$ASSET" || die "download failed: $ASSET ($VERSION)"
download "$ASSET.sha256" "$TMP/$ASSET.sha256" || die "download failed: $ASSET.sha256"
want=$(cut -d' ' -f1 <"$TMP/$ASSET.sha256")
got=$(sha256 "$TMP/$ASSET")
[ "$want" = "$got" ] || die "checksum mismatch for $ASSET (expected $want, got $got)"

tar -xzf "$TMP/$ASSET" -C "$TMP" ferrule
mkdir -p "$DIR"
# Copy beside the old binary, then rename over it: a running gateway keeps
# its old file and never sees a half-written one.
cp "$TMP/ferrule" "$DIR/.ferrule.new"
chmod 755 "$DIR/.ferrule.new"
mv -f "$DIR/.ferrule.new" "$DIR/ferrule"
BIN=$DIR/ferrule
say "Installed $("$BIN" --version) → $BIN"

case ":$PATH:" in
    *":$DIR:"*)
        other=$(command -v ferrule || true)
        if [ -n "$other" ] && [ "$other" != "$BIN" ]; then
            say "Note: \`ferrule\` in this shell is $other, which comes first on PATH."
        fi
        ;;
    *)
        case ${SHELL:-} in
            */zsh) rc="~/.zshrc" ;;
            */bash) rc="~/.bashrc" ;;
            *) rc="your shell's profile" ;;
        esac
        say "Add $DIR to PATH so \`ferrule\` works in new shells — put this in $rc:"
        say "  export PATH=\"$DIR:\$PATH\""
        ;;
esac

# An upgrade: the gateway service keeps the old binary until it restarts.
if [ "$(uname -s)" = Darwin ]; then
    unit=$HOME/Library/LaunchAgents/ai.ferrule.gateway.plist
    if [ -f "$unit" ] && launchctl print "gui/$(id -u)/ai.ferrule.gateway" >/dev/null 2>&1; then
        launchctl kickstart -k "gui/$(id -u)/ai.ferrule.gateway" && say "Restarted the gateway service."
    fi
elif [ "$(id -u)" = 0 ] && [ -f /etc/systemd/system/ferrule.service ]; then
    unit=/etc/systemd/system/ferrule.service
    if systemctl is-active --quiet ferrule.service 2>/dev/null; then
        systemctl restart ferrule.service && say "Restarted the gateway service."
    fi
else
    unit=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/ferrule.service
    if [ -f "$unit" ] && systemctl --user is-active --quiet ferrule.service 2>/dev/null; then
        systemctl --user restart ferrule.service && say "Restarted the gateway service."
    fi
fi
if [ -f "$unit" ] && ! grep -q "$BIN" "$unit"; then
    say "Note: the gateway service runs a ferrule from elsewhere; \`ferrule setup\` → service points it at this one."
fi

if "$BIN" config path 2>/dev/null | grep -q '^config *none yet'; then
    if [ "${FERRULE_NO_SETUP:-}" = 1 ]; then
        say "Next: \`$BIN setup\` walks you through the settings."
    elif (: </dev/tty) 2>/dev/null; then
        say ""
        "$BIN" setup </dev/tty
    else
        say "No terminal to ask on. Next, in a terminal: \`$BIN setup\`."
    fi
else
    say "Your settings are kept. \`ferrule setup\` changes them, \`ferrule doctor\` checks them."
fi
