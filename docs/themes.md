# Bundled themes

Every theme that ships with Harmonium is bundled as a TOML literal in
`src/config.rs::bundled_themes()` and written to the themes directory on first
run. This page records the upstream source and SPDX license for each bundled
palette so the provenance is auditable.

The TOML shape and the role of each key are documented in
[configuration.md](./configuration.md#theme-files); this page is only about
*where the colors come from* and *under which license they are reused*.

## Bundled themes

| Theme name | Variant | Source | License |
|------------|---------|--------|---------|
| `default` | internal | Harmonium (uses ANSI color names; no upstream palette) | n/a |
| `gruvbox` | dark | https://github.com/morhetz/gruvbox | MIT |
| `gruvbox-light` | light | https://github.com/morhetz/gruvbox | MIT |
| `catppuccin-mocha` | dark | https://github.com/catppuccin/alacritty | MIT |
| `catppuccin-latte` | light | https://github.com/catppuccin/alacritty | MIT |
| `catppuccin-macchiato` | middle | https://github.com/catppuccin/alacritty | MIT |
| `dracula` | dark | https://github.com/dracula/alacritty | MIT |
| `nord` | dark | https://github.com/nordtheme/alacritty | MIT |
| `tokyo-night` | dark | https://github.com/folke/tokyonight.nvim (`extras/kitty/tokyonight_night.conf`) | MIT |
| `solarized-dark` | dark | https://github.com/altercation/solarized | MIT |
| `solarized-light` | light | https://github.com/altercation/solarized | MIT |
| `monokai` | dark | https://github.com/tommodore/monokai (`wezterm/monokai.toml`) | MIT |
| `one-dark` | dark | https://github.com/joshdick/onedark.vim (`term/One Dark.Xresources`) | MIT |
| `github-dark` | dark | https://github.com/primer/primitives (design tokens) + https://github.com/alacritty/alacritty-theme (terminal port) | MIT (Primer) / Apache-2.0 (alacritty port) |
| `rose-pine` | dark | https://github.com/rose-pine/alacritty (`dist/rose-pine.toml`) | MIT |
| `rose-pine-dawn` | light | https://github.com/rose-pine/alacritty (`dist/rose-pine-dawn.toml`) | MIT |
| `synthwave-84` | dark | https://github.com/robb0wen/synthwave-vscode | MIT |
| `kanagawa` | wave (dark) | https://github.com/rebelot/kanagawa.nvim (`extras/ghostty/kanagawa-wave`) | MIT |
| `everforest` | hard (dark) | https://github.com/sainnhe/everforest (`autoload/everforest.vim`) | MIT |
| `palenight` | dark | https://github.com/whizkydee/vscode-palenight-theme (`themes/palenight.json`) | MIT |
| `horizon` | dark | https://github.com/TheoFABIEN/horizon-theme-windows-terminal (derived from https://github.com/jolaleye/horizon-theme-vscode) | MIT |
| `iceberg` | dark | https://github.com/cocopon/iceberg.vim (terminal port: https://github.com/mbadolato/iTerm2-Color-Schemes, scheme `Iceberg Dark`) | MIT |
| `matrix` | dark | https://github.com/i3d/term-themes (`iterm/matrix.itermcolors`) | MIT |
| `moonfly` | dark | https://github.com/bluz71/vim-moonfly-colors (`extras/moonfly.itermcolors`) | MIT |

All `catppuccin-*` themes share the same source repository (the file inside
the repo is `catppuccin-<flavor>.toml`). The same applies to the `rose-pine*`
variants and to the two `solarized-*` themes.

## Themes deliberately not bundled

The themes below are part of the originally requested list but are not bundled
because their licensing is incompatible with Harmonium's current palette
policy. Harmonium itself is licensed under BSD-3-Clause, but bundled palettes
must also have clear, permissive reuse terms suitable for redistribution.

| Theme | Reason for exclusion |
|-------|----------------------|
| `bearded-arc` | GPL-3.0; also lacks a canonical terminal palette (only a VS Code/Zed editor theme) |
| `bearded-black-amethyst` | GPL-3.0; same canonical-palette caveat |
| `bearded-black-gold` | GPL-3.0; same canonical-palette caveat |
| `material-ocean` | GPL-3.0 |
| `cyberpunk` | License of the original `Cyberpunk` palette is not documented; the only widely used terminal port lives in `mbadolato/iTerm2-Color-Schemes`, whose LICENSE explicitly disclaims per-theme copyright, leaving attribution unclear |
| `zenburn` | GPL-2.0-or-later; no clean MIT/BSD/Apache-2.0 community port exists for the terminal palette specifically |

## License policy

Harmonium only bundles palettes that are available under a permissive open
source license (MIT, BSD, Apache-2.0, or equivalent). For each palette the
SPDX identifier above is taken from the LICENSE file in the cited upstream
repository at the time the theme was added. The license of the upstream
project does not extend to Harmonium itself unless explicitly declared in
this repository.

The hex values in each bundled TOML literal are taken from the upstream
terminal-format files (Alacritty TOML, kitty.conf, iTerm2 `.itermcolors`,
Xresources, etc.) — not from editor color schemes, which target a different
rendering layer and may use unrelated values for the same role.

If the upstream LICENSE file is later updated to a different SPDX identifier
or the palette files are moved, update the row above as part of the next
review pass so the table stays accurate.
