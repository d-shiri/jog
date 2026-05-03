# jog

A terminal UI for browsing and triggering GitHub Actions workflows.

![Main](./images/main.png)
![Runs](./images/runs.png)

## Download

Pre-built binaries are on the [Releases](https://github.com/d-shiri/jog/releases/latest) page.

| Platform | File |
|----------|------|
| Linux x86_64 (static) | `jog-linux-x86_64` |
| macOS Apple Silicon | `jog-macos-aarch64` |
| Windows x86_64 | `jog-windows-x86_64.exe` |

```sh
# Linux
curl -fsSL https://github.com/d-shiri/jog/releases/latest/download/jog-linux-x86_64 -o jog
chmod +x jog && sudo mv jog /usr/local/bin/

# macOS
curl -fsSL https://github.com/d-shiri/jog/releases/latest/download/jog-macos-aarch64 -o jog
chmod +x jog && sudo mv jog /usr/local/bin/
```

## Install from source

```sh
cargo install --path .
# or
cargo build --release && ./target/release/jog
```

## Auth

`jog` needs a GitHub token. It uses, in order:

1. `$GITHUB_TOKEN` if set
2. `gh auth token` (requires the [GitHub CLI](https://cli.github.com/) and `gh auth login`)

## Usage

Run from inside a git checkout — the repo is detected from `origin`:

```sh
jog
```

Override the repo explicitly:

```sh
jog --repo owner/name
```

### Subcommands

```sh
jog run <workflow> [ref] [-i KEY=VAL ...]   # fire-and-forget trigger
jog watch <workflow>                        # live status view
jog open <workflow>                         # open latest run in browser
```

`<workflow>` is the workflow file name (e.g. `ci.yml`) or a fuzzy match on its display name.

## Default keys (TUI)

| View      | Keys |
|-----------|------|
| Global    | `q` quit · `Esc` back · `j`/`k` move · `Enter` open |
| Workflows | `t` trigger · `w` watch · `o` open in browser |
| Runs      | `t` trigger · `r` rerun · `R` rerun-failed · `x` cancel · `w` watch |
| Run detail| `Enter`/`l` open logs |
| Logs      | `j`/`k` scroll · `d`/`u` page · `g` top · `n`/`p` next/prev step · `a` all steps |
| Trigger   | `i`/`Enter` edit field · `Space` cycle choice · `t` submit |

All keys are remappable in `config.toml` (see [Config](#config)).

## Config

`jog` reads `$XDG_CONFIG_HOME/jog/config.toml` (typically `~/.config/jog/config.toml`). All fields are optional.

```toml
[ui]
theme = "dark"
poll_interval_ms = 5000
favorites = ["ci.yml", "deploy.yml"]   # pinned to the top of the list
complete_sound = "/usr/share/sounds/freedesktop/stereo/complete.oga"  # set to "" to disable

[provider]
kind = "github"
repo = "owner/name"  # optional; otherwise auto-detected from git remote

[keys]
quit = "q"
back = "Esc"
down = "j"
up = "k"
trigger = "t"
# ... see src/config.rs for the full list
```

## License

MIT — see [LICENSE](LICENSE).
