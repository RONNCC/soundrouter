# Releasing

Two ways to get an installable build: a notarized one (needs a paid Apple Developer ID) and an ad-hoc signed one (free, needs a manual Gatekeeper override). Everything is driven by `packaging/*.sh`; full Xcode is not needed, the Command Line Tools are enough.

## Version

The single source is `[workspace.package] version` in `Cargo.toml`; every crate inherits it. The packaging scripts stamp it into `CFBundleShortVersionString` of the app and driver `Info.plist` files; `CFBundleVersion` is the git commit count (override with `BUILD_NUMBER`). To release: bump the version, update `CHANGELOG.md`, commit, tag `vX.Y.Z`. The release workflow fails if the tag and the Cargo version differ.

## Environment variables

| Variable | Used by | Meaning |
|---|---|---|
| `SIGN_ID` | `build-driver.sh`, `bundle-app.sh`, `make-dmg.sh` | Code-signing identity, e.g. `Developer ID Application: Name (TEAMID)`. Set: hardened runtime + secure timestamp (required for notarization). Unset: ad-hoc signature, no runtime flag. |
| `INSTALLER_ID` | `make-pkg.sh` | `Developer ID Installer: Name (TEAMID)`. Unset: the `.pkg` is unsigned (a warning is printed) and cannot be notarized. |
| `NOTARY_PROFILE` | `notarize.sh` | `notarytool` keychain profile, created once with `xcrun notarytool store-credentials <profile>`. |
| `BUILD_NUMBER` | scripts | Overrides `CFBundleVersion`. |
| `REQUIRE_NOTICES=1` | `bundle-app.sh` | Fail if `cargo-about` is missing instead of skipping `ThirdPartyLicenses.html`. |

## Notarized release (local)

Needs the Apple Developer Program, a "Developer ID Application" and a "Developer ID Installer" certificate in the login keychain.

```sh
export SIGN_ID="Developer ID Application: Name (TEAMID)"
export INSTALLER_ID="Developer ID Installer: Name (TEAMID)"
xcrun notarytool store-credentials sr-notary          # once
packaging/bundle-app.sh        # also builds the driver: runtime + timestamp, signed inside-out
packaging/make-pkg.sh          # signs the pkg and runs pkgutil --check-signature
packaging/make-dmg.sh          # signs the dmg
export NOTARY_PROFILE=sr-notary
packaging/notarize.sh target/SoundRouter.pkg
packaging/notarize.sh target/SoundRouter.dmg
```

`notarize.sh` checks before it uploads anything, and stops with a message naming the offending file if:

- the pkg is unsigned or not signed by a Developer ID Installer certificate; the dmg is unsigned;
- any app or driver inside (the pkg is expanded, the dmg mounted read-only) fails `codesign --verify --deep --strict`, is ad-hoc, is not signed by a Developer ID Application certificate, or lacks the hardened runtime or a secure timestamp.

`packaging/notarize.sh --check <file>` runs only these checks (no credentials needed). After submitting it reads the notarytool status; anything but `Accepted` prints Apple's log and fails. Then it staples, validates, and runs `spctl --assess`.

## GitHub release (CI)

Push a tag: `git tag vX.Y.Z && git push origin vX.Y.Z`. `.github/workflows/release.yml` (macos-15) builds, then attaches `SoundRouter.pkg`, `SoundRouter.dmg`, `SoundRouter.app.zip`, `SoundRouter.driver.zip` and `SHA256SUMS.txt` to a GitHub release. "Run workflow" (manual dispatch) builds and uploads the same files as a workflow artifact without creating a release.

Repository secrets (all optional):

| Secret | Content |
|---|---|
| `CERT_P12` | base64 of a `.p12` holding both the Developer ID Application and Installer certificates and their keys |
| `CERT_PASSWORD` | password of that `.p12` |
| `SIGN_ID`, `INSTALLER_ID` | the two identity names above |
| `NOTARY_APPLE_ID`, `NOTARY_TEAM_ID`, `NOTARY_PASSWORD` | Apple ID, team ID and an app-specific password |

Without `CERT_P12`/`SIGN_ID` the build is ad-hoc signed; without `INSTALLER_ID` the pkg is unsigned; notarization runs only when all secrets are present. A release built without them says "UNSIGNED build" in its notes.

## Build without an Apple Developer ID

```sh
packaging/bundle-app.sh && packaging/make-pkg.sh && packaging/make-dmg.sh
```

The result is ad-hoc signed and not notarized. Consequences, honestly:

- **Gatekeeper blocks the first launch** of the app and the `.pkg`. Right-click the app (or pkg) and choose Open; on macOS 15 use System Settings > Privacy & Security > Open Anyway. For a downloaded build you trust, `xattr -dr com.apple.quarantine /Applications/SoundRouter.app` (and on the pkg/dmg before opening) removes the block from the shell.
- **The driver may need approval.** coreaudiod loads HAL plug-ins signed by other teams (it carries `com.apple.private.security.clear-library-validation`), and the packaging path removes the quarantine flag, so an ad-hoc driver is expected to load. This has been checked in-process (`packaging/driver-dev.sh fakehost`) but **not** inside a real coreaudiod on a clean machine. If the device does not appear, check `codesign -dv /Library/Audio/Plug-Ins/HAL/SoundRouter.driver`, remove quarantine with `sudo xattr -dr com.apple.quarantine <driver>`, allow it under Privacy & Security if macOS lists it, and see [TROUBLESHOOTING.md](TROUBLESHOOTING.md).
- **Privacy grants reset on every rebuild**, because each ad-hoc build is a new code identity. A self-signed certificate (README, "Signing and privacy grants") keeps one identity; it still cannot be notarized.

## Upgrades and installer behaviour

The driver package's `preinstall` removes any existing `/Library/Audio/Plug-Ins/HAL/SoundRouter.driver`, so an upgrade never leaves files of an older build inside the signed bundle. Its `postinstall` clears quarantine, logs a warning if the signature does not verify, and restarts `coreaudiod` (audio drops for 1-2 s). The app package is non-relocatable. `packaging/uninstall.sh` (also copied to `/Library/Application Support/SoundRouter`) removes the driver and forgets the receipts.

## Not verified

Developer ID signing, notarization, stapling and the `spctl` assessments need Apple credentials and were not run. The hardened-runtime signing path was exercised with a self-signed certificate (flags `runtime`, timestamp present, fakehost load OK).
