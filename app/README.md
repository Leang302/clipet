# Clipet desktop app

## Development

```sh
npm install
npm run tauri dev
```

On macOS, a development build inherits Accessibility consent from the app that
launched it. If automatic paste is blocked, enable Visual Studio Code, Terminal,
or iTerm (whichever owns the development shell) in **System Settings → Privacy &
Security → Accessibility**, then restart that launcher and Clipet.

## Signed macOS release

```sh
npm run tauri:build:mac
```

The release script selects a `Developer ID Application` identity when one is
available, otherwise it uses `Apple Development`. It deliberately refuses to
emit an unsigned bundle: macOS ties Accessibility consent to the app's signing
requirement, so replacing an unsigned build can leave an enabled-looking but
invalid permission row and break automatic paste.

For distribution to other Macs, use a `Developer ID Application` certificate
and notarize the result. `Apple Development` is appropriate only for local
development/testing on machines that trust that certificate.
