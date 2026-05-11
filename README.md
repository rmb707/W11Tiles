# W11 Tiles

A native Windows 11 tiling window manager — pop-shell / Hyprland feel, Win32 underneath.

Drag windows together to create tab groups. Drag onto an edge to split a tile in any direction. Per–virtual-desktop layouts that survive `Ctrl+Win+Arrow` switches. Animated transitions. System-tray controls. Built in Rust against `windows-rs`, no MSVC required to build.

![status](https://img.shields.io/badge/status-pre--alpha-orange)
![tests](https://img.shields.io/badge/tests-60%20passing-brightgreen)
![rust](https://img.shields.io/badge/rust-1.78%2B-orange)
![target](https://img.shields.io/badge/target-x86__64--pc--windows--gnu-blue)

## What it does

- **BSP tiling.** Every new window splits the focused cell along its longer dimension — same heuristic Hyprland uses by default. Closing a window collapses its parent split so the survivor takes the freed space.
- **Tab groups, drag-to-merge.** Drop a window onto the *center* of another tile to stack them as tabs. A title-bar strip across the top of the cell shows every tab; click to switch, click the X to close. Inactive tabs are stacked off-Z behind the active one — they can't pop forward unexpectedly.
- **Edge-aware drops.** Drop on the top/bottom/left/right edge of a tile to split that cell in the corresponding direction. The dragged window becomes the new sibling; the target shrinks to share the cell. Cursor-following highlights show *exactly* which sub-region of which tile your drop will land in before you release.
- **Per-virtual-desktop state.** Each Windows VD gets its own BSP tree. Switching desktops with `Ctrl+Win+Arrow` reveals that VD's layout. Tile arrangement is preserved across cloak/uncloak — the daemon ignores `EVENT_OBJECT_HIDE` (it fires for every cloak) and only treats `EVENT_OBJECT_DESTROY` as a real close.
- **Animated transitions.** Layout changes tween over 140ms with an ease-out cubic. Mid-flight retargets capture the current visible frame as the new origin so motion stays continuous.
- **Multi-monitor + DPI aware.** Per-monitor DPI awareness (V2). Plug/unplug detection runs on a 2-second discovery tick — no daemon restart needed when you change displays. HiDPI scaling on a 4K@150% setup is exact.
- **Low-level keyboard hook.** `WH_KEYBOARD_LL` intercepts hotkeys *ahead* of the shell, so binds like `WIN+ALT+1..9` work reliably regardless of what Windows itself reserves. Same approach used by komorebi and GlazeWM.
- **System-tray icon.** Right-click for Reload Config, About, Quit. Best-effort: if Explorer denies registration, the daemon still runs.
- **No MSVC dependency.** Builds against the GNU/MinGW Rust toolchain via WinLibs — about 300 MB total install vs. 7 GB for Visual Studio Build Tools. `windows-rs` works fine on either.

## Default keybinds

The low-level keyboard hook can claim *any* chord. Defaults use `WIN+ALT` so they don't conflict with Windows-reserved bindings out of the box; remap freely in `%APPDATA%\TileManager\config.toml`.

| Combo                     | Action                              |
|---------------------------|-------------------------------------|
| `WIN+ALT+H/J/K/L`         | Focus left/down/up/right            |
| `WIN+ALT+SHIFT+H/J/K/L`   | Swap focused window                 |
| `WIN+ALT+CTRL+H/J/K/L`    | Resize parent split (5%)            |
| `WIN+ALT+SPACE`           | Toggle floating                     |
| `WIN+ALT+TAB`             | Cycle tabs in current group         |
| `WIN+ALT+SHIFT+TAB`       | Cycle tabs backward                 |
| `WIN+ALT+U`               | Untab focused window                |
| `WIN+ALT+1..9`            | Switch to workspace                 |
| `WIN+ALT+SHIFT+1..9`      | Move focused window to workspace    |
| `WIN+ALT+Q`               | Quit daemon                         |

Plus mouse:

- **Drag** a window's title bar to start a drag. Drop zones light up on every other tile (5 sub-regions per tile: 4 edges + center).
- **Center drop** = merge as tab group.
- **Edge drop** = split + tile (window goes above/below/left/right of target).
- **Drop on blank** for a *non*-tabbed window = float. For a tab member = snap back to the group (tab popouts must be deliberate via the `WIN+ALT+U` keybind or the close X).
- **Click a tab** on the strip to switch active. **Click the X** on a tab to close that window.

## Build

You need **Rust ≥ 1.78** with the `x86_64-pc-windows-gnu` target. Why GNU and not MSVC? See *Architecture* below — short version, this stack avoids Visual Studio Build Tools entirely without giving anything up.

One-time setup (≈300 MB):

```powershell
# 1. Rust toolchain
winget install Rustlang.Rustup
rustup toolchain install stable-x86_64-pc-windows-gnu

# 2. MinGW (UCRT runtime, POSIX threads — what WinLibs ships)
winget install BrechtSanders.WinLibs.POSIX.UCRT

# 3. (in this directory) pin the GNU toolchain for the project
rustup override set stable-x86_64-pc-windows-gnu
```

Then build:

```powershell
cargo build --release
```

Outputs to `target\x86_64-pc-windows-gnu\release\`:
- `tile-daemon.exe` (~1.7 MB) — long-running background process.
- `tilectl.exe` (~900 KB) — command-line client over a named pipe.

## Run

```powershell
.\target\x86_64-pc-windows-gnu\release\tile-daemon.exe
```

The first run writes a starter config to `%APPDATA%\TileManager\config.toml` documenting all defaults; edit and `tilectl reload` to pick up changes (config hot-reload is partially wired — the file lands cleanly, the live reload path is a stub at the moment).

Run the daemon **as Administrator** if you want it to manage Administrator-elevated apps. UIPI prevents non-elevated processes from positioning elevated windows; if your normal workflow has any elevated apps (Task Manager, elevated PowerShell), running tile-daemon elevated lets it tile them too.

## Architecture

```
                 +--------------------+
   WinEventHook  |                    |   SetWindowPos (per-frame)
  Win32 events --+      tile-daemon   +--> tile-win::Applier
  WH_KEYBOARD_LL |   (one tokio task) |   GDI overlays:
   tilectl IPC --+                    |     tile-win::tab_strip
   Tray menu  ---+                    |     tile-win::drop_zones
                 +---------+----------+     tile-win::tray
                           |
                           v
                  tile_core::State
                 (BSP tree per workspace
                   per monitor — pure)
                  + Animator (ease-out)
```

| crate          | purpose                                                                      |
|----------------|------------------------------------------------------------------------------|
| `tile-core`    | Pure layout engine. BSP tree, tab groups, workspaces, state machine, IPC schema, animator. No Win32. Fully unit-tested. |
| `tile-win`     | Win32 surface: `EnumWindows`, `WinEventHook`, `WH_KEYBOARD_LL`, `SetWindowPos`, named-pipe IPC, tab-strip overlay, drop-zone overlay, tray icon, virtual-desktop COM. |
| `tile-daemon`  | Long-running orchestrator. One `tokio::select!` over hooks, hotkeys, IPC, tab clicks, tray commands, cursor poll, animator frames, monitor discovery. |
| `tilectl`      | CLI client. `tilectl ping`, `focus right`, `workspace 2`, `untab`, `cycle-tab`, etc. Talks to the daemon over `\\.\pipe\tilemanager.sock`. |

The split is deliberate: the layout engine never touches the Windows API, so it's testable with `cargo test` on any platform and provable in isolation. 43 unit tests in `tile-core` cover BSP insert/remove/swap/neighbour, tab group merge/untab/cycle, edge-aware drops, animator interpolation, idempotent state mutations, dynamic-workspace creation per VD, etc.

### Why GNU and not MSVC?

Both work on `windows-rs`. The MSVC route requires a 7+ GB Visual Studio Build Tools install with the C++ workload; the GNU route requires a 300 MB MinGW (WinLibs) install. Same Rust source compiles on either. komorebi ships MSVC; GlazeWM ships MSVC; we ship GNU because the developer ergonomics are better and the binary output is identical at runtime — just a different linker pulling in a different libc shim. MSVC is supported as a target if you want it (just `rustup default stable-x86_64-pc-windows-msvc`).

### Why a daemon and not an Explorer extension?

Explorer's window-management API surface (`IShellWindows`, `IVirtualDesktopManager`) is enough to *observe* but not enough to *intervene* with the latency we need. A daemon with a `WinEventHook` and a low-level keyboard hook gets us pre-shell input + sub-frame layout updates without fighting Explorer for the same APIs.

## Status

Pre-alpha. The full happy path works — layout, tabs, drag-to-merge, drag-to-edge, virtual-desktop state, animations, tray. Known sharp edges:

- **`tilectl reload`** is a stub. Drop a new `config.toml` and restart for now.
- **Tray icon** uses the generic `IDI_APPLICATION` icon. Custom `.ico` is a one-line swap; not done yet.
- **Tray icon** doesn't handle `TaskbarCreated` re-add — if Explorer crashes and restarts, the icon disappears until daemon restart. Tracked.
- **No installer.** `cargo-wix` MSI + Task Scheduler logon-trigger is the planned path; nothing committed yet.
- **No code signing** for v1; SmartScreen will warn on first run.
- **Fullscreen apps** are skipped by the applier while in fullscreen and re-tile when they exit. Won't catch DXGI-exclusive-fullscreen games that don't resize their HWND (rare).
- **Multi-monitor mixed DPI** works but hasn't been tested with three monitors at three different scales. Bug reports welcome.

## License

MIT or Apache-2.0, your choice.
