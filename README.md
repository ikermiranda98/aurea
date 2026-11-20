# Aurea

A desktop trading desk for Windows: watch your account, positions and trading systems on one page, and ask an AI agent about them from a chat beside the desk.

*English | [Español](README.es.md)*

## What it does

- **Trading desk** — equity, daily P&L, exposure, an equity curve, open positions, your systems (bots) with start/stop, the engine log and scheduled tasks. **Close all** and **Panic** are always one click away.
- **Connections** — Interactive Brokers TWS (paper trading only, port 7497) and MetaTrader 5. Without a connection the desk runs in demo mode with sample data.
- **Desk chat** — an agent (OpenCode) in the left drawer that knows the desk. Its system prompt is editable in Settings → Chat.
- **Browser and terminal** — a side pane, always available, to keep a chart, a broker's site or a shell next to the desk.
- **Dictation** — talk to the chat; speech is transcribed on your computer.

Everything stays on your computer: no account, no cloud. Actions that touch real money always ask you first.

## Install

There are no published releases yet. Build the installer from source (Windows, Rust and [Inno Setup 6](https://jrsoftware.org/isinfo.php)):

```powershell
.\scripts\package-windows.ps1 -ReleasesUrl https://github.com/ikermiranda98/aurea/releases
```

The installer is written to `target\package\aurea-<version>-windows-x86_64-setup.exe`. To just run it while developing:

```powershell
cargo run -p zeron
```

## Credits

Aurea is a fork of [Zeron](https://github.com/zeronsh/zeron) (MIT). Internal names (`zeron.exe`, `zeron-*` crates, `ZERON_*` variables) are kept from it. See [ARCHITECTURE.md](ARCHITECTURE.md) for how it is put together.

Licensed under the [MIT License](LICENSE).
