> **Legacy version.** This is the earlier C#/.NET 10 WPF build, kept for reference. The maintained app is the Rust version in the repository root. The two can still send games to each other (this one speaks protocol v1).
>
> Build: `dotnet publish -c Release -o publish` (in this folder) → `publish\SteamTransfer.exe`.

# Steam Game Transfer

Copy installed Steam games from one Windows PC to another without downloading them again. Works over a USB4/Thunderbolt cable, an ethernet cable, your home network, or a USB drive.

## Install (one click)
1. Copy `publish\SteamTransfer.exe` to the PC: from a USB stick, a download, or anywhere else.
2. Double-click it, then click **Install** in the banner. This adds Start menu and desktop shortcuts, an entry in Settings › Apps (so you can uninstall it), and the firewall rule (one Windows prompt).

You can also skip installing and just run the exe. When you copy a game to a USB drive, the app copies itself onto the drive too (`SteamTransfer\SteamTransfer.exe`), so the other PC can run it straight from there.

## Send a game
1. Open the app on both PCs and connect them:

   | Connection | Typical speed | Notes |
   |---|---|---|
   | **USB4 / Thunderbolt cable** | 10–40 Gbps link | Fastest. Plug it straight into both PCs. Windows creates a network link (on older Thunderbolt 3 PCs you may need the PC maker's Thunderbolt Networking driver). |
   | **Ethernet cable** | 1–10 Gbps | PC to PC or through a router. No IP setup needed. |
   | **Same Wi-Fi** | slow | Works, but expect long transfers. |
   | **USB drive / external SSD** | the drive's speed | Copy to the drive, then install from it on the other PC. |

2. Pick a game, then pick the other PC. The fastest link is chosen for you, and USB4/Thunderbolt links are marked.
3. On the other PC, choose a Steam library and click **Accept**. Steam closes during the copy and reopens afterwards with the game installed.

## Good to know
- **Resume / update:** start the same transfer again and it skips files that are already there (same size and timestamp).
- **Fast links:** on USB4/Thunderbolt or 5G+ ethernet the app uses 4 parallel streams, and 2–3 on slower links. Big files go first, and manifests are written last, so Steam never sees a half-copied game.
- **Workshop content** for the game is copied too.
- The receiving PC's Steam account must own the game to play it.
- Ports: UDP 47800 (finding other PCs), TCP 47801 (transfers).
- Native Windows 11 look (WPF Fluent theme): follows your light/dark mode and accent colour. Runs on Windows 10 too.
- **Uninstall:** Settings › Apps › Steam Game Transfer.

## Build
```
dotnet publish -c Release -o publish
```
Produces a single self-contained `publish\SteamTransfer.exe`.

Code: `Net.cs` (finding other PCs, firewall, parallel transfer protocol) · `Transfer.cs` (file plan, copy pump, resume, USB packages) · `Steam.cs` (libraries, manifests, cover art) · `Installer.cs` (one-click install, USB4/link and drive detection) · `MainWindow.xaml` + `OfferDialog.xaml` + `ViewModels.cs` (native WPF UI).
