# Updates

Both apps check GitHub releases for a newer version and install it on request. Nothing is installed without a click.

## How it works

Each release has two extra files next to the downloads:

- `latest.txt`: the version, and the SHA-256 and size of `DisplayWire-macos.zip` and `displaywire-server-windows.zip`.
- `latest.txt.sig`: an Ed25519 signature of `latest.txt`.

CI makes both in the release job (`tools/dwsign`) with a private key held in the repository's `UPDATE_SIGNING_KEY`
secret. The public half is compiled into the apps (`crates/update`, `PUBLIC_KEY`). An app fetches
`releases/latest/download/latest.txt` and its signature, checks the signature, and only then looks at the version.
A download is used only if its size and hash are exactly what the signed manifest names. So having write access to
the GitHub repository is not enough to run code on a PC: the signing key is needed as well. This matters most for the
Windows server, which runs as SYSTEM.

Only a strictly newer `x.y.z` is offered, so an old signed manifest can't be used to downgrade. Fetching is done with
the system `curl` over HTTPS (TLS 1.2+, no redirects to plain HTTP).

## Mac

- Checks at most once a day when the launcher is first shown, and from **DisplayWire > Check for Updates…**.
- A new version asks **Install and Restart / Not Now**. Installing downloads and checks it, quits, swaps
  `DisplayWire.app` in place and opens the new one. If anything fails the old app is put back.
- `DISPLAYWIRE_NO_UPDATE_CHECK=1` turns the daily check off.

## Windows server

- Checks a minute after starting and then twice a day, and shows a balloon when something newer is out.
  The tray menu then has **Update to x.y.z…**, or **Check for updates**.
- Installing downloads and checks the zip, unpacks it under `C:\ProgramData\DisplayWire\update` (administrators and
  SYSTEM only) and runs the new exe's `--setup`, the same step as a first install: stop the service, replace the
  files, start it again. The stream drops for a few seconds. Its log is `update\update.log` there.
- Over SSH: `displaywire-server --check-update`, or `displaywire-server --update` from an administrator shell.
- `DISPLAYWIRE_NO_UPDATE_CHECK=1` in the service's environment turns the checks off.

## Publishing a release

Bump `version` in `Cargo.toml`, commit, and push a matching tag (`v0.7.0`). The release job refuses a tag that
doesn't match `Cargo.toml`. The signing key is also kept (outside the repository) by the maintainer: if it is lost, the
installed apps can't be updated and have to be reinstalled by hand with a build holding a new `PUBLIC_KEY`.
