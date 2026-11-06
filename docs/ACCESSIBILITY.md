# Accessibility

Lector's accessibility work in v0.4 is keyboard operation, visible focus, interface scaling,
reduced motion and color-independent meaning. **Screen readers are not supported yet** (see
[Screen readers](#screen-readers)).

## Keyboard navigation

Every part of the window can be reached and operated without a mouse. Keys below use `Ctrl`;
on macOS read `Cmd` where the table says so. All of them can be rebound in `keymap.toml`, and
the Keyboard Shortcuts view (File → Preferences, or the command palette) lists every command, bound or not ("Unbound").

| Keys | What it does |
| --- | --- |
| `F6` / `Shift-F6` | Next / previous region: menu bar and toolbar, activity bar, sidebar panel, each editor pane, status bar. A hidden sidebar is skipped. |
| `Tab` / `Shift-Tab` | Next / previous control. Inside a dialog, focus wraps around within the dialog. |
| `Escape` | Close the popover, menu, find bar or dialog and return focus to where it was. |
| `Up` / `Down` | Move through lists and trees (Files, Changes, History, Branches, Search, Outline, References, menus, pickers). |
| `Left` / `Right` | Collapse / expand a tree node. |
| `Enter` / `Space` | Activate the focused button, row or checkbox. |
| `Shift-F10`, `Menu` | Open the context menu of the focused row, tab or editor caret. |
| `F10` | Open the menu bar (Linux and Windows). |
| `Ctrl-Alt-=` / `Ctrl-Alt--` / `Ctrl-Alt-0` | Zoom the interface in / out / reset (View menu). The code font has its own zoom. |
| `Ctrl-Shift-P` (`Cmd-Shift-P`) | Command palette: every action is reachable by name. |

Tab stops follow the visual order. The editor itself keeps `Tab` for indentation when the
file is editable; in read-only views `Tab` moves to the next control.

In the Agent panel every control is a tab stop: the agent picker (`Enter` opens it), thought
and tool-call headers (`Enter` expands or collapses), file locations, the turn buttons
(Review, Undo turn), permission buttons (Allow once, Always, Reject), the changed-file rows
(`Enter` opens the file, `Shift-F10` opens its menu: Open File, Review Turn, Copy Path) and the
prompt box with its attach / Send buttons. Status badges carry their text ("done", "M" with a
"Modified: path" tooltip), not only a color.

### Focus is always visible

Every focusable control shows the theme's `focus_ring`: 2 px (3 px at 160 % scale or more).
Buttons, tabs and inputs swap their border for it; lists and panels outline the region that
holds the focus. The ring has at least 3:1 contrast against every surface it sits on in all
built-in themes (7:1 in the high-contrast themes), enforced by tests in `lector-theme`.

## Scale and text size

* `[ui] scale` in `settings.toml` (0.75 to 2.0, default 1.0) scales the whole interface:
  title bar, menus, dialogs, panels and status bar. Menus: **View → Zoom UI In / Out / Reset**.
* The code font size (`buffer_font_size`, **View → Zoom In / Out**) is separate.
* GPUI 0.2.2 does not expose the operating system's text scale, so Lector does not follow it
  automatically; set `ui.scale` instead.

## Reduced motion

`[ui] reduce_motion` turns off caret blinking, animated scrolling, toast slide-ins and the
spinner rotation. Left unset, Lector follows the operating system where it can read it
(macOS "Reduce motion", GNOME/GTK "enable-animations"; `LECTOR_REDUCE_MOTION=1` forces it);
otherwise motion stays on. **View → Reduce Motion** switches it for the session.

## Color

* Meaning never relies on color alone: diff line numbers carry `+` / `-`, files show a status
  letter, risk badges say "High" / "Med", diagnostics have severity icons, toggles show a
  check or empty box.
* **View → Colorblind-Safe Diff Colors** swaps red/green for blue/orange.
* The four built-in themes (light, dark, high-contrast light and dark) are contrast-tested for
  text, syntax, status colors and the focus ring.

## Known gaps

* No screen-reader support (below).
* The Activity log and a few read-only panels scroll with the mouse or trackpad; keyboard
  scrolling there is limited to moving focus between their buttons.
* The split diff view marks added and removed lines with `+` / `-` and filler rows, but the
  intra-line word highlights are still color only.
* Window controls drawn by Lector on Linux and Windows follow the same keyboard model, but the
  window manager's own title bar is outside Lector's control.
* The OS text scale and high-contrast setting are not read (GPUI 0.2.2 does not expose them);
  choose a high-contrast theme and `ui.scale` by hand.
* Tooltips show shortcuts for actions; a few controls without an action show text only.

## Screen readers

GPUI 0.2.2, the UI framework Lector uses, has no accessibility tree and no AccessKit
integration, so VoiceOver, Narrator, NVDA and Orca cannot read Lector's interface. Nothing in
this release changes that, and we do not claim otherwise.

Plan: Zed's upstream AccessKit integration for GPUI is the route. When a GPUI release that
includes it is adopted, Lector will expose roles, names and states for the controls above
(the focus order, labels and shortcuts described here are the groundwork), and this section
will be updated with what works. Until then a CLI/MCP route exists for scripted use:
`lector status`, `lector changes`, `lector diff` and the other headless commands print plain text or `--json`.
