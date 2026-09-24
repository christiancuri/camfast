# CamFast.app (macOS)

Local packaging of the viewer. Ad-hoc signed, not notarized — personal use only.

## Build / install

```sh
packaging/macos/bundle.sh            # builds dist/CamFast.app (release)
packaging/macos/bundle.sh --install  # builds and copies to /Applications/CamFast.app
packaging/macos/bundle.sh --debug    # uses cargo's dev profile
packaging/macos/dmg.sh               # builds dist/CamFast-<version>.dmg to install on another Mac
```

The DMG contains the app, a shortcut to Applications and `README.txt` with the steps to
allow the first launch (the app is not notarized) and the Local Network permission.

The script can be run from any directory and is idempotent. The version comes from
`[workspace.package] version` in `Cargo.toml`. The icon is generated from
`icon-1024.png` (`swift packaging/macos/make_icon.swift` regenerates the PNG).

## Run

`open -a CamFast`, or from Launchpad/Spotlight. Running with `cargo run` works, but
"Open at login" is only available inside the `.app`.

The first time a camera is accessed, macOS shows the **Local Network** prompt
("CamFast" would like to find and connect to devices on your local network). It must be
allowed, otherwise RTSP connections on the LAN are blocked. To review it later: System
Settings > Privacy & Security > Local Network.

## Files

- Configuration: `~/Library/Application Support/camfast/config.toml`
- Logs: `~/Library/Logs/camfast/`
