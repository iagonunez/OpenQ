#!/usr/bin/env bash
# Builds the big demo show for manual testing and opens it in OpenQ.
#
#   tools/demo_show.sh            download media (once), generate the show, open it
#   tools/demo_show.sh --no-run   only download + generate
#   tools/demo_show.sh --fresh    generate again even if the show file exists (keeps the media)
#   tools/demo_show.sh --media    only download the media
#
# Everything lives in $OPENQ_DEMO_DIR (default target/demo-show): media/, demo.openq, CREDITS.txt.
# The media are real recordings under free licences (see CREDITS.txt), downloaded on the first run
# (~130 MB). The show is written by examples/demo_show.rs, so it always matches the current model.
set -euo pipefail

cd "$(dirname "$0")/.."
DIR="${OPENQ_DEMO_DIR:-target/demo-show}"
MEDIA="$DIR/media"
RUN=1
FRESH=0
MEDIA_ONLY=0
for a in "$@"; do
    case "$a" in
        --no-run) RUN=0 ;;
        --fresh) FRESH=1 ;;
        --media) MEDIA_ONLY=1 ;;
        *) echo "unknown option $a" >&2; exit 1 ;;
    esac
done
mkdir -p "$MEDIA"/{music,sfx,video,images,download}

fetch() { # fetch <local path under media/> <url>
    local out="$MEDIA/$1"
    if [[ -s "$out" ]]; then return; fi
    echo "  $1"
    if ! curl -fsSL --retry 3 -o "$out.part" "$2"; then
        echo "    failed: $2" >&2
        rm -f "$out.part"
        return
    fi
    mv "$out.part" "$out"
}

echo "Media (cached in $MEDIA):"
# Music: Kevin MacLeod (incompetech.com), CC BY 4.0.
MUSIC=https://incompetech.com/music/royalty-free/mp3-royaltyfree
for t in "Gymnopedie No 1" "Carefree" "Local Forecast - Elevator" "Wallpaper" "Easy Lemon" "Comfortable Mystery" \
    "Celtic Impulse" "Darkest Child" "Ghost Story" "Long Note Two" "Lightless Dawn" "Dreamy Flashback" \
    "Crossing the Chasm" "Heroic Age" "Anguish" "Dark Times" "Rites" "Sneaky Snitch" "Fluffing a Duck" "Volatile Reaction"; do
    file=$(echo "$t" | tr 'A-Z ' 'a-z_' | tr -d -c 'a-z0-9_-').mp3
    fetch "music/$file" "$MUSIC/$(python3 -I -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1]))' "$t").mp3"
done

# Sound effects: Google sound library (actions.google.com), CC BY 4.0. They come as Ogg Opus, which
# OpenQ's decoder doesn't read: short ones become WAV, long ones FLAC (as a sound designer's library).
SFX=https://actions.google.com/sounds/v1
for s in water/waves_crashing_on_rock_beach water/water_lapping_wind water/large_bubbles_surfacing water/air_woosh_underwater \
    water/small_stream_flowing weather/strong_wind weather/wind weather/desert_howling_wind weather/house_windy_day \
    weather/wind_through_window weather/rain_heavy_loud weather/rain_on_roof weather/light_rain weather/thunder_crack \
    weather/rolling_thunder weather/distant_thunder weather/low_thunder_rolls weather/thunderstorm_long weather/wind_quiet_creaks \
    animals/owl_hooting animals/crow_call animals/june_songbirds animals/afternoon_crickets_long animals/distant_dog_barking \
    doors/creaking_wooden_door doors/knock_on_wooden_door doors/heavy_door_close doors/key_in_lock doors/door_close_and_lock \
    doors/latch_opening_closing foley/distant_footsteps_on_wood \
    household/clock_ticking household/gas_lamp_flickering household/fireplace_tools household/cup_on_saucer \
    household/glosing_old_window ambiences/fire ambiences/outside_night impacts/broken_plates impacts/crash impacts/debris_hits \
    alarms/medium_bell_ringing_near alarms/mechanical_clock_ring alarms/beep_short alarms/phone_alerts_and_rings \
    transportation/air_horn_in_close_hall_series transportation/dock_movement crowds/crowd_talking \
    crowds/murmuring_and_talking_men crowds/team_cheer crowds/voices_murmur emergency/emergency_flare_burns \
    emergency/fuse_ignite_and_burn emergency/emergency_radio_alert cartoon/tympani_bing cartoon/drum_roll \
    cartoon/wind_chimes cartoon/rainstick_slow; do
    name=$(basename "$s")
    ls "$MEDIA/sfx/$name".* >/dev/null 2>&1 && continue
    fetch "download/$name.ogg" "$SFX/$s.ogg"
    src="$MEDIA/download/$name.ogg"
    [[ -s "$src" ]] || continue
    d=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$src")
    if awk "BEGIN { exit !($d < 20) }"; then ext=wav; else ext=flac; fi
    ffmpeg -loglevel error -y -i "$src" -ar 48000 "$MEDIA/sfx/$name.$ext"
done

# Video: Blender Foundation open films (CC BY 3.0); short clips via test-videos.co.uk.
fetch video/sintel_trailer.mp4 https://download.blender.org/durian/trailer/sintel_trailer-720p.mp4
fetch video/big_buck_bunny_trailer.mov https://download.blender.org/peach/trailer/trailer_480p.mov
fetch video/jellyfish_10s.mp4 https://test-videos.co.uk/vids/jellyfish/mp4/h264/720/Jellyfish_720_10s_5MB.mp4
fetch video/big_buck_bunny_10s.mp4 https://test-videos.co.uk/vids/bigbuckbunny/mp4/h264/720/Big_Buck_Bunny_720_10s_5MB.mp4
fetch video/sintel_10s.mp4 https://test-videos.co.uk/vids/sintel/mp4/h264/720/Sintel_720_10s_5MB.mp4

# Stills taken from the films (JPEG and PNG), so picture cues have real images too.
still() { # still <name> <video> <seconds>
    [[ -s "$MEDIA/images/$1" ]] || ffmpeg -loglevel error -y -ss "$3" -i "$MEDIA/video/$2" -frames:v 1 "$MEDIA/images/$1"
}
still sintel_snow.jpg sintel_trailer.mp4 6
still sintel_dragon.jpg sintel_trailer.mp4 33
still sintel_city.jpg sintel_trailer.mp4 20
still sintel_title.png sintel_trailer.mp4 47
still bunny_meadow.png big_buck_bunny_trailer.mov 4
still jellyfish.jpg jellyfish_10s.mp4 3

# The show's logo (SVG, drawn here).
cat > "$MEDIA/images/lighthouse_logo.svg" <<'EOF'
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 800 450">
  <defs><radialGradient id="beam" cx="0.5" cy="0.5" r="0.5"><stop offset="0" stop-color="#fff6c0" stop-opacity="0.9"/><stop offset="1" stop-color="#fff6c0" stop-opacity="0"/></radialGradient></defs>
  <polygon points="400,120 800,40 800,200" fill="url(#beam)"/>
  <polygon points="400,120 0,40 0,200" fill="url(#beam)" opacity="0.5"/>
  <polygon points="370,140 430,140 450,400 350,400" fill="#e9e4da"/>
  <rect x="362" y="190" width="76" height="28" fill="#b3261e"/><rect x="356" y="280" width="88" height="28" fill="#b3261e"/>
  <rect x="372" y="96" width="56" height="44" rx="6" fill="#ffd54a"/>
  <polygon points="364,96 436,96 400,62" fill="#3a3a3a"/>
  <path d="M0 400 Q100 380 200 400 T400 400 T600 400 T800 400 V450 H0 Z" fill="#1c4f7a"/>
  <text x="400" y="440" font-family="sans-serif" font-size="34" font-weight="700" fill="#ffffff" text-anchor="middle" letter-spacing="6">THE LIGHTHOUSE KEEPER</text>
</svg>
EOF

cat > "$DIR/CREDITS.txt" <<'EOF'
Demo show media (downloaded by tools/demo_show.sh; not part of OpenQ):
- Music: Kevin MacLeod (incompetech.com), Creative Commons Attribution 4.0.
- Sound effects: Google sound library (developers.google.com/assistant/tools/sound-library), CC BY 4.0.
- Video and stills: Blender Foundation (Sintel, Big Buck Bunny; www.blender.org), CC BY 3.0;
  10 s clips via test-videos.co.uk. Jellyfish clip: test-videos.co.uk.
EOF

# Durations (seconds) for the generator: trims, slices and loops are placed inside the real files.
DUR="$DIR/durations.tsv"
: > "$DUR.part"
for f in "$MEDIA"/{music,sfx,video}/*; do
    d=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$f" || true)
    if [[ -z "$d" || "$d" == "N/A" ]]; then echo "  unreadable, skipped: $f" >&2; rm -f "$f"; continue; fi
    echo -e "${f#"$MEDIA"/}\t$d" >> "$DUR.part"
done
mv "$DUR.part" "$DUR"
echo "$(wc -l < "$DUR") media files ready."
[[ $MEDIA_ONLY == 1 ]] && exit 0

cargo build --release --bin openq --example demo_show
if [[ $FRESH == 1 || ! -f "$DIR/demo.openq" ]]; then
    ./target/release/examples/demo_show "$DIR"
fi
if [[ $RUN == 1 ]]; then
    exec ./target/release/openq "$DIR/demo.openq"
fi
