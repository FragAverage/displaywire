# Roadmap

Discussed 2026-11-03, one item at a time. Suggested build order: 1 → 10 → 2 → 6 → 5, then 3 and 4, with 7, 8 and 9
alongside; 11 (iPad) after 3, since it reuses the client split; 12 (LG) last, as a prototype.

## 1. Survive a resolution change (built, needs a real test)
Decided: the Mac window keeps its size and fits the new picture; the server does a full rebuild (no NVENC
in-place reconfigure); the Mac shows a short banner ("PC is now 2560×1440").
Today `Capture::duplication` (`server/src/windows/capture.rs:273`) sees the new size and waits forever.
The Mac side already copes: a keyframe with new parameter sets rebuilds the decoder (`receiver.rs:new_decoder`).
- [x] Capture: accept the new duplication, reallocate `frame`/`composed`/pointer textures, report a resize.
- [x] `ScreenSource`: rebuild `Converter` and encoder at the new (even) size, then force a keyframe.
- [x] Input: `Injector.screen` is read once at startup (`server/src/windows/input.rs:47`); refresh it on resize or the mouse lands in the wrong place.
- [x] Client: banner on a size change after the first frame (`UserEvent::VideoSize`, `client/src/main.rs:770`).
- [ ] Test: change resolution in Windows Settings mid-stream; an exclusive-fullscreen game at a non-desktop size; lock screen; rotate a monitor.

## 2. Gamepads (phase A done 2026-11-03, tested with a PlayStation pad)
Decided: bundle ViGEmBus (installer offers it, server detects it and says if missing); emulate an Xbox 360 pad,
or a DualShock 4 when the Mac pad is a PlayStation one (gyro, touchpad, light bar); up to 4 pads; rumble goes back.
- [x] Protocol: `Msg::Pads` (whole state of up to 4 pads, on change and every 50 ms; motion and touchpad fields
      already in the format), `Msg::Rumble`, `Msg::Light`. No version bump: older peers ignore unknown messages.
- [x] Mac: GameController polling in the receiver loop, stable slots, Home/PS button kept from macOS
      (`preferredSystemGestureState`), pads read as released while the window is in the background.
- [x] Mac: rumble through CoreHaptics (`createEngineWithLocality:` by `msg_send!`, as objc2-game-controller only
      binds it on iOS), light bar through `GCDeviceLight`.
- [x] PC: `vigem-client` (features `unstable_ds4`, `unstable_xtarget_notification`), X360 or DS4 per slot, Xbox
      rumble notifications back to the client; unplugged after 1 s of silence or on a new session.
      ViGEmBus's `ERROR_NO_MORE_ITEMS` (no HID read pending) on DS4 reports is ignored, not a replug.
- [ ] Phase B: DS4 extended report (gyro, accelerometer, touchpad) and DS4 notifications (rumble, light bar):
      our own IOCTLs to ViGEmBus (`IOCTL_DS4_SUBMIT_REPORT` with `DS4_REPORT_EX`, `IOCTL_DS4_REQUEST_NOTIFICATION`);
      Mac side reads `GCMotion` and the DualShock/DualSense touchpad.
- [ ] Installer: offer ViGEmBus; check its licence (BSD-3) and redistribution of the signed driver.
- [ ] README: controllers section.

## 3. Windows client (agreed)
Decided: D3D11 video decode (any GPU, zero-copy to the swapchain); one shared cross-platform UI for the PC list,
pairing and settings on both Mac and Windows; one `client` crate with platform modules like the server; v1 has the
Win key / Alt-Tab hook, raw relative mouse, gamepads, clipboard and file drop. UI toolkit: egui, drawn in our own
winit window (also replaces the AppKit launcher, pairing sheet and settings on the Mac).
- [ ] Split `client/src` into shared code (receiver, pacing, clock, stats, settings, servers, transfer, updater) and
      `macos/` + `windows/` behind small traits: Decoder, Presenter, AudioOut, Mic, Input, Clipboard.
- [ ] Shared UI toolkit for launcher, pairing, settings and dialogs; port the Mac launcher to it with egui.
- [ ] Decode: `ID3D11VideoDecoder` HEVC Main (Main10 later for item 8), parse slices with `crates/hevc`.
- [ ] Present: flip-model swapchain (`FLIP_DISCARD`, waitable object, tearing allowed), NV12 → RGB in a pixel shader.
- [ ] Audio: WASAPI shared-mode render with the existing jitter cushion; WASAPI capture for the mic.
- [ ] Input: Raw Input for relative mouse; `WH_KEYBOARD_LL` hook for Win key, Alt-Tab and friends while focused;
      ClipCursor when captured; let go of everything on focus loss.
- [ ] Gamepads: GameInput (or XInput) to the item 2 protocol.
- [ ] Clipboard (text, skip `ExcludeClipboardContentFromMonitorProcessing`) and drag-and-drop files.
- [ ] Wi-Fi: the Windows equivalent of AWDL tuning (`WlanSetInterface` background scan off, streaming mode).
- [ ] Packaging: signed zip + update channel for the client; winget later.
## 4. Mac server (agreed)
Decided: macOS 13+ (ScreenCaptureKit captures system audio, no loopback driver); menu-bar app started by a
LaunchAgent at login, mirroring the Windows tray (no login-window streaming); virtual display comes with item 7.
- [ ] Split `server/src` like the client: shared session/auth/pairing/transfer code, `windows/` and `macos/` sources.
- [ ] Capture: `SCStream` of one display at native size and refresh rate, IOSurface-backed, cursor composited by SCK
      (until item 6), frames only on change.
- [ ] Encode: VideoToolbox HEVC, `RealTime`, `PrioritizeEncodingSpeedOverQuality`, low-latency rate control
      (`EnableLowLatencyRateControl`), no B-frames, infinite GOP; reuse `crates/videotoolbox`.
- [ ] Audio: SCK audio (exclude our own process) → the existing PCM16 path.
- [ ] Mic: play into a virtual input device; decide on BlackHole vs our own Audio Server plug-in.
- [ ] Input: `CGEventPost` for keys, mouse, wheel; Windows-key ↔ ⌘ mapping the other way round; Accessibility
      permission prompt and a clear message when it's missing.
- [ ] Clipboard (NSPasteboard, skip `org.nspasteboard.ConcealedType`) and received files into `~/Downloads/DisplayWire`.
- [ ] Permissions UX: Screen Recording + Accessibility first-run walkthrough; pairing and device list in the menu bar.
- [ ] Keep the Mac awake while streaming (`IOPMAssertion`); Bonjour/our discovery answer.
- [ ] Packaging: same `.app` with a server mode, or a second app; Homebrew cask; signed updates.
## 5. Adaptive bitrate + reference-frame invalidation (agreed)
Decided: the Settings bitrate becomes a ceiling and the server adapts below it ("Fixed" toggle keeps today's
behaviour); GCC-style controller on one-way delay trend plus loss; reference-frame invalidation for lost frames with a
keyframe as fallback; FEC parity adapts 5–30% with the Settings value as the minimum.
- [ ] Client: per-frame feedback (arrival spread, one-way delay from the synced clock, loss, FEC repairs) at ~20 Hz.
- [ ] Server: controller (delay-gradient trend filter, overuse → multiplicative cut, loss > 10% → hard cut, slow
      additive climb back to the ceiling); call `set_bitrate` (NVENC reconfigure, already wired).
- [ ] Pacing follows the current bitrate; don't let the pacer queue build delay itself.
- [ ] RFI: client sends the first lost frame id; server calls `nvEncInvalidateRefFrames` (keeps enough reference
      frames, `maxNumRefFrames`); if the frame is too old or the encoder is MF, force a keyframe as today.
- [ ] Adaptive FEC parity from smoothed loss and burst length.
- [ ] Stats graph: show target bitrate and FEC % alongside actual.
- [ ] Test with netprobe-style impairment (loss, bursts, delay ramps) and on the real Wi-Fi.
## 6. Draw the cursor on the client (agreed)
Decided: the client shows the PC's pointer shape as its own OS cursor (NSCursor on Mac, HCURSOR on Windows), so it
moves with zero added latency; when the PC moves the pointer itself, the client warps its cursor to match.
- [ ] Protocol: pointer shape (id + RGBA + hotspot, cached by id) and position/visibility messages; capability in Hello.
- [ ] Server: stop compositing the pointer for clients that support it (keep it for old clients and captured/game mode
      where the pointer is hidden anyway); convert monochrome/masked-colour shapes to RGBA (invert → outline fallback).
- [ ] Mac: NSCursor from the shape with HiDPI scaling; Windows client: `CreateIconIndirect`.
- [ ] Warp detection: a PC position change that our last injected input doesn't explain → `CGWarpMouseCursorPosition`
      (Mac) / `SetCursorPos` (Windows), rate-limited so it never fights the user.
- [ ] Hidden pointer on the PC (games, video players) hides it locally too.
## 7. Monitor choice and a virtual display (agreed)
Decided: hybrid: the main window streams the PC's main monitor; the client can open any other monitor in its own
window (one more capture + encode session each). Virtual display: bundle the open-source, signed IddCx Virtual
Display Driver (MIT); add a mode matching the client's size and refresh when connecting, remove it afterwards.
- [ ] Protocol: monitor list (id, name, size, refresh, primary) and per-stream monitor id; several video streams per
      session, each with its own frame ids, FEC and keyframe requests.
- [ ] Server: capture + converter + encoder per stream, shared D3D device per adapter; GeForce NVENC session limit
      (8) is plenty; monitor on another adapter needs its own device.
- [ ] Input: pointer coordinates per stream's monitor rect (item 1's refresh generalised); keyboard goes to whichever
      window has focus.
- [ ] Client: "Monitors" menu: open another monitor in a new window; remember per PC which ones were open.
- [ ] Bitrate: split the ceiling across open streams (item 5 controller owns the total).
- [ ] VDD: installer offers it; on connect, add a mode for the client display (e.g. 3024×1964@120), optionally make
      it primary and turn physical screens off for the session ("headless mode" setting); restore on disconnect,
      including after a crash (persist what we changed).
- [ ] Mac server (item 4): `CGVirtualDisplay` equivalent.
## 8. HDR and AV1 (agreed)
Decided: HDR is a manual per-PC toggle (off by default); codec is negotiated: client lists what it decodes in
hardware, server what it encodes, AV1 preferred over HEVC, with an override in Settings.
- [ ] Protocol: codec + bit depth + colour (BT.709 SDR / BT.2020 PQ) in Hello and per keyframe; HDR metadata
      (mastering display, MaxCLL/MaxFALL) when the PC provides it.
- [ ] Server HDR: duplicate with `DuplicateOutput1` in `R16G16B16A16_FLOAT`/`R10G10B10A2`, convert to P010 (PQ),
      NVENC HEVC Main10 / AV1 10-bit; pointer compositing in HDR space (or item 6 makes it moot).
- [ ] Mac HDR: VideoToolbox P010 output, `CALayer` with `wantsExtendedDynamicRangeContent`, BT.2100 PQ colourspace;
      grey out the toggle when the display can't do EDR.
- [ ] AV1: NVENC AV1 (RTX 40+), OBU packetisation, `av1C` already in `crates/videotoolbox`; Mac decode needs M3+
      (check `VTIsHardwareDecodeSupported`); Windows client via D3D11 AV1 decode.
- [ ] Re-run decodebench on real NVENC AV1 streams (the plan notes p99 spikes on synthetic clips).
## 9. AMD and Intel encoders (agreed)
Decided: direct AMF (AMD) and oneVPL/QSV (Intel) now, like direct NVENC; MF stays as the last fallback.
Risk: no AMD/Intel hardware to test on here; needs community testers or cloud machines.
- [ ] Encoder trait already exists (`AnyEncoder`); add `AmfEncoder` and `QsvEncoder`; choose by the adapter's vendor
      id (0x1002 AMD, 0x8086 Intel), fall back to MF, then fail clearly.
- [ ] AMF: ultra-low-latency usage, no B-frames, CBR with small VBV, `ForceIntraRefresh`/LTR for item 5's recovery.
- [ ] QSV (oneVPL): low-power mode, `LowDelayBRC`, `MaxFrameSize`, D3D11 surface sharing with the capture device.
- [ ] Per-vendor bitrate reconfigure and keyframe-on-demand; RFI where the SDK supports it (AMF LTR, QSV RefList).
- [ ] Find testers: a "help wanted" issue with a `--profile` log template; consider a cloud Windows VM with an AMD GPU.
- [ ] README: drop "NVIDIA" from the requirements when this lands.
## 10. Security fixes before wider release (built 2026-11-03, see the list below)
Decided: every open item under "Should fix > Security" below ships in the next release (0.7), before new features
reach users. The firewall rule allows LAN + Tailscale only.

## 11. iPad app (agreed)
Decided: iPad first (universal build so iPhone comes later); Rust core + SwiftUI shell; touch works as a trackpad;
App Store distribution, TestFlight first.
- [ ] Apple Developer account ($99/year); this also lets us notarize the Mac app and drop the `xattr` step.
- [ ] Rust core as an xcframework (`aarch64-apple-ios`, simulator slices): receiver, transport, secure, pairing, xfer,
      clock; a small C ABI (or UniFFI) for connect, frames, audio, input, events. Shares the client split from item 3.
- [ ] Video: VideoToolbox decode (reuse `crates/videotoolbox`) into `AVSampleBufferDisplayLayer` or a CAMetalLayer;
      ProMotion 120 Hz via `CADisplayLink` preferred rate.
- [ ] Audio: AVAudioSession (playback + mic, low IO buffer), same jitter cushion.
- [ ] Touch as trackpad: one-finger drag moves the pointer, tap clicks, two-finger tap right-clicks, two-finger drag
      scrolls, tap-and-drag holds the button; on-screen keyboard button.
- [ ] Hardware keyboard and trackpad/mouse: `UIKey` presses, pointer lock (`prefersPointerLocked`) with
      `GCMouse` relative motion for games; ⌘ → Windows key as on the Mac.
- [ ] Gamepads via GameController (shared logic with the Mac, item 2).
- [ ] SwiftUI: PC list with discovery (needs the local network permission), pairing PIN, settings, stats overlay.
- [ ] Stage Manager / Split View: resize the stream, release input when inactive; pause on background.
- [ ] Clipboard (text) both ways; files via the share sheet / Files picker.
- [ ] App Store review notes: no remote code, explain local network use; privacy labels (no data collected).

## 12. LG TV app (agreed, exploratory)
Decided: native client for webOS (low latency, like moonlight-tv), installed through Developer Mode or the Homebrew
Channel, not the LG store. Control by gamepad, keyboard and mouse, and the Magic Remote as a pointer.
Caveat: a Developer Mode app stops working unless the session is renewed every 50 hours (the Homebrew Channel
avoids that but needs a rootable TV), so this is for enthusiasts until LG offers a better route.
- [ ] Find the TV's webOS version (Settings > General > About This TV, or `ares-device -i` in Developer Mode) and
      check it against what moonlight-tv supports.
- [ ] Toolchain: cross-compile the Rust client for webOS (armv7/aarch64 Linux, LG's NDK sysroot), package as an IPK
      with `ares-package`.
- [ ] Video: LG's native media pipeline (NDL DirectMedia, or SMP on newer webOS) for hardware HEVC with low latency;
      study moonlight-tv's backends for each webOS version.
- [ ] Audio: NDL/PulseAudio output; no mic on a TV.
- [ ] Input: SDL2 (or evdev) for gamepads, keyboard and mouse; Magic Remote as a pointer (motion → absolute pointer,
      OK = click, wheel = scroll); remote Back = menu.
- [ ] UI: the shared egui UI from item 3, sized for a TV (10-foot layout, remote navigation).
- [ ] Decide after a prototype: is latency good enough to keep going, and is a store app ever possible?

## Not planned
- Internet access without Tailscale (STUN hole punching, a relay, accounts). Tailscale stays the answer; we run no
  servers. README explains a manual port-forward + firewall rule for people who insist.

# Open source release checklist

From the readiness audit on 2026-10-30. Ordered by priority.

## Rename to DisplayWire (owner actions)
Code, docs, app bundle, icon, service and protocol magic (`"DW"`, keys `dw1-…`) are renamed. Still to do by hand:
- [ ] Search "DisplayWire" on a trademark register (USPTO / UK IPO).
- [ ] Claim the `displaywire` GitHub org and register `displaywire.app` and/or `.dev` (`.com` is held by the Squadhelp marketplace).
- [ ] Create the new repo, point `origin` at it, then `git push -u origin main --tags`.
- [ ] Delete or archive the old private `FragAverage/streamy` repo once the new one is up.
- [ ] Re-run the release workflow for the new name (push a fresh tag, e.g. `v0.5.1`).
- [ ] On the PC: `streamy-server.exe --uninstall --purge` (old service and key), then install `displaywire-server.exe`.
- [ ] On the Mac: delete `Streamy.app` and `~/Library/Application Support/Streamy`, install `DisplayWire.app`, enter the new `dw1-` key.

## Blockers

### Legal and packaging
- [x] Add a `LICENSE` file: dual MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`), noted in the README.
- [x] Add `license`, `description`, `repository` and `rust-version` to `[workspace.package]`. Set `publish = false` so `proto`, `timing` and the other generic names never go to crates.io. MSRV 1.88, checked with `cargo +1.88 check`.
- [ ] Check that NVIDIA's NVENC header licence (via `moq-nvenc`) fits the chosen licence.

### Security (Windows server, runs as SYSTEM)
- [x] **A standard user can plant the pairing key** (`server/src/auth.rs:38-66`). `load_or_create` trusts any existing file, and `protect()` shells out to `icacls` and ignores failure. Create `C:\ProgramData\DisplayWire` with a protected DACL (SYSTEM + Administrators) via `SetNamedSecurityInfoW`, verify owner/ACL on every load, and refuse to start otherwise. Done: `auth::guard`, `auth::lock_dir`, `acl::owned_by_admins`.
- [x] **Uploads are renamed/deleted as SYSTEM in a user-controlled folder** (`server/src/transfer.rs:201`, `:207`). Junction + object-manager symlink → arbitrary file move/delete. Rename through the open handle (`SetFileInformationByHandle` + `FileRenameInfo`) or inside `as_user`; delete with `FILE_FLAG_DELETE_ON_CLOSE`. Done: rename and delete run inside `as_user`.

### Personal info
- [x] Scrub email, PC hostname, Tailscale IP and Claude co-author lines from git history.
- [ ] Rewrite or delete `PLAN.md`. It still describes personal hardware (M5 Max, RTX 5070 Ti) and home Wi-Fi measurements, and it's stale (see Docs).

## Should fix

### Security
- [x] **Connection blocking without the key** (`server/src/main.rs:499`). The single global handshake rate limit is spent before HS1 is checked. Verify first, rate-limit valid handshakes per source IP, and cap pending sessions per address.
- [x] **Service registers the exe wherever it sits** (`server/src/windows/service.rs:74-84`). Only allow install from `%ProgramFiles%\DisplayWire` (via `--setup`).
- [x] Call `sc`, `netsh`, `icacls`, `reg` and `cmd` by full `%SystemRoot%\System32` path. Load `nvEncodeAPI64.dll` with `LoadLibraryExW(…, LOAD_LIBRARY_SEARCH_SYSTEM32)` (`nvenc.rs:76`).
- [x] Limit the firewall rule to `profile=private,domain remoteip=localsubnet,100.64.0.0/10` (LAN + Tailscale) (`service.rs:92-95`); README: how to add a rule for other networks. Done as `remoteip=localsubnet,100.64.0.0/10` on every
  profile: home networks are often marked Public, and the address filter is what matters.
- [x] Answer discovery only from private/local addresses (`main.rs:490-495`), and correct the "server stays silent" claim in `docs/AUTH.md`.
- [x] Bound the clipboard read to `GlobalSize(h) / 2` (`server/src/windows/shell.rs:48-51`).
- [x] Make `as_user` return an error instead of falling back to SYSTEM (`shell.rs:111-112`).
- [x] Never print the key from the service child (`main.rs:273`). (The log folder is now admin-only.) Already gone with
  per-device keys: nothing prints a key.
- [x] Cap concurrent incoming transfers and their total size (`crates/xfer/src/endpoint.rs:80`, `lib.rs:408`).
- [x] Fix `quote()` for arguments ending in a backslash (`service.rs:56-58`).
- [x] Defence in depth: drop unneeded privileges from the SYSTEM child token (`service.rs:210-217`). Removes debug,
  driver loading, backup and 13 others; keeps TCB/impersonation (acting for the user), base priority, and
  take-ownership/restore (`auth::lock_dir`).

### Bugs
- [ ] **A resolution change freezes the stream**: see Roadmap 1.
- [ ] `--fps 0` divides by zero (`server/src/file.rs:25`), and `--pace-mbps 0` overflows (`main.rs:318`, `:364`). Add clap value ranges.
- [x] Each mic toggle starts a thread that clears the banner after 1.5 s, which can wipe a newer notice (`client/src/main.rs:307`).
- [ ] Name the server `control` thread and handle its death (`main.rs:304`); stop ignoring `spawn` failures.
- [ ] `hevc::nal_type` panics on an empty slice (`crates/hevc/src/lib.rs:10`).

### Docs
- [ ] `PLAN.md` is stale: it lists `server-win/`/`client-mac/` crates that don't exist, says there's no encryption/input/FEC, gives audio as Opus (it's PCM16) and the encoder as MF (it's NVENC by default).
- [ ] README: add `crates/secure` and `crates/xfer` to the layout table, fix the "Media Foundation" encoder line, document `--setup`, and give install steps for `cargo-xwin` and the ffmpeg (libx265 + libsvtav1) that `make-clips.sh` needs.
- [ ] Add `CONTRIBUTING.md`, `CHANGELOG.md`, `SECURITY.md` (private vulnerability reporting) and issue templates.

## Nice to have
- [ ] CI: add `cargo fmt --check` (with a `rustfmt.toml`, then format once), run the `secure`, `xfer` and `streamy-server` tests on the Windows job, and add `cargo deny`.
- [ ] Pin the toolchain with `rust-toolchain.toml`.
- [ ] Tests: client audio jitter cushion, receiver pacing/keyframe logic, clock-offset estimation. Fuzz `Msg::decode`.
- [ ] Add `// SAFETY:` comments to the ~246 `unsafe` blocks (only one exists).
