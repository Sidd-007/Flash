# RDM: a Rust Download Manager for macOS

An IDM-style download manager. It downloads each file over several parallel connections, resumes after interruptions, keeps a queue with a scheduler and speed limits, has a menu bar app, and comes with a browser extension that sends downloads to it.

## What's in here

| Path | What it is |
|------|------------|
| `crates/rdm-core` | Download engine: segmented, resumable HTTP downloads, HLS video, speed limits, checksums, batch patterns |
| `crates/rdm-cli` | `rdm` command-line tool |
| `crates/rdm-manager` | Download queue: SQLite storage, simultaneous-download limit, scheduler, categories, live events |
| `crates/rdm-app` | macOS app (Tauri v2): window, menu bar icon, notifications, browser bridge |
| `extension` | Browser extension for Chrome (MV3) and Firefox |

## The app

Build the `.app` and `.dmg`:

```bash
npm install
```

```bash
npm run build
```

The output is in `target/release/bundle/macos/RDM.app` (and `bundle/dmg/`). Drag `RDM.app` into `/Applications`.

The app isn't signed with an Apple Developer ID. The first time you open it, right-click it and choose **Open**, or run:

```bash
xattr -dr com.apple.quarantine /Applications/RDM.app
```

To run it in development mode with live reload of the UI:

```bash
npm run dev
```

Closing the window keeps RDM running in the menu bar, where it shows the total download speed. Click the menu bar icon for a small panel with live progress, pause and resume buttons, and the speed profiles. Right-click it for the menu, including **Quit RDM**.

- **Speed profiles:** **Full speed**, **Background** (all downloads capped at the speed set in Settings, 1 MB/s by default) and **Paused**. Switch from the panel, the menu bar's Speed submenu or the ⌘K palette. Leaving **Paused** resumes only the downloads it paused.
- **Refresh link:** when a signed link from a file host expires partway through, right-click the paused or failed download and choose **Refresh link…**. Paste a new link, or start the download again in your browser and RDM catches it. The new link must serve a file of the same size; RDM carries on from the bytes it already has.
- **Duplicate check:** the add dialog and the browser overlay warn when you already have the file, matched by link or by name and size. The button changes to **Download again**.
- The add dialog and the browser overlay remember the last folder you saved to.

### Keyboard shortcuts

| Keys | Action |
|------|--------|
| ⌘N | Add URL (pre-filled from the clipboard) |
| Space | Pause or resume the selected downloads |
| ⌫ | Remove the selected downloads |
| ⌘A | Select all |
| ⌘, | Settings |

## The browser extension

1. Open RDM, go to **Settings → Browser** and click **Copy** next to the pairing token.
2. Chrome: open `chrome://extensions`, turn on **Developer mode**, click **Load unpacked** and choose the `extension` folder.
3. The extension's options open on their own. Paste the token and click **Test connection**.

What it does:
- Takes over downloads of the listed file types (default: archives, disk images, installers, video, audio, PDFs; minimum 1 MB) and passes along the browser's cookies, so downloads that need a login work. If RDM isn't running, the browser downloads the file as usual.
- Adds **Download with RDM** to the right-click menu for links and media.
- Detects HLS video streams (`.m3u8`) on the current tab. Click the toolbar icon, then **Download**.
- On YouTube (watch pages, Shorts, live and embedded players) a **Download** button appears on the player when you hover or pause. It opens RDM's dialog with a quality picker; RDM downloads the video through yt-dlp. Turn it off in the extension options.

**Firefox:** copy `manifest.firefox.json` over `manifest.json` in a copy of the folder, then load it from `about:debugging` → **This Firefox** → **Load Temporary Add-on**.

**Safari** needs an Xcode project (`xcrun safari-web-extension-converter extension`). That requires full Xcode, not just the Command Line Tools.

Security: the bridge only listens on `127.0.0.1`. It requires the pairing token, refuses requests whose `Origin` is a web page, and checks the `Host` header to block DNS-rebinding attacks.

## The CLI

```bash
cargo install --path crates/rdm-cli --force
```

```bash
rdm "https://example.com/big.iso" -d ~/Downloads -c 16
```

```bash
rdm "https://example.com/photos/img[001-120].jpg" --limit 2M
```

```bash
rdm "https://cdn.example.com/video/master.m3u8" -o "My video"
```

```bash
rdm "https://example.com/tool.dmg" --checksum sha256:3a7bd3e2360a3d...
```

Press Ctrl-C to pause. Running the same command again resumes the download.

## How it works

- **Segments:** a 1-byte ranged GET finds the size, whether ranges work, and the filename. The file is split across N HTTP/1.1 connections. Each connection writes straight to its offset in a preallocated `.part` file. When a connection finishes, it takes over half of the biggest remaining segment.
- **Crash safety:** a single writer thread `pwrite`s the data. Every 2 s the `.part` file is fsynced and then the remaining byte ranges are written atomically to `.rdm.json`.
- **HLS:** downloads the master playlist and picks the highest-bandwidth variant. Segments download in parallel and are decrypted if they use AES-128. Finished segments are kept until the end, so the download resumes, and are then joined into one file. If `ffmpeg` is installed, `.ts` output is remuxed to `.mp4`. Live streams and DRM (SAMPLE-AES) aren't supported.
- **Queue:** SQLite in `~/Library/Application Support/com.siddhant.rdm/`. Downloads still running when the app quits go back in the queue.

## Development

```bash
cargo test --workspace
```

```bash
cargo clippy --workspace --all-targets
```
