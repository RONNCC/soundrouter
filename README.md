# soundrouter

Open-source Loopback-style virtual audio router for macOS, written in Rust.

## Status

Scaffold only. Workspace layout and frozen contracts (`sr-core`) are in place.
The driver, CoreAudio engine, mixer, and GUI are not implemented yet.

## Build

Requires macOS 14+, Command Line Tools (`xcode-select --install`) and Rust 1.89+. Full Xcode is not needed.

```sh
cargo test --workspace
packaging/build-driver.sh      # target/SoundRouter.driver
packaging/bundle-app.sh        # target/SoundRouter.app (ad-hoc signed)
packaging/make-pkg.sh          # target/SoundRouter.pkg (driver + app installer)
packaging/make-dmg.sh          # target/SoundRouter.dmg
```

Distribution builds: set `SIGN_ID` ("Developer ID Application: ...") for `bundle-app.sh`, `INSTALLER_ID` ("Developer ID Installer: ...") for `make-pkg.sh`, then `NOTARY_PROFILE=<profile> packaging/notarize.sh target/SoundRouter.pkg`. The profile is created once with `xcrun notarytool store-credentials`.

## Developer setup

**Signing.** With no `SIGN_ID`, scripts ad-hoc sign (`codesign --sign -`). macOS treats each ad-hoc build as a new app identity, so privacy grants (TCC) reset on every rebuild. To keep grants, create a self-signed certificate once (no sudo, no fee):

1. Open Keychain Access > Certificate Assistant > Create a Certificate.
2. Name it `SoundRouter Dev`, set Identity Type to Self Signed Root and Certificate Type to Code Signing.
3. Build with it: `SIGN_ID="SoundRouter Dev" packaging/bundle-app.sh`.

**Driver install (needs sudo).** The HAL plug-in lives in a system folder, so install and remove it by hand:

```sh
sudo cp -R target/SoundRouter.driver /Library/Audio/Plug-Ins/HAL/ && sudo killall coreaudiod
```

Remove it with:

```sh
sudo rm -rf /Library/Audio/Plug-Ins/HAL/SoundRouter.driver && sudo killall coreaudiod
```

**Privacy permissions.** On first run, grant SoundRouter.app both:

- Microphone: needed for any input, including the virtual device's input inside the engine's aggregate.
- System Audio Recording: needed for app taps (System Settings > Privacy & Security).

The hardware-dependent tests are `#[ignore]`d; run them with `cargo test -- --ignored` on a machine with the driver installed and permissions granted.

## License

MIT OR Apache-2.0
