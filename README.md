# Flash

A fast download manager for macOS, written in Rust.

- Splits each file across parallel connections, and resumes after a pause, crash or network drop
- Queue, scheduler, speed limits and a menu bar panel
- Browser extension that sends downloads to Flash, including YouTube videos
- Videos from YouTube and similar sites through `yt-dlp`, and HLS streams (`.m3u8`)

## Install

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

For video sites, install `yt-dlp` and `ffmpeg`:

```bash
brew install yt-dlp ffmpeg
```

## Browser extension

1. In Flash, open **Settings → Browser** and copy the pairing token.
2. In Chrome, open `chrome://extensions`, turn on **Developer mode**, click **Load unpacked** and pick the `extension` folder.
3. Paste the token in the extension's options.

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
