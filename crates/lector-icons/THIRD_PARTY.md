# Third-party assets in lector-icons

| Directory | Project | License | Notes |
|---|---|---|---|
| `assets/material/` | [Material Icon Theme](https://github.com/material-extensions/vscode-material-icon-theme) | MIT (see `assets/material/LICENSE`) | File and folder icons plus the name/extension mapping (`src/generated.rs`). SVGs minified (whitespace and comments removed), otherwise unmodified. |
| `assets/lucide/` | [Lucide](https://lucide.dev) (`lucide-static`) | ISC (see `assets/lucide/LICENSE`) | Monochrome UI icons. License comment and `class` attribute stripped, otherwise unmodified. `lector-*.svg` (window caption glyphs) are Lector's own (MIT OR Apache-2.0). |

Regenerate with `scripts/gen_icons.py <material-icon-theme dir> <lucide-static dir>`.
