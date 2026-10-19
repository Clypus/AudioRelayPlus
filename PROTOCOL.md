# AudioRelayPlus protocol v1.1

One-way audio from the phone (client/sender) to the PC (receiver), plus
soundpad remote control. Default port: **48222**.

Two transports carry exactly the same packets:
- **UDP** (the Wi-Fi path): one packet per datagram.
- **TCP** (the USB path, through `adb reverse tcp:48222 tcp:48222`): each
  packet is framed as `[length u16 BE][packet]`. The receiver listens on the
  same TCP port number on 127.0.0.1; in USB mode the phone connects to
  127.0.0.1.

## General rules

- All multi-byte **header fields are big-endian** (network order).
- **PCM16 audio samples are little-endian** (inside the payload; the native
  order of both ARM and x86).
- The audio format is fixed in v1: **48000 Hz, mono**.
  - Codec `OPUS(1)`: frame duration **20 ms** (960 samples). Encoder: VOIP
    mode, VBR, in-band FEC **on**, `packet_loss_perc=20` (0 over USB),
    recommended bitrate 128 kb/s.
  - Codec `PCM16(0)`: frame duration **10 ms** (480 samples = 960 bytes, to
    stay under the MTU).
- The client starts `seq` at 0 and increments it by 1 for every audio packet.
  `timestamp` = sample index of the frame's first sample since the start of
  the stream (u32, wraps around).
- Session: the client picks a random u32 `session`. When it reconnects
  (network dropped, receiver restarted...) it sends **HELLO again with the same
  session**; the receiver resets and opens a new session for an unknown
  session id, and simply ACKs a known one.

## Packet header (4 bytes)

| offset | field | value |
|--------|-------|-------|
| 0      | magic | `0x41 0x52` ("AR") |
| 2      | ver   | `0x01` |
| 3      | type  | see the table below |

## Packet types

| type | name           | direction    | body after the header |
|------|----------------|--------------|-----------------------|
| 1    | DISCOVER       | phone → bcast| `nonce:u32` |
| 2    | DISCOVER_REPLY | pc → phone   | `nonce:u32, port:u16, name_len:u8, name:utf8` |
| 3    | HELLO          | phone → pc   | `session:u32, sample_rate:u32, channels:u8, codec:u8, frame_ms:u8` |
| 4    | HELLO_ACK      | pc → phone   | `session:u32` |
| 5    | AUDIO          | phone → pc   | `session:u32, seq:u32, timestamp:u32, payload...` |
| 6    | HEARTBEAT      | phone → pc   | `session:u32, time_ms:u32` (sender's clock, for RTT) |
| 7    | HEARTBEAT_ACK  | pc → phone   | `session:u32, time_ms:u32` (echoes the HEARTBEAT value) |
| 8    | BYE            | phone → pc   | `session:u32` |
| 9    | SOUND_LIST_REQ | phone → pc   | `session:u32` (sent after connecting) |
| 10   | SOUND_LIST     | pc → phone   | `count:u8, [id:u8, name_len:u8, name:utf8]...` (≤32 names, ≤40 bytes each) |
| 11   | SOUND_PLAY     | phone → pc   | `session:u32, id:u8` — plays from the PC's soundpad |
| 12   | SOUND_STOP     | phone → pc   | `session:u32` — stops all sounds |

## Flows

**Discovery:** the phone sends DISCOVER to the subnet broadcast address(es)
and 255.255.255.255 on port 48222 and collects replies for ~1.5 s. The
receiver answers with a unicast DISCOVER_REPLY (`port` = audio port, normally
48222; `name` = PC name). In parallel the phone sends a framed DISCOVER over
TCP to 127.0.0.1:48222: if an `adb reverse` tunnel to a receiver exists, the
reply lists that PC as "USB".

**Connect:** HELLO → wait for HELLO_ACK (300 ms timeout, 10 tries). Audio
starts once the ACK arrives. Right after that the phone sends SOUND_LIST_REQ
and one DISCOVER over the same link, to learn the PC's name (used to find the
PC again if its address changes).

**Liveness:** the client sends a HEARTBEAT every **500 ms**; the receiver ACKs
at once, but only for its current session. If the client sees no ACK for 3 s
it considers the link DOWN: it keeps sending audio and knocks with a HELLO
once a second; when ACKs come back it carries on (the microphone never stops).
Over Wi-Fi, after 8 s down it also re-scans and switches to the PC with the
same name at a new address. The receiver closes a session after 5 s without
packets and waits for a new HELLO.

**Soundpad updates:** the receiver may send SOUND_LIST at any time — it does
so when its soundpad folder changes. Clients must accept unsolicited lists.

**Shutdown:** a stopping client sends BYE 3 times (best effort).

## Receiver behavior (informative)

- Adaptive jitter buffer: initial target 60 ms; floor 30 ms, ceiling 300 ms.
- A single lost packet is rebuilt from the next packet's **Opus FEC** data;
  otherwise **PLC** fills in.
- When the buffer runs dry it conceals but keeps waiting for the missing
  packet ("elastic"): a late packet is still played and the buffer grows by
  the concealed time, which becomes part of the target. A packet that arrives
  after its slot was already concealed raises the target by 10 ms (at most
  once a second). After 10 s without trouble the target drops by 10 ms
  (so USB/clean links settle at 30 ms).
- After ~6 frames (120 ms) of concealment with nothing arriving: rebuffer
  (silence), target +20 ms.
- Gaps of more than 3 frames are skipped (resync).
- Clock drift: a servo steered by the smoothed buffer fill micro-resamples
  within ±0.5% (absorbing both the phone/PC ppm difference and the sound
  device's rate error).

## Versioning / compatibility

Unknown `ver` or `type` values are silently ignored. v1 has no encryption (it
assumes a LAN); v2 plans packet encryption with a key derived from a pairing PIN.
