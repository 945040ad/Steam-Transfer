# Steam Game Transfer

Copy installed Steam games from one Windows PC to another — over a **USB4/Thunderbolt cable**, **Ethernet**, **Wi-Fi**, or a **USB drive** — without downloading them again.

Pick a game, pick the other PC (or a drive), click **Send**. The other PC asks where to install it, closes Steam while copying, and reopens it with the game ready to play.

- Single 14 MB `.exe`, nothing to install (one-click install optional)
- Native Windows 11 look (Fluent style, follows light/dark mode and your accent colour)
- Finds other PCs automatically — even on a direct cable with no network setup
- Pause / resume, resumable after cancel or crash, survives a USB drive being unplugged mid-copy
- Fast: parallel streams, adaptive compression, and tuned small-file handling (details below)

## Download

Get `SteamTransfer.exe` from the [Releases](../../releases) page. Run it on **both** PCs.

## How to use

1. Run `SteamTransfer.exe` and click **Install** in the banner. This adds Start-menu and desktop shortcuts, an entry in *Settings › Apps*, and a firewall rule; Windows asks for admin permission once.
2. Connect the PCs: a USB4/Thunderbolt cable (fastest), an Ethernet cable (direct or through a router), or the same Wi-Fi.
3. Pick a game, pick the other PC, click **Send**. On the other PC, choose a Steam library and click **Accept**.

**With a USB drive or external SSD:** pick the drive under *Copy to a drive*. The app copies itself onto the drive too. On the other PC, open `SteamTransfer\SteamTransfer.exe` from the drive; the game appears under *Install from a drive*.

| Connection | Typical speed | Notes |
|---|---|---|
| USB4 / Thunderbolt cable | 10–40 Gbps link | Plug into the ⚡ ports on both PCs; Windows creates a network link. Older Thunderbolt 3 PCs may need the PC maker's Thunderbolt Networking driver. |
| Ethernet | 1–10 Gbps | PC to PC or via a router. A direct cable can take up to a minute to connect. |
| Wi-Fi | slower | Works; compression helps a lot here. |
| USB drive / SSD | drive speed | Measured ≈360 MB/s to a USB 3.1 SSD. |

The receiving PC's Steam account must own the game to play it.

## Performance

Measured on a laptop (Intel Core Ultra, NVMe, USB 3.1 SSD), every transfer verified byte-for-byte:

| Scenario | Before tuning | Now |
|---|---|---|
| Silksong (7.6 GB) to a USB SSD | 108 MB/s | **360 MB/s** (drive limit ≈374) |
| 20,000 small files to a USB SSD | 17.9 MB/s | **47.8 MB/s** |
| Silksong over a 1 Gbps link (simulated) | 75 s | **23 s** (compression) |
| One 2 GB file over the network | 578–699 MB/s | **≈1 GB/s** (split across connections) |

How:

- **Many small files** — reader and writer thread pools (32 on SSDs) overlap each file's open/close/antivirus-scan latency; 1 MB socket buffers batch tiny files into big writes.
- **Huge files** — split into 64 MB ranges spread over every connection, so one giant archive uses the whole link.
- **Adaptive compression (zstd)** — only on links under 5 Gbps; a file is sent raw once a chunk doesn't shrink, and compression switches off for already-compressed games (most AAA titles), so they cost no extra CPU.
- **USB drives** — files are written with `FILE_FLAG_WRITE_THROUGH`, so a finished file survives an unplug without a per-file cache-flush command (1.9× faster for small files). Several big files are written at once.
- **Hard drives** — detected automatically; fewer workers and sequential reads to avoid seek thrashing.

## Building

Requires Windows and Rust (stable, MSVC toolchain).

```
cargo build --release        # → target\release\SteamTransfer.exe
cargo test --release         # unit tests + end-to-end test (uses Wallpaper Engine if installed, and `subst X:`)
```

Benchmarks (ignored by default) — e.g. `cargo test --release bench_send -- --ignored --nocapture`:

| Test | What it measures |
|---|---|
| `bench_send` | network send; `BENCH_GAME` = an app id, `small` (20k small files) or `big` (one 2 GB file) |
| `bench_drive` | raw drive speed + export/import; `BENCH_DRIVE=E:\` (uses a scratch folder, removed afterwards) |
| `bench_small_files` | small-file network + drive copy |
| `measure_compressibility` | how well each installed game compresses |

Environment switches for experiments: `STEAMXFER_THROTTLE_MBPS` (simulate a link speed), `STEAMXFER_COMPRESS`, `STEAMXFER_SPLIT`, `STEAMXFER_WORKERS`, `STEAMXFER_BIG_WORKERS`, `STEAMXFER_DURABLE` (`wt` / `flush` / `none`).

## How it works

| File | What |
|---|---|
| `src/steam.rs`, `src/vdf.rs` | Steam libraries, app manifests, cover-art cache, closing/restarting Steam |
| `src/transfer.rs` | File plan, copy pump, pause/cancel, resume, USB-drive packages, unplugged-drive recovery |
| `src/pipeline.rs` | Parallel data plane: read-ahead/writer pools, range splitting, compression, wire formats |
| `src/net.rs` | PC discovery (UDP broadcast on every adapter), firewall rule, control protocol |
| `src/hardware.rs` | USB4/Thunderbolt detection, network links, drives, HDD detection (WMI) |
| `src/installer.rs` | One-click install/uninstall, single instance, UAC |
| `src/app.rs`, `ui/app.slint` | User interface |
| `src/e2e_tests.rs` | End-to-end, interop and benchmark tests |

**Protocol.** Ports UDP 47800 (discovery) and TCP 47801 (transfers). A control connection carries a JSON offer (file list with sizes and timestamps); the receiver replies with the files it already has, then data flows over 2–4 parallel connections and the Steam manifests are sent last, so Steam never sees a half-copied game. Rust peers negotiate **v2** framing (chunked, offset-addressed, optionally zstd-compressed); the legacy C# version speaks **v1**, and the two still interoperate.

**Safety.** Paths from the other PC are confined to the Steam game folders; the user must accept every incoming transfer.

## Legacy C# version

[`legacy/`](legacy) holds the earlier C#/.NET 10 WPF version. It is wire-compatible with this app (protocol v1) but lacks the newer speed work.

## License

Copyright © 2026. All rights reserved. No license is granted to copy, modify or redistribute this code.

Built with [Slint](https://slint.dev) under the Slint Royalty-free Desktop Applications License; the app's **About** dialog shows the required attribution.

[![Made with Slint](https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png)](https://slint.dev)
