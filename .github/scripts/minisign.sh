#!/bin/sh
# Minisign signatures made with OpenSSL alone (M36, docs/m36-self-update.md
# §2.2), so the release job needs no extra tool.
#
#   minisign.sh sign KEY.pem TAG FILE...   writes FILE.minisig for each FILE
#   minisign.sh pub KEY.pem                prints the minisign public key
#
# KEY.pem is an Ed25519 private key (`openssl genpkey -algorithm ed25519`).
# Each signature is minisign's prehashed form (Ed25519 over the file's
# BLAKE2b-512) with the trusted comment `ferrule TAG <file name>`, which the
# updater checks, so a signature is bound to one tag and one asset. The key
# id is the first 8 bytes of BLAKE2b-512 of the raw public key.
set -eu

die() { printf 'minisign.sh: %s\n' "$*" >&2; exit 1; }
[ $# -ge 2 ] || die "usage: minisign.sh sign KEY.pem TAG FILE... | pub KEY.pem"
cmd=$1 key=$2
shift 2
[ -f "$key" ] || die "no key file"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
b64() { openssl base64 -A <"$1"; }

openssl pkey -in "$key" -pubout -outform DER | tail -c 32 >"$tmp/pk"
openssl dgst -blake2b512 -binary "$tmp/pk" | head -c 8 >"$tmp/id"

case $cmd in
pub)
    printf 'Ed' >"$tmp/pub"
    cat "$tmp/id" "$tmp/pk" >>"$tmp/pub"
    b64 "$tmp/pub"
    echo
    ;;
sign)
    [ $# -ge 2 ] || die "sign needs a tag and at least one file"
    tag=$1
    shift
    for file in "$@"; do
        comment="ferrule $tag $(basename "$file")"
        openssl dgst -blake2b512 -binary "$file" >"$tmp/hash"
        openssl pkeyutl -sign -rawin -inkey "$key" -in "$tmp/hash" -out "$tmp/sig"
        printf 'ED' >"$tmp/line"
        cat "$tmp/id" "$tmp/sig" >>"$tmp/line"
        printf '%s' "$comment" >"$tmp/comment"
        cat "$tmp/sig" "$tmp/comment" >"$tmp/global"
        openssl pkeyutl -sign -rawin -inkey "$key" -in "$tmp/global" -out "$tmp/gsig"
        {
            echo "untrusted comment: signature from ferrule's release key"
            b64 "$tmp/line"
            echo
            echo "trusted comment: $comment"
            b64 "$tmp/gsig"
            echo
        } >"$file.minisig"
        echo "signed $(basename "$file")"
    done
    ;;
*) die "unknown command $cmd" ;;
esac
