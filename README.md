# LoKI

LoKI stands for *lokale künstliche Intelligenz*, German for "local artificial intelligence". It is
a launcher-style chat client for KDE Plasma that talks only to AI models running on your own
hardware: this PC, or another machine on your network. Press a shortcut, ask, and the answer
streams in. It can also work on your machine: run shell commands and read, edit and search
files, asking for your approval first.

![LoKI answering a question about disk usage and asking before it runs a command](assets/screenshot.png)

## Features

- A borderless launcher on **Meta+Alt+Space**, centered on the screen under your mouse. It starts
  as a single prompt and grows into a conversation, staying centered as its height changes.
- Local only: it refuses servers outside loopback, private IP ranges and LAN host names. It speaks
  plain http to any OpenAI-compatible server: llama.cpp (`llama-server --jinja`), Ollama,
  LM Studio, vLLM, …
- Tools: run commands, read, write and edit files, list directories, search file contents. You
  choose when it asks: before every tool, only before tools that change something, or never.
- A scratch directory (`~/.local/share/loki/scratch` by default) where commands start and where
  temporary and generated files go, so they don't end up in your home folder
- Pictures in the chat: ask for a plot or chart, and the model draws it with Python and shows it
  inline, as wide as the chat; click it to open it in your image viewer
- A Python environment for scripting in the scratch directory, made with uv (or `python3 -m venv`),
  so packages the model installs never touch your system
- Streaming answers with Markdown, syntax-highlighted code blocks with a copy button, and the
  model's reasoning folded away
- A tray icon with live status (connection and latency, model, tok/s, context size, activity) and
  *Open Chat*, *Clear Session*, *Settings…*, *Exit*. Middle-click toggles the chat. The icon is
  monochrome by default and follows the panel's colors; it can also be the color logo or any
  icon you pick.
- Follows your KDE color scheme, accent color and fonts; icons from Lucide
- The conversation is kept across restarts

## Installation

Download a package for your distribution from the
[releases page](https://github.com/YesserLab/loki/releases):

```sh
# Debian / Ubuntu
sudo apt install ./loki_1.0.2_amd64.deb

# Fedora
sudo dnf install ./loki-1.0.2-1.x86_64.rpm

# Arch / Manjaro
sudo pacman -U loki-1.0.2-1-x86_64.pkg.tar.zst
```

Each package installs the `loki` binary, a desktop entry and the icon. They need glibc 2.39 or
newer: Ubuntu 24.04+, Debian 13+, Fedora 40+, or Arch. LoKI needs a Plasma 6 Wayland session (X11
isn't supported); the window placement relies on KWin. The packages recommend `uv` for the Python environment,
`wl-clipboard` so copied text stays on the clipboard after the launcher closes, and `ripgrep` for
faster searches.

LoKI doesn't add itself to autostart. To start it with your session, add it in System Settings →
Autostart and change the command to `loki --background`, which starts it with only the tray icon.

## Usage

Run `loki`, or launch "LoKI" from your application menu. On first start, open the settings
(Ctrl+, or the gear icon) and enter your server's URL. The default is `http://127.0.0.1:8080`, a
server on this PC; for another machine on your network, use its address, e.g.
`http://192.168.1.50:8080`. LoKI picks the first model the server lists; you can switch models
from the footer.

| Key | Action |
|---|---|
| Meta+Alt+Space | toggle the launcher (change it in System Settings → Keyboard → Shortcuts → LoKI) |
| Enter / Shift+Enter | send / new line |
| Enter (prompt empty) | allow the pending tool call |
| ↑ / ↓ | recall previous prompts |
| Esc | stop the reply, leave settings, or close |
| Ctrl+N | clear the session |
| Ctrl+, | settings |

From the command line: `loki` (start, or show the running instance), `loki --toggle`,
`loki --settings`, `loki --background`, `loki --quit`.

The settings live in `~/.config/loki/config.toml`:

- **Server**: URL (`/v1` is added if the URL has no path), optional API key, model, local network
  restriction
- **Generation**: temperature, max tokens, idle timeout, extra instructions for the system prompt
- **Tools**: on/off, approval mode, start directory, scratch directory, command timeout, output
  limit, max tool rounds
- **Window**: launcher size, hide on focus loss, tray icon, keep history
  (`~/.local/share/loki/history.json`)

The tools:

| Tool | Read-only | |
|---|---|---|
| `run_command` | no | bash; stdin is /dev/null; a timeout kills the whole process group; `cd` persists between calls |
| `read_file` | yes | numbered lines, offset/limit |
| `write_file` | no | creates parent directories |
| `edit_file` | no | exact string replacement (unique unless `replace_all`) |
| `list_directory` | yes | |
| `search_files` | yes | ripgrep, falling back to grep |
| `show_image` | yes | shows a PNG, JPEG or SVG file inline in the chat |

Commands run as your user, so the approval mode is your safety net. They start in the scratch
directory, which is also their `$TMPDIR`; open it with the folder button in the footer.

### Python and pictures

LoKI creates a virtual environment at `<scratch>/.venv` when it starts, with `uv venv` if uv is
installed and `python3 -m venv` otherwise. Commands run with it activated, so the model's
`python`, `pip` and `uv pip install` use it. Ask for a plot and the model installs matplotlib into
the environment, saves the plot to the scratch directory and calls `show_image`, which puts the
picture in the chat:

![LoKI plotting sin(x) and cos(x): it installs matplotlib, writes and runs a script, and shows the plot](assets/screenshot-plot.png)

The system prompt is rebuilt with every message, so a conversation kept across an update or a
settings change always knows the current tools and environment.

## How it fits into Plasma

- **Tray**: a StatusNotifierItem. The monochrome icon is an SVG with Plasma's `ColorScheme-Text`
  class, so the panel recolors it.
- **Shortcut**: registered with KGlobalAccel over D-Bus, so it shows up in System Settings.
- **Placement**: Wayland apps can't position their own windows, so on every open LoKI loads a
  small KWin script over D-Bus. It centers the launcher on the screen under the cursor (again
  whenever the launcher's height changes), keeps it above other windows, hides it from the
  taskbar and focuses it.
- **Single instance**: the running app owns `org.loki.LoKI` on the session bus; a second `loki`
  hands its command to it.

## Building from source

Requirements:

- Rust 1.88 or newer
- [`just`](https://github.com/casey/just) for the build recipes
- [`nfpm`](https://github.com/goreleaser/nfpm) and `objdump` (binutils) to build packages
  (optional)

```sh
just              # list recipes
just build        # build target/release/loki (thin LTO, stripped)
just run          # build and run
just test         # unit tests and clippy
just e2e http://127.0.0.1:8080   # agent runs against a real server
just snapshots    # render the launcher into PNGs (target/snapshots)
just screenshot   # render the README screenshots (assets/screenshot*.png)
just package      # build deb/rpm/archlinux packages into dist/
just stage DIR    # install the files into DIR (e.g. a package root)
just icons        # download the Lucide icons again
```

## Layout

```
src/main.rs        command line and startup
src/config.rs      settings, local-network policy, scratch directory
src/api.rs         OpenAI-compatible streaming client
src/core.rs        conversation state and the agent loop
src/tools.rs       tool definitions and execution
src/desktop.rs     D-Bus service, global shortcut, KWin placement, tray
src/ui.rs          the launcher window (egui)
src/theme.rs       palette from the KDE color scheme, fonts, logo
src/icons.rs       Lucide icons
src/python.rs      the scratch Python environment
src/compat.rs      glibc compatibility shims
assets/            logo, screenshots and their demo plot, Lucide icons (and their fetch script)
packaging/         desktop entry
nfpm.yaml          package manifest
justfile           build, test, package recipes
```

## License

MIT, see [LICENSE](LICENSE).

## Acknowledgements

Icons by [Lucide](https://lucide.dev) (ISC license, see `assets/lucide/LICENSE`). Built with
[egui](https://github.com/emilk/egui), [ksni](https://github.com/iovxw/ksni),
[zbus](https://github.com/dbus2/zbus) and [ureq](https://github.com/algesten/ureq).
