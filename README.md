# Flash

A fast download manager for macOS and Windows, written in Rust.

- Splits each file across parallel connections, and resumes after a pause, crash or network drop
- Queue, scheduler, speed limits and a menu bar (or tray) panel
- Browser extension that sends downloads to Flash, with a **Download** button on video players on any site
- Videos from YouTube and ~1,800 other sites through `yt-dlp`, plus HLS (`.m3u8`) and DASH (`.mpd`) streams

## Install on macOS

```bash
pnpm install
```

```bash
pnpm build
```

Move `target/release/bundle/macos/Flash.app` to `/Applications`. The app isn't signed, so clear the quarantine flag once:

```bash
xattr -dr com.apple.quarantine /Applications/Flash.app
```

For video sites:

```bash
brew install yt-dlp ffmpeg
```

## Install on Windows

Download the installer from the latest [CI run](../../actions/workflows/ci.yml): open the run, then **Flash-windows-installer** under Artifacts. Or build it yourself with Rust, pnpm and the Visual Studio C++ build tools:

```powershell
pnpm install
```

```powershell
pnpm build
```

The installer is in `target\release\bundle\nsis\`. Windows SmartScreen may warn because it isn't signed: choose **More info → Run anyway**.

For video sites:

```powershell
winget install yt-dlp.yt-dlp
```

```powershell
winget install Gyan.FFmpeg
```

## Browser extension

1. In Flash, open **Settings → Browser** and copy the pairing token.
2. In Chrome or Edge, open `chrome://extensions` (or `edge://extensions`), turn on **Developer mode**, click **Load unpacked** and pick the `extension` folder.
3. Paste the token in the extension's options.

Hover over a video player on any site and click **Download**. Flash uses yt-dlp for sites it knows, and otherwise the file or stream the player loaded; when there are several, the dialog lets you pick. DRM-protected video (Netflix and the like) can't be downloaded. Turn the button off for a site from the extension's popup.

The extension only talks to Flash on `127.0.0.1`, and only with the token.

## CLI

```bash
cargo install --path crates/rdm-cli
```

```bash
flash "https://example.com/file.iso" -c 16 -d ~/Downloads
```

Press Ctrl-C to pause. Run the same command again to resume.

## Development

```bash
pnpm dev
```

```bash
cargo test --workspace
```
