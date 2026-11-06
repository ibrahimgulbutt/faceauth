#!/usr/bin/env bash
# Download FaceAuth's face models from InsightFace's official releases and
# verify every file against a pinned SHA-256. Nothing is redistributed by
# FaceAuth: the models come straight from InsightFace, under InsightFace's
# terms ("non-commercial research purposes only").
#
#   dist/fetch-models.sh DEST_DIR [accurate|fast] [--installed DIR]
#
#   accurate  ScrFD-500MF detector + ResNet50 recognizer (buffalo_sc + buffalo_l, ~300 MB download)
#   fast      ScrFD-500MF detector + MobileFaceNet recognizer (buffalo_sc only, ~15 MB download)
#
# Files already present and valid in DEST_DIR (or in --installed DIR) are not
# downloaded again. Used by install.sh and for development (DEST_DIR=models).

set -euo pipefail

BASE_URL="https://github.com/deepinsight/insightface/releases/download/v0.7"

# pack        zip SHA-256
declare -A PACK_SHA256=(
    [buffalo_sc]="57d31b56b6ffa911c8a73cfc1707c73cab76efe7f13b675a05223bf42de47c72"
    [buffalo_l]="80ffe37d8a5940d59a7384c201a2a38d4741f2f3c51eef46ebb28218a7b0ca2f"
)
# installed name → "pack member sha256"
declare -A MODEL_SOURCE=(
    [det_500m.onnx]="buffalo_sc det_500m.onnx 5e4447f50245bbd7966bd6c0fa52938c61474a04ec7def48753668a9d8b4ea3a"
    [arcface.onnx]="buffalo_sc w600k_mbf.onnx 9cc6e4a75f0e2bf0b1aed94578f144d15175f357bdc05e815e5c4a02b319eb4f"
    [arcface_r50.onnx]="buffalo_l w600k_r50.onnx 4c06341c33c2ca1f86781dab0e829f88ad5b64be9fba56e56bc9ebdefc619e43"
)

die() { printf 'fetch-models: %s\n' "$*" >&2; exit 1; }

DEST="${1:-}"
MODEL="${2:-accurate}"
INSTALLED=""
[ -n "$DEST" ] || die "usage: fetch-models.sh DEST_DIR [accurate|fast] [--installed DIR]"
if [ "${3:-}" = "--installed" ]; then
    INSTALLED="${4:-}"
    [ -n "$INSTALLED" ] || die "--installed needs a directory"
fi

case "$MODEL" in
    accurate) WANTED=(det_500m.onnx arcface_r50.onnx) ;;
    fast) WANTED=(det_500m.onnx arcface.onnx) ;;
    *) die "unknown model '$MODEL' (use accurate or fast)" ;;
esac

valid() { [ -s "$1" ] && echo "$2  $1" | sha256sum -c --status; }

extract() { # extract ZIP MEMBER OUT
    if command -v unzip >/dev/null 2>&1; then
        unzip -p "$1" "$2" >"$3"
    elif command -v python3 >/dev/null 2>&1; then
        python3 -I -c 'import sys, zipfile, shutil
with zipfile.ZipFile(sys.argv[1]) as z, z.open(sys.argv[2]) as src, open(sys.argv[3], "wb") as dst:
    shutil.copyfileobj(src, dst)' "$1" "$2" "$3"
    elif command -v bsdtar >/dev/null 2>&1; then
        bsdtar -xOf "$1" "$2" >"$3"
    else
        die "need unzip, python3 or bsdtar to unpack the model archive"
    fi
}

mkdir -p "$DEST"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

for name in "${WANTED[@]}"; do
    read -r pack member sha <<<"${MODEL_SOURCE[$name]}"
    if valid "$DEST/$name" "$sha" || { [ -n "$INSTALLED" ] && valid "$INSTALLED/$name" "$sha"; }; then
        echo "  $name: already present (checksum OK)"
        continue
    fi
    zip="$WORK/$pack.zip"
    if [ ! -f "$zip" ]; then
        echo "  Downloading InsightFace $pack..."
        curl -fL --proto '=https' --retry 3 --progress-bar -o "$zip" "$BASE_URL/$pack.zip" \
            || die "download failed: $BASE_URL/$pack.zip"
        echo "${PACK_SHA256[$pack]}  $zip" | sha256sum -c --status \
            || die "checksum mismatch for $pack.zip; refusing to use it"
    fi
    extract "$zip" "$member" "$WORK/$name"
    valid "$WORK/$name" "$sha" || die "checksum mismatch for $member in $pack.zip"
    install -m 644 "$WORK/$name" "$DEST/$name"
    echo "  $name: installed (from $pack/$member)"
done
