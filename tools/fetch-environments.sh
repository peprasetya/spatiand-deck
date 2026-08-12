#!/usr/bin/env bash
# Fetch a few CC0 360-degree panoramas to use as Spatiand environments.
#
# Not run automatically and not vendored into the repository. Spatiand generates its own
# environment (`Sky::studio`) and works with none of these installed; this is here so that
# getting a photographic one is a single deliberate command rather than a hunt.
#
# Poly Haven publishes everything under CC0, which is the one licence with no attribution or
# redistribution question attached. https://polyhaven.com/hdris
#
# Drop your own equirectangular JPEG or PNG in the same folder and it joins the rotation.
# Layout is inferred from the file name and shape -- see crates/spatiand/src/environment.rs:
#   name contains _ou / _tb  -> over-under stereo      name contains 180 -> front hemisphere
#   name contains _sbs       -> side-by-side stereo    2:1 aspect        -> mono 360
set -euo pipefail

DEST="${SPATIAND_ENVIRONMENTS:-$HOME/.local/share/spatiand/environments}"
RES="${RES:-4k}"

# A spread rather than a set: an interior, an outdoor evening, and a neutral studio, so the
# glass bubbles have something different to refract in each.
SLUGS=(
    "kloofendal_48d_partly_cloudy_puresky"
    "studio_small_09"
    "moonless_golf"
    "phalzer_forest_01"
)

mkdir -p "$DEST"
echo "fetching into $DEST"

for slug in "${SLUGS[@]}"; do
    out="$DEST/$slug.jpg"
    if [ -s "$out" ]; then
        echo "  have    $slug"
        continue
    fi
    url="https://dl.polyhaven.org/file/ph-assets/HDRIs/jpg/${RES}/${slug}.jpg"
    echo "  getting $slug"
    if ! curl -fL --retry 2 --connect-timeout 20 -o "$out.part" "$url"; then
        echo "  FAILED  $slug (skipping)" >&2
        rm -f "$out.part"
        continue
    fi
    mv "$out.part" "$out"
done

echo
echo "done. In Spatiand: STEAM -> Environment cycles through them."
ls -la "$DEST"
