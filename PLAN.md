# Streamy — Plan

Low-latency game streaming. Windows server (NVIDIA) → macOS client (M5 Max). LAN + Tailscale.
Rust throughout.

## Targets
- Our own code per hop: < 1 ms (packetize, FEC, crypto, socket, thread handoff).
- Glass-to-glass: ~6–12 ms wired, ~10–20 ms over Wi-Fi (jitter is the enemy, not mean latency).
- 120 fps default, 240 fps capable. 1440p/4K.

## Decisions
| Area | Choice |
|---|---|
| Language | Rust (`windows` crate, `objc2-*` crates, NVENC SDK via bindgen) |
| Capture | DXGI Desktop Duplication (full desktop) first; WGC per-window later |
| Convert | D3D11 compute shader BGRA→NV12/P010, zero-copy into NVENC |
| Encode | NVENC direct: ULL tuning, P1–P3, no B-frames, infinite GOP + intra-refresh, ~1-frame VBV, slice output |
| Codec | RTX 5070 Ti (Blackwell NVENC) + M5 Max: AV1 and HEVC both HW on each end — benchmark in spike, AV1 likely default on Wi-Fi (better quality per bit) |
| Transport | Custom UDP, ≤1200 B payload, interleaved Reed-Solomon FEC, per-packet AEAD, Noise IK pairing |
| Decode | VTDecompressionSession (RealTime, HW required) → IOSurface → Metal, zero-copy |
| Present | CAMetalLayer, present-on-decode, CADisplayLink timing; "lowest latency" vs "smooth" mode |
| Cursor | Captured without cursor; drawn locally on client in desktop mode; hidden in relative/game mode |
| Input | Reliable-over-UDP channel; raw relative mouse; keyboard scancode map; gamepads (GameController → virtual XInput) |
| Audio | WASAPI loopback → Opus 5 ms frames → CoreAudio (phase 5) |
| Hot path | No async runtime, dedicated threads, SPSC rings (`rtrb`), pooled buffers, MMCSS / QoS userInteractive |

## Wi-Fi specifics (both ends wireless today)
Biggest risk to latency. Mitigations built into the app:
- **Pacing**: spread each frame's packets over part of the frame interval; never burst.
- **FEC interleaved** across packets to survive burst loss; parity adapts 5–30% from loss stats.
- **Adaptive bitrate from day one** (delay-gradient + loss), default ~50–80 Mbps on Wi-Fi.
- **Optional tiny jitter buffer** (0–1 frame, adaptive) for "smooth" mode.
- **Windows**: disable WLAN background scan + enable media-streaming mode via `WlanSetInterface`
  while streaming; disable Wi-Fi power saving.
- **macOS**: AWDL (AirDrop/Continuity) causes periodic latency spikes — detect and offer to
  disable `awdl0` during sessions (needs privileged helper).
- DSCP EF marking (WMM voice/video queue).
- **Measured 2026-10-29** (netprobe, 80 Mbps / 120 fps, Mac Wi-Fi 6 160 MHz, PC Wi-Fi):
  | Setup | Loss | Delay p99 | Max | RTT p50 |
  |---|---|---|---|---|
  | No tuning | 2.8% (170 ms blackouts) | 239 ms | 341 ms | 10.7 ms |
  | AWDL off + PC WLAN tuning | 0% | 17 ms | 63 ms | 9.9 ms |
  AWDL caused a periodic ~70 ms stall. Bursty loss means FEC can't fix a bad link;
  disabling AWDL and PC scans is mandatory, not optional.
- **Recommendation**: wire the server to the router (removes one wireless hop). Mac on 5/6 GHz,
  Wi-Fi 6E/7, close to AP.

## Decode results (M5 Max, decodebench, synthetic x265/SVT-AV1 clips, 2026-10-29)
| Clip | Paced p50 | p99 | max | Max throughput |
|---|---|---|---|---|
| HEVC 1440p 80 Mbps | 1.77 ms | 2.98 | 3.13 | 1635 fps |
| AV1 1440p 85 Mbps | 1.45 ms | 6.65 | 7.70 | 613 fps |
| HEVC 4K 150 Mbps | 2.90 ms | 5.17 | 5.44 | 738 fps |
| AV1 4K 100 Mbps | 2.33 ms | 3.18 | 12.71 | 725 fps |
Output pixel format `&8v0` (Apple lossless-compressed NV12): confirm Metal can sample it
directly, else request `420v`. AV1 tail spikes need re-checking with real NVENC streams.

## Measurement (day one)
- Client/server clock sync (NTP-style).
- Per-packet capture timestamp → per-stage p50/p99 on an in-app HUD (incl. network jitter, FEC recoveries, drops).
- Glass-to-glass via 240 fps camera + flashing test pattern.

## Workspace
```
crates/
  proto/       wire format, zerocopy headers
  transport/   UDP, FEC, AEAD, pacing, reliable input, congestion control
  timing/      clock sync, stage stats
  server-win/  capture, convert, encode, input/gamepad inject, Wi-Fi tuning, audio
  client-mac/  decode, Metal render, input/gamepad capture, audio out
```

## Status (2026-10-29)
**MVP built**: Windows desktop → HEVC (Media Foundation NVENC MFT) → UDP → Mac VideoToolbox →
CoreAnimation layer. Verified on the Mac end to end with `--file` source (120 fps, ~3 ms
send→decoded on loopback). Windows capture/encode path compiles; awaiting first run on Gaming-PC.
MVP shortcuts to revisit: MF instead of direct NVENC SDK, IDR-on-loss instead of FEC/intra-refresh,
no encryption, no input, no cursor, primary monitor only.

## Phases
0. **Spike** — DDA → NVENC → file (HEVC + AV1); VideoToolbox decode on M5 Max; measure encode/decode ms.
   Measure raw Wi-Fi UDP jitter/loss between the two machines with a pacing test tool.
1. **Video MVP** — full desktop over UDP with pacing, FEC, adaptive bitrate, HUD.
2. **Input** — keyboard, raw relative mouse, gamepads, local cursor in desktop mode.
3. **Hardening** — encryption + pairing, intra-refresh/ref-invalidation recovery, reconnect,
   Wi-Fi tuning toggles, Tailscale (direct vs DERP detection).
4. **Audio.**
5. **Window mode** (WGC per-HWND), virtual display (IddCx) for headless/240 Hz.
6. **Network tuning (parked 2026-10-29)** — current Wi-Fi: 9.9 ms RTT p50, 17 ms p99 delay with
   AWDL off + PC WLAN tuning. To do: clear congested 5 GHz ch 36–64 (move to 100–128 or 6 GHz/WPA3),
   check router Wi-Fi 7/MLO settings, test wired server, AWDL privileged helper in client.
7. **Later** — HDR (Main10/PQ), service mode (UAC/login screen), clipboard, Windows client.
