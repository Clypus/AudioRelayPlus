#!/bin/bash
# AudioRelayPlus — Linux (PipeWire/PulseAudio) virtual microphone setup.
#
# Creates a virtual microphone called "AudioRelayPlus Mic" and starts
# arp-receiver with its output going into it. Pick "AudioRelayPlus Mic" as
# the microphone in Discord/games. The virtual device is removed when the
# script exits (Ctrl-C). (arp-gui does the same with one checkbox.)
#
# Recipe (tested): null-sink + remap-source + PULSE_SINK + --device pulse
#
# Usage: ./linux-virtual-mic.sh [extra arguments for arp-receiver]
set -euo pipefail

SINK="arp_sink"
SOURCE="arp_mic"
DESC="AudioRelayPlus Mic"
DIR="$(cd "$(dirname "$0")" && pwd)"

find_receiver() {
  for c in "$DIR/arp-receiver" "$(command -v arp-receiver 2>/dev/null || true)" \
           "$DIR/../receiver/target/release/arp-receiver" \
           "$DIR/../receiver/target/debug/arp-receiver"; do
    if [ -n "$c" ] && [ -x "$c" ]; then echo "$c"; return; fi
  done
  echo ""
}

RECEIVER="$(find_receiver)"
if [ -z "$RECEIVER" ]; then
  echo "ERROR: arp-receiver not found. Build it first: cd receiver && cargo build --release" >&2
  exit 1
fi

if ! command -v pactl >/dev/null; then
  echo "ERROR: pactl is missing (pipewire-pulse or pulseaudio is required)" >&2
  exit 1
fi

MODULES=()
cleanup() {
  for m in "${MODULES[@]:-}"; do
    [ -n "$m" ] && pactl unload-module "$m" 2>/dev/null || true
  done
  echo "virtual microphone removed"
}
trap cleanup EXIT

if ! pactl list short sinks | awk '{print $2}' | grep -qx "$SINK"; then
  MODULES+=("$(pactl load-module module-null-sink sink_name="$SINK" \
      sink_properties="device.description='ARP-Internal-Output'")")
fi
if ! pactl list short sources | awk '{print $2}' | grep -qx "$SOURCE"; then
  MODULES+=("$(pactl load-module module-remap-source master="$SINK.monitor" \
      source_name="$SOURCE" source_properties="device.description='$DESC'")")
fi

echo "virtual microphone ready: \"$DESC\""
echo "→ pick \"$DESC\" as the input device in Discord/games."
echo "→ starting the receiver; connect from the phone (Wi-Fi, or USB with adb). Quit: Ctrl-C"
PULSE_SINK="$SINK" "$RECEIVER" --device pulse --auto-usb "$@"
