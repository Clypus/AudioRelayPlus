# AudioRelayPlus 🎙️

[![build](https://github.com/Clypus/AudioRelayPlus/actions/workflows/build.yml/badge.svg)](https://github.com/Clypus/AudioRelayPlus/actions/workflows/build.yml)
[![release](https://img.shields.io/github/v/release/Clypus/AudioRelayPlus)](https://github.com/Clypus/AudioRelayPlus/releases/latest)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Use your Android phone as a **glitch-free, low-latency microphone for your PC**
(Windows and Linux), over Wi-Fi or a USB cable. One tap to connect, no audio
dropouts, no silent disconnects.

It was built as a dependable replacement for AudioRelay after its audio kept
going choppy and cutting out for no reason. Every part of the pipeline is
designed around that.

| Problem | How AudioRelayPlus handles it |
|---|---|
| Choppy audio | Adaptive jitter buffer that grows instead of dropping late packets, Opus in-band FEC (a lost packet's backup rides in the next one) and PLC |
| Audio stops when the screen turns off | `microphone` foreground service, wake lock, low-latency Wi-Fi lock, battery-optimization exemption |
| Silent disconnects | 500 ms heartbeat; after 3 s without an answer it reconnects **without asking**, and finds the PC again if its IP changed |
| Latency creeping up, crackles over time | A clock-drift servo absorbs the phone/PC clock difference with ±0.5% micro-resampling; after an outage it jumps to the live edge |
| Sound card hiccups | The PC output reopens itself after device errors (VB-Cable resets, unplugged headsets, sleep/resume) |

Measured with the built-in network simulator (90 s, see [Testing](#testing)):

| Simulated network | v0.3 | v0.4 |
|---|---|---|
| 0% loss, 60 ms jitter | 4.2% of audio synthesized, 191 packets dropped | 0.07%, none dropped |
| 150 ms Wi-Fi stall every 4 s | 61 frames concealed | 10 |
| USB / clean network | 30 ms buffer | 30 ms buffer |

## Features

- **Wi-Fi or USB.** Plug the phone in and it just shows up as "USB" in the
  app: the PC sets up the `adb reverse` tunnel by itself. Lossless, 30 ms buffer.
- **Automatic everything**: LAN discovery, auto-connect to the last PC, silent
  reconnects, re-discovery when the PC's address changes.
- **AI noise suppression** (RNNoise) on the PC: fans, keyboards and street noise
  are gone, without the cut-outs of a noise gate.
- **Soundpad**: drop mp3/ogg/wav/flac files on the window; play them from the
  PC or remotely from the phone, mixed into your mic. Optional "hear it myself".
- **Level meter, mute** on the PC and a Mute button in the phone's notification.
- **Echo cancellation** (experimental) for when your speakers leak into the phone.
- **Virtual microphone**: one checkbox on Linux; VB-Cable on Windows (picked
  automatically).
- **Windows niceties**: one-click firewall fix, start with Windows (minimized).
- English and Turkish UI, chosen from the system language.

## Quick start

Grab the files from the [latest release](https://github.com/Clypus/AudioRelayPlus/releases/latest).

### Windows

1. Install [VB-Cable](https://vb-audio.com/Cable/) (free, once).
2. Unzip `AudioRelayPlus-Windows-*.zip` and run `arp-gui.exe`. It selects
   "CABLE Input" by itself; click **Allow (admin)** next to *Firewall* if it
   says there is no rule.
3. In Discord / your game, choose **CABLE Output** as the microphone.

Step-by-step guide with troubleshooting: [docs/INSTALL-WINDOWS.txt](docs/INSTALL-WINDOWS.txt)
(Türkçe: [docs/KURULUM-WINDOWS.txt](docs/KURULUM-WINDOWS.txt)).

### Linux

Unpack `AudioRelayPlus-Linux-*.tar.gz` and run `./arp-gui`. With
*Virtual microphone* ticked (the default), a microphone called
**"AudioRelayPlus Mic"** appears — pick it in Discord or your game.
Allow UDP port 48222 through your firewall (e.g.
`sudo firewall-cmd --add-port=48222/udp --permanent && sudo firewall-cmd --reload`).

Headless alternative: `scripts/linux-virtual-mic.sh` (creates the same
virtual mic and runs the command-line receiver).

### Android (8.0+)

Install `AudioRelayPlus-Android-*.apk`, open it and tap your PC in the list.
On first use it asks for the microphone permission and to ignore battery
optimizations — allow both for uninterrupted audio. Next time it connects by
itself.

> On Xiaomi / Huawei / Samsung also set *Battery → No restrictions* for the
> app. Vendor-specific tips: <https://dontkillmyapp.com>

> **Upgrading from v0.3?** v0.4 is signed with a new release key: uninstall
> the old app once, then install. Future updates install on top.

### USB cable mode

1. Enable *USB debugging* on the phone (Settings → About phone → tap
   *Build number* 7 times → Developer options → USB debugging).
2. The PC needs `adb`: put Google's
   [platform-tools](https://developer.android.com/tools/releases/platform-tools)
   folder next to `arp-gui` (or have it on PATH / in the Android SDK).
3. Plug in the cable and accept the prompt on the phone. The PC window says
   *ready*, and the app lists your PC as **USB** — tap it.

## How it works

```
 Phone                                   PC (arp-gui / arp-receiver)
 ─────                                   ───────────────────────────
 mic → gain/limiter → Opus 20 ms ─UDP──▶ adaptive jitter buffer (FEC/PLC, elastic)
       + heartbeat, auto-reconnect  or   → echo cancel → RNNoise → mute → soundpad
                                    TCP  → drift servo resampler → gain → sound device
                                   (USB)   (VB-Cable / virtual mic / speakers)
```

```
receiver/   Rust PC receiver (Windows + Linux): arp-gui, arp-receiver (CLI), arp-tools
android/    Kotlin phone app (Android 8.0+)
scripts/    Linux virtual mic helper, Android toolchain setup
docs/       Windows install guides (English, Turkish)
PROTOCOL.md Wire protocol (v1.1)
```

## Building from source

**Receiver** (Rust stable; on Linux: ALSA, cmake and libclang dev packages):

```bash
cd receiver
cargo build --release      # target/release/arp-gui, arp-receiver, arp-tools
cargo test
```

Windows binaries come from CI. To cross-compile locally from Linux use
[cargo-xwin](https://github.com/rust-cross/cargo-xwin):

```bash
CMAKE_POLICY_VERSION_MINIMUM=3.5 CL=/arch:AVX2 \
  cargo xwin build --release --target x86_64-pc-windows-msvc
```

(`CL=/arch:AVX2` works around libopus' CMake not passing SSE4.1 flags to
clang-cl; it makes the build need an AVX2 CPU, so release builds use CI/MSVC.)

**Android** (JDK 17 + Android SDK; `scripts/setup-android-toolchain.sh`
installs both into your home directory):

```bash
cd android
./gradlew assembleDebug    # app/build/outputs/apk/debug/app-debug.apk
```

Release builds are signed when `ARP_KEYSTORE_FILE`, `ARP_KEYSTORE_PASSWORD`,
`ARP_KEY_ALIAS` and `ARP_KEY_PASSWORD` are set (or `android/keystore.properties`
exists); otherwise `assembleRelease` produces an unsigned APK.

**Releases**: pushing a `v*` tag makes CI build everything and publish a
GitHub release with the notes from [CHANGELOG.md](CHANGELOG.md).

## Command line

```bash
arp-receiver                       # play on the default output
arp-receiver --list-devices
arp-receiver --device cable        # output device by name (substring)
arp-receiver --denoise --auto-usb  # RNNoise + plug-and-play USB
```

`arp-receiver --help` lists everything (gain, echo cancellation, soundpad
folder, port, name...). It prints a status line every second (buffer fill,
FEC/PLC counters, dropouts, drift ppm), so it's easy to see whether a problem
is the network or something else.

## Testing

Bad-network simulation on one PC, no phone needed:

```bash
cd receiver/target/release
# terminal 1: receiver, also recording what it plays
./arp-receiver --headless --dump /tmp/test.wav --duration 40
# terminal 2: 10% loss + 80 ms jitter + clock drift, and a 150 ms stall every 5 s
./arp-tools send --loss 10 --jitter 80 --drift-ppm 200 --spike-every 5 --duration 30
# dropout check (exits with an error on gaps over 20 ms)
./arp-tools analyze /tmp/test.wav
```

When the sender says goodbye the receiver logs a session summary, e.g.
`📊 session 0:30: 0.4% concealed (FEC 3, PLC 1), late 0, dropouts 0, final buffer 50 ms`.
That concealment figure is the one to watch: `analyze` only sees silent gaps
and can't hear concealment on a test tone.

## Troubleshooting

- **The phone doesn't find the PC**: same Wi-Fi (not a guest network)? Firewall
  allows UDP 48222? When you tap *Scan* on the phone, the PC log should show
  `🔎 discovery request from ...`. If it does but the phone still sees nothing,
  the router's "AP/client isolation" is on. You can always type the IP the PC
  window shows.
- **Discord cuts words off**: turn off *Automatically determine input
  sensitivity* and Krisp; use the PC's RNNoise checkbox instead.
- **Echo of the other person**: wear headphones (or try the experimental echo
  cancellation).

## Limitations / roadmap

- No encryption yet (assumes a trusted LAN). Planned: PIN-paired end-to-end encryption.
- One phone at a time; a new phone takes over.
- PCM16 (no FEC) is only meant for very clean links; Opus is the default.

## License

[MIT](LICENSE)
