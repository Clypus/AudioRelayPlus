#!/bin/bash
# AudioRelayPlus — Linux (PipeWire/PulseAudio) sanal mikrofon kurulumu.
#
# "AudioRelayPlus Mic" adında bir sanal mikrofon oluşturur ve arp-receiver'ı
# çıkışını ona verecek şekilde başlatır. Discord/oyunlarda mikrofon olarak
# "AudioRelayPlus Mic" seçin. Script kapanınca (Ctrl-C) sanal aygıt kaldırılır.
#
# Tarif (test edildi): null-sink + remap-source + PULSE_SINK + --device pulse
#
# Kullanım: ./linux-virtual-mic.sh [arp-receiver'a geçecek ek argümanlar]
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
  echo "HATA: arp-receiver bulunamadı. Önce derleyin: cd receiver && cargo build --release" >&2
  exit 1
fi

if ! command -v pactl >/dev/null; then
  echo "HATA: pactl yok (pipewire-pulse ya da pulseaudio gerekli)" >&2
  exit 1
fi

MODULES=()
cleanup() {
  for m in "${MODULES[@]:-}"; do
    [ -n "$m" ] && pactl unload-module "$m" 2>/dev/null || true
  done
  echo "sanal mikrofon kaldırıldı"
}
trap cleanup EXIT

if ! pactl list short sinks | awk '{print $2}' | grep -qx "$SINK"; then
  MODULES+=("$(pactl load-module module-null-sink sink_name="$SINK" \
      sink_properties="device.description='ARP-Ara-Cikis'")")
fi
if ! pactl list short sources | awk '{print $2}' | grep -qx "$SOURCE"; then
  MODULES+=("$(pactl load-module module-remap-source master="$SINK.monitor" \
      source_name="$SOURCE" source_properties="device.description='$DESC'")")
fi

echo "sanal mikrofon hazır: \"$DESC\""
echo "→ Discord/oyun ayarlarında giriş aygıtı olarak \"$DESC\" seçin."
echo "→ Alıcı başlatılıyor; telefondan bağlanabilirsiniz. Çıkış: Ctrl-C"
PULSE_SINK="$SINK" "$RECEIVER" --device pulse "$@"
