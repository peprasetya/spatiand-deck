#!/usr/bin/env bash
# Fetch 360° panoramas to use as Spatiand environments.
#
# Two sources, for different reasons:
#
#   NOIRLab — night skies over real observatories, which is the single most convincing thing
#   to be standing in when the display is a pair of see-through glasses with a limited field.
#   Their public images are CC BY 4.0; the credit line for each is written alongside it.
#
#   Poly Haven — CC0 studio and outdoor HDRIs. No attribution required, and useful as a neutral
#   background to read against when the sky is too busy.
#
# The `publicationjpg` size is what gets downloaded, not the original. The originals here run to
# 90–210 MB, which is minutes of download and several seconds of JPEG decode at startup for an
# image that is then sampled at roughly six pixels per degree. 4000x2000 is already more than
# the optics resolve.
#
# Files land in ~/.local/share/spatiand/environments and appear in the HUD's Environment list
# on the next run.

set -uo pipefail

DEST="${SPATIAND_ENVIRONMENTS:-$HOME/.local/share/spatiand/environments}"
mkdir -p "$DEST"

# --- NOIRLab, from the 360pano category ---
#
# id|name|credit. Ids are stable; the category page they came from is
# https://noirlab.edu/public/images/archive/category/360pano/
NOIRLAB=(
  "iotw2413a|Gemini North Panorama|KPNO/NOIRLab/NSF/AURA"
  "iotw2438b|Kitt Peak Night Sky|KPNO/NOIRLab/NSF/AURA"
  "iotw2442b|Cerro Tololo Panorama|CTIO/NOIRLab/NSF/AURA"
  "iotw2446b|Observatory Twilight|NOIRLab/NSF/AURA"
  "noirlab2430a|Rubin Observatory|RubinObs/NOIRLab/NSF/AURA"
)

echo "== NOIRLab 360 panoramas =="
for entry in "${NOIRLAB[@]}"; do
    IFS='|' read -r id name credit <<< "$entry"
    out="$DEST/${id}_${name// /-}.jpg"
    if [ -s "$out" ]; then
        echo "  have $name"
        continue
    fi
    url="https://storage.noirlab.edu/media/archives/images/publicationjpg/${id}.jpg"
    echo "  fetching $name"
    if curl -fL --max-time 180 -o "$out.part" "$url"; then
        mv "$out.part" "$out"
        # Credit alongside the image rather than in this script only, so it survives the file
        # being copied somewhere else.
        printf '%s\nCredit: %s\nSource: https://noirlab.edu/public/images/%s/\nLicence: CC BY 4.0\n' \
            "$name" "$credit" "$id" > "$DEST/${id}_${name// /-}.txt"
    else
        rm -f "$out.part"
        echo "    failed"
    fi
done

# Poly Haven's HDRIs were fetched here too, until their download URLs moved and every
# request came back 404. Not worth chasing: the generated "Studio" environment already covers
# the neutral-background case, and it needs no network at all.

echo
echo "Installed in $DEST:"
ls -1sh "$DEST"/*.jpg 2>/dev/null || echo "  (nothing)"
echo
echo "All of these are mono 360. Spatiand reads the layout from the filename, so a stereo"
echo "panorama named *_ou.jpg or *_sbs.jpg will be treated as one; see crates/spatiand/src/"
echo "environment.rs for the full table."
