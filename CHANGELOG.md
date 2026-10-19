# Changelog

## v0.4.0

### Fixed
- **Choppy audio on jittery Wi-Fi.** The jitter buffer used to sink to its
  30 ms floor even when the network needed more, and then threw away every
  packet that arrived a little late. With 60 ms of jitter and zero loss, about
  5% of the audio was synthesized (PLC/FEC). Now a dry buffer waits for the
  late packet instead of skipping it (the buffer grows instead of losing
  audio), and late packets raise the target delay. Same network: 0.07%
  concealed, nothing dropped. Clean networks and USB still sit at 30 ms.
- **Drift servo noise.** The clock-drift servo sampled the buffer fill once a
  second, and that reading jumps by a whole frame whenever a packet lands. It
  now steers by a smoothed fill.
- **Sound output never recovered** after a device error (VB-Cable reset,
  headset unplugged, sleep/resume, audio service restart). Outputs are now
  watched and reopened on their own; a missing device is retried every few
  seconds.
- **The PC window forgot its settings.** Output device, gain and toggles are
  now remembered. On Windows, VB-Cable's "CABLE Input" is picked
  automatically the first time.
- **USB race:** a closing old connection could end the session that a new
  connection had just taken over.
- A console window flashed on Windows whenever adb ran.
- The status dot and microphone icon rendered as empty boxes.
- The install guide said `adb.exe` alone is enough; it also needs its DLLs.

### Added
- **AI noise suppression** on the PC (RNNoise): fans, keyboards and street
  noise disappear without the cut-outs of input gating.
- **Plug-and-play USB**: the PC sets up `adb reverse` by itself whenever a
  phone is plugged in, and the app lists the PC as "USB" on its own. The USB
  checkbox and button are gone.
- **Level meter and "mute mic"** in the PC window.
- **Soundpad**: the folder is watched (drop files in, or onto the window, and
  they appear on the PC and the phone), a volume slider, and "hear it myself"
  plays the sounds on your speakers too.
- **Windows**: one-click firewall fix (asks for admin rights) and "start with
  Windows (minimized)".
- **Phone app**: finds the PC again by name if its IP address changes,
  auto-connects to the last PC on launch, and has a Mute button in the
  notification.
- English user interface on both sides; Turkish is used automatically when
  the system language is Turkish (`ARP_LANG=en|tr` overrides on the PC).
- Session quality summary in the event log when a phone disconnects.
- `arp-tools send --spike-every/--spike-ms` simulates Wi-Fi stalls.
- Releases are built and published by CI, with a signed release APK.
- MIT license.

### Changed
- The USB/TCP listener only listens on 127.0.0.1 (adb is its only client).
- **The Android app is now signed with a release key.** Uninstall the old
  version once before installing v0.4.0; later updates install on top.

## v0.3.0
- USB (cable) mode over `adb reverse` (TCP): no Wi-Fi, lossless, 30 ms buffer.
- Soundpad: mp3/ogg/wav/flac from a `soundpad` folder; GUI buttons and remote
  play from the phone (long-press = stop).
- Experimental echo cancellation (speex AEC with the speaker output as reference).
- Opus 128 kb/s and a lower-latency buffer profile (target 60 ms, floor 30 ms).

## v0.2.0
- First public release: Rust receiver (CLI + GUI) and the Android app.
- Adaptive jitter buffer with Opus FEC/PLC, clock-drift compensation, live-edge
  catch-up; heartbeat with silent auto-reconnect; foreground mic service with
  wake/Wi-Fi locks; LAN discovery; Linux one-click virtual microphone.
