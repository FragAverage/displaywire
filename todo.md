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
- [ ] **Connection blocking without the key** (`server/src/main.rs:499`). The single global handshake rate limit is spent before HS1 is checked. Verify first, rate-limit valid handshakes per source IP, and cap pending sessions per address.
- [ ] **Service registers the exe wherever it sits** (`server/src/windows/service.rs:74-84`). Only allow install from `%ProgramFiles%\DisplayWire` (via `--setup`).
- [ ] Call `sc`, `netsh`, `icacls`, `reg` and `cmd` by full `%SystemRoot%\System32` path. Load `nvEncodeAPI64.dll` with `LoadLibraryExW(…, LOAD_LIBRARY_SEARCH_SYSTEM32)` (`nvenc.rs:76`).
- [ ] Limit the firewall rule to `profile=private,domain remoteip=localsubnet` (plus the tailnet range) (`service.rs:92-95`).
- [ ] Answer discovery only from private/local addresses (`main.rs:490-495`), and correct the "server stays silent" claim in `docs/AUTH.md`.
- [ ] Bound the clipboard read to `GlobalSize(h) / 2` (`server/src/windows/shell.rs:48-51`).
- [x] Make `as_user` return an error instead of falling back to SYSTEM (`shell.rs:111-112`).
- [ ] Never print the key from the service child (`main.rs:273`). (The log folder is now admin-only.)
- [ ] Cap concurrent incoming transfers and their total size (`crates/xfer/src/endpoint.rs:80`, `lib.rs:408`).
- [ ] Fix `quote()` for arguments ending in a backslash (`service.rs:56-58`).
- [ ] Defence in depth: drop unneeded privileges from the SYSTEM child token (`service.rs:210-217`).

### Bugs
- [ ] **A resolution change freezes the stream** (`server/src/windows/capture.rs:273`, the TODO). Rebuild the pipeline at the new size; exclusive-fullscreen games hit this.
- [ ] `--fps 0` divides by zero (`server/src/file.rs:25`), and `--pace-mbps 0` overflows (`main.rs:318`, `:364`). Add clap value ranges.
- [ ] Each mic toggle starts a thread that clears the banner after 1.5 s, which can wipe a newer notice (`client/src/main.rs:307`).
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
