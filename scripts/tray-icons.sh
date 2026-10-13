#!/bin/bash
# Renders app/data/tray/N.png from the colour app icon. The icon's own
# artwork spans x 10 to 118 and y 10.5 to 118.5 of its 128 unit canvas
# once the view is cut to a square around it, so the penguin fills the
# picture and the host does not draw it small. Needs rsvg-convert.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
svg="$root/app/data/icons/scalable/apps/io.github.c9dev.PenguinMail.svg"
cut=$(mktemp --suffix=.svg)
trap 'rm -f "$cut"' EXIT
sed 's/width="128" height="128" viewBox="0 0 128 128"/viewBox="10 10.5 108 108"/' "$svg" > "$cut"
for size in 16 22 24 32 48 64; do
    rsvg-convert -w "$size" -h "$size" "$cut" -o "$root/app/data/tray/$size.png"
done
