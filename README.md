<a href="https://t3boat.paukraft.com">
  <img src="apps/web/public/og.png" alt="t3boat: Run your T3 Code agents in the cloud, on Boat">
</a>

<p align="center">
  <a href="https://t3boat.paukraft.com">Website</a> ·
  <a href="#install">Install</a> ·
  <a href="apps/desktop/docs/architecture.md">How it works</a>
</p>

t3boat creates cloud machines on your [Boat](https://boat.dev) account and adds them to [T3 Code](https://t3.codes). They sleep when idle, so you only pay for the minutes your agents run.

Chat in T3 Code as usual. A message to a sleeping machine wakes it; running work finishes with your laptop closed.

## What you get

- Create, resize and delete machines from the menu bar
- Add projects the way T3 Code's "Add to another machine" does, `.env` files included
- Sign Claude Code or Codex in on a machine with their own login
- T3 Connect, to reach a machine from your phone or browser
- A machine's dev servers at `localhost` in your own browser, same ports as on the machine, or on a Boat link for any device
- The same machines on every computer you use: add one that another computer created
- Cost per machine, today and this month
- An MCP server, so agents on your computer can manage machines

## What you need

- [T3 Code](https://t3.codes) desktop app
- A [Boat](https://boat.dev) account (t3boat walks you through the CLI sign-in)
- macOS, Windows or Linux

## Install

No prebuilt download yet. Build from source with [Rust](https://rustup.rs) and [Bun](https://bun.sh). Windows also needs the [Microsoft C++ Build Tools](https://v2.tauri.app/start/prerequisites/#windows), Linux [Tauri's system packages](https://v2.tauri.app/start/prerequisites/#linux) (Debian/Ubuntu: `sudo apt install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev libssl-dev build-essential file`).

```sh
git clone https://github.com/paukraft/t3boat && cd t3boat
bun install
bun run build:desktop
```

The app lands in `apps/desktop/src-tauri/target/release/bundle/`:

- **macOS**: copy `macos/t3boat.app` to `/Applications` and open it. It lives in the menu bar.
- **Windows**: run `nsis/t3boat_*-setup.exe`. It lives in the system tray, maybe behind the overflow arrow.
- **Linux**: install `deb/t3boat_*.deb` or run the AppImage in `appimage/`. It lives in the system tray; without one (GNOME without AppIndicator) it opens as a window.

Or let your agent do it:

```
Please build and install t3boat (https://github.com/paukraft/t3boat) on my computer: clone it, follow the Install section of its README, and open the app when it's done.
```

## Usage

1. Click the boat in the menu bar and create a machine.
2. Accept the new backend in T3 Code.
3. Work in T3 Code as usual. Push or open a PR from the machine.

To let Claude Code or Codex on your computer manage machines, click "Add" next to them in t3boat's settings.

## Development

```sh
bun run dev           # website (apps/web)
bun run dev:desktop   # menu bar app (apps/desktop, Tauri)
```

Architecture: [apps/desktop/docs/architecture.md](apps/desktop/docs/architecture.md).

## License

[MIT](LICENSE). Made by [Pau Kraft](https://x.com/paukraft). Not affiliated with or endorsed by T3 Code or Boat.
