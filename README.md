# launcher

XDG app launcher for Linux. Lists `.desktop` apps, filter by name, Enter or click to launch, Escape to quit.

Built with [Rune](https://github.com/anomalyco/rune). Borderless, always-on-top, transparent window with a centred card. Does not close on focus loss (focus-follows-mouse would kill it otherwise).

## Keys

| Key | Action |
|---|---|
| Type | Filter apps by name |
| ↑ / ↓ | Move selection by row (keeps column in grid) |
| ← / → | Move selection in grid; caret in list |
| Enter / click | Launch selected app |
| Escape | Close (or leave settings) |
| Ctrl+, | Toggle layout settings |
| Ctrl+P | Pin / unpin selected app |

Empty query shows pinned apps first, then most-launched, then alphabetical. Search covers every app.

Apps with `Terminal=true` open in `$TERMINAL`, or in `xdg-terminal-exec` when `$TERMINAL` is not set. The terminal must accept `-e command`. Launch uses the Desktop Entry `Exec` argv (no shell); a failed start keeps the window open and shows the error.

## Install

```bash
nix build
./result/bin/launcher
```

Bind that binary to a compositor shortcut. Wayland app_id / X11 class is `sh.bzn.launcher`.

### Hyprland

The window is transparent and draws its own card, corners and shadow. Turn off Hyprland's versions so they do not frame the empty margin. The window does not resize, so let Hyprland float and centre it.

```lua
hl.window_rule({
  ["match"] = { ["class"] = "^(sh\\.bzn\\.launcher)$" },
  ["name"] = "launcher",
  ["float"] = true,         -- tiling would stretch the fixed-size window
  ["center"] = true,
  ["pin"] = true,           -- show on every workspace
  ["stay_focused"] = true,  -- focus-follows-mouse cannot move focus away
  ["border_size"] = 0,      -- the card draws its own edge
  ["rounding"] = 0,         -- the card draws its own corners
  ["no_shadow"] = true,     -- the card draws its own shadow
  ["no_blur"] = true,       -- blur would fill the transparent margin
})
```

Set `TERMINAL` in the bind if `xdg-terminal-exec` is not installed. Otherwise `Terminal=true` apps do not start:

```lua
hl.bind("SUPER + SPACE", hl.dsp.exec_cmd("env TERMINAL=ghostty launcher"))
```

## Config

| Path | Purpose |
|---|---|
| `$XDG_CONFIG_HOME/sh.bzn.launcher/settings` | Layout (`key=value`) |
| `$XDG_CONFIG_HOME/sh.bzn.launcher/pinned` | Pinned `.desktop` filenames, one per line |
| `$XDG_STATE_HOME/sh.bzn.launcher/usage` | Launch counts |

Settings panel (gear / Ctrl+,): list vs grid, icon size, show/hide names, compact/tall, corner style, shadow.

## Develop

```bash
nix develop
cargo run
cargo test
```

Runtime wrap in the flake puts Vulkan/Wayland/X11 libs on `LD_LIBRARY_PATH` — keep that if you change the package.
