# Design

SoundRouter patches audio between apps, microphones and virtual devices. The people using it record podcasts, stream, play music or sit on calls, and the one thing the window has to make obvious is who hears what. The visual language comes from the studio: patch bays, console strips, patch cables ending in jacks, LED level ladders, faders, tape.

Status: the tokens, fonts and egui style below are in place, and every screen reads them. The sidebar, footer, sheets, welcome, copy and the graph (source cables, jacks, LED ladders, empty states) are done.

## The one memorable thing

Colour-coded patch cables and segmented LED meters. Every source gets its own cable colour, cables sag like real cables and end in round jacks. Everything around them is quiet, flat and disciplined, so the cables carry the picture.

## Tokens

All colours, radii and type roles live in `app/src/theme.rs` (`Tokens`, `radius`, the type-role functions). Components read tokens; they never carry their own hex values.

### Surfaces and ink

| Token | Light "Mixer Mist" | Dark "Studio Night" | Use |
|---|---|---|---|
| `canvas` | `#EEF1F7` | `#11131B` | window background, graph |
| `panel` | `#F8F9FC` | `#181B26` | sidebar, footer |
| `raised` | `#FFFFFF` | `#202433` | cards (channel strips), sheets, buttons |
| `ink` | `#151826` | `#ECEEF6` | text |
| `muted` | `#5C6278` | `#8F95AB` | secondary text, idle icons |
| `hairline` | `#D9DDE8` | `#2C3144` | 1 px structure |
| `selected` | `#E1E8FC` | `#1F2848` | selected sidebar row |

Elevation is tonal: a raised surface is one step lighter (dark) or whiter (light) than what it sits on, outlined by a hairline. There are no gradients.

### Cables

Assigned per source in order (`cable_color(index)`), stable per source id, the same on both themes:

| Signal Blue | Cable Magenta | Tangerine | Mint Tape | Violet | Cyan | Sun | Coral |
|---|---|---|---|---|---|---|---|
| `#3B6CFF` | `#E8458B` | `#FF8A1F` | `#19B98A` | `#8B5CF6` | `#12AEDB` | `#F2C21B` | `#FF5C5C` |

Coral is deliberately pinker and redder than the terracotta `#D97757` that generated designs default to. App and device tiles without a real icon (`tile_tint`) draw from the same set.

In the graph the colour of a source is the number in its id (`s3` is slot 2, so removing another source never recolours it), used for the jack before its card title and for every wire that carries it. A wire from an output channel takes the colour of the first source feeding that channel, grey when nothing does. A muted source or a disabled device fades its cables into the canvas.

A cable is a cubic bezier whose handles run out of each end and sag in proportion to the horizontal run (`layout::wire_points`, capped at 36 px): 3 px of cable colour over a 1 px darker outline, ending in round jacks (a filled circle, a lighter ring, a dark hole; an empty socket is a raised circle with a muted ring). A hovered wire thickens and shows an x at its middle. Meters are 20-segment LED ladders, one segment per 3 dB from -57 to 0 dBFS: green to -12, amber to -3, red above, unlit segments in `track`, and the segment at the peak holds its light.

### Status and accent

| Token | Light | Dark | Use |
|---|---|---|---|
| `ok` / `warn` / `bad` | `#19B98A` / `#E89A0C` / `#E5484D` | same | meters, status dots, glyph tiles |
| `ok_ink` / `warn_ink` / `bad_ink` | `#0E7657` / `#8C5C06` / `#C8242A` | `#19B98A` / `#E89A0C` / `#F0696D` | the same states as text |
| `accent` | `#2D5BEF` | `#3366FF` | primary fill under white text, selection |
| `accent_ink` | `#2554E6` | `#7C9BFF` | links and accent text |
| `focus` | `#3B6CFF` | `#3B6CFF` | 2 px keyboard focus ring |
| `danger` | `#D7262C` | `#D7262C` | destructive button fill under white text |

Signal Blue itself (`#3B6CFF`) reaches only 4.4:1 under white text and 3.9:1 as text on the light canvas, so text and primary fills use the nearest step of the same hue that passes. The status hues work the same way: the bright signal colour for meters and dots, a darker (light theme) or lighter (dark theme) ink for words.

### Contrast

Every text token keeps at least 4.5:1 on `canvas`, `panel`, `raised` and `selected` in both themes, and white keeps 4.5:1 on `accent` and `danger`; `theme::tests::text_contrast` checks this. The focus ring keeps 3:1 against the canvas. Hover and press darken filled buttons rather than lighten them, so the white label never drops below 4.5:1.

### Shape

Radii vary on purpose, by what the thing is:

| What | Radius |
|---|---|
| jacks, status dots, knobs | circle |
| channel strips (cards), sheets | 14 |
| sidebar rows, menus, checklist groups | 12 |
| inputs, menu-row highlights, checkboxes | 8 |
| buttons, pills, chips | stadium (`radius::PILL`) |

One shadow style (`soft_shadow`: 0 8 24) exists, and only floating layers use it: menus, tooltips, sheets. Cards, knobs and switches have none.

### Type

Two embedded OFL families, both variable fonts with each role pinned to one instance through its axes:

- **Bricolage Grotesque** for the interface and titles. Its optical-size and width axes give it a voice the egui default font does not have: titles use a display optical size, labels a slightly narrow width so card titles keep their room.
- **JetBrains Mono** for numbers: dB, ms, percentages, channel numbers, so they sit in tabular columns.

| Role | Function | Face |
|---|---|---|
| caption | `caption()` | 11, Regular |
| body | `body()` | 13, Regular |
| label (buttons, chips, card titles, row names) | `label()` | 13, SemiBold, width 90 |
| strong body | `strong()` | 15, Medium |
| section title | `section()` | 20, Bold, optical size 28 |
| device title | `device_title()` | 26, Bold, optical size 28 |
| numbers | `num(size)` | JetBrains Mono |

egui's family names: `Proportional` (Bricolage Regular), `Medium`, `semibold`, `Title`, `Monospace` and `Num` (JetBrains Mono). egui's own fonts stay at the end of every family as fallbacks for symbols and emoji. Text is sentence case.

The brief planned static Regular/Medium/SemiBold/Bold files because egui used to ignore variation axes. egui 0.37 applies them (`FontTweak::coords`), so the app ships the two variable files instead: about 600 KB together, smaller than the static set.

`theme::apply_style` maps the tokens onto egui's `Style` (text styles, widget fills and strokes, selection, text cursor, menus, tooltips, slider rail, 28 px minimum hit height), so stock widgets such as menus, checkboxes, sliders, tooltips, scroll bars and text fields match the painted controls.

### Motion

Motion only answers an action: a short plug-in pop when a wire connects, eased switches and sliders, meter peak-hold falling back. Nothing animates while idle, and an idle window asks for no repaint (the tests check this).

## Chrome and copy

### Sidebar

- **No vibrancy.** The sidebar is an opaque `panel`, in both themes. The macOS blur showed through a 55 % panel and moved the contrast of every row with the wallpaper behind the window, while the main pane next to it is flat `canvas`; a translucent strip beside a flat one looked like two apps. The window is still created transparent with the blur view under it (`main.rs`), but nothing translucent is painted over it, so it is invisible. Revisit only if the main pane becomes translucent too.
- **Title and traffic lights.** The window has no title bar and its content runs under the traffic lights, so the sidebar header is a 46 px band: the lights sit at the left, "Devices" starts at x = 84, past them, and a hairline closes the band. Nothing else is drawn in the band.
- **Rows.** The icon tile takes the device's cable colour (`cable_color(index)`, so a device keeps its colour until a device above it is removed) with a white loop glyph. The selected row is the `selected` fill plus a 3 px bar in the same colour on the straight part of its left edge. The name is the label role, "N channels" is a muted caption, the volume slider has a 14 px knob and the percentage is in the `Num` face. State dot, switch and remove button share the right edge.
- **No loop glyph next to "Devices".** Tried and dropped, by the mirror test: each row already carries the loop mark in its own colour, and a fourth copy in the header added nothing the tiles do not say.

### Footer

The sidebar footer and the window footer are both 44 px, so their hairlines are one line. Left to right in the sidebar: "New device", the one primary action, in `accent_ink` with a ⊕ and a `selected` pill on hover. In the window footer: "Remove device" (same word as the row's remove button, the toast and Undo), the status text, then "Audio delay: Balanced" (a chip that opens the delay menu), "Test setup" and a quiet "…" button. "Uninstall…" is inside that menu: it is destructive and used once, so it must not sit next to the buttons people press.

### Sheets and welcome

Checklist rows are lighter: the group is an inset `canvas` fill with no outline, the title is the strong role (15), the detail is body size (13). Every primary button is `Kind::Primary`; row actions are secondary, so one button per sheet answers Return.

On a brand-new install (the `.setup-seen` marker file does not exist yet) a welcome sheet comes before anything else: "Route sound between apps, mics and virtual devices.", three plain lines (what a virtual device is, what a source is, where audio goes) and one "Get started" button. It creates the marker as an empty file. The marker still holds the version only once the setup sheet was finished or dismissed, so the permission sheet is not skipped, and an upgrade from a version that already wrote the marker never sees the welcome.

### Copy

Sentence case, active voice, plain verbs. A button says what happens ("Allow microphone", "Install driver", "Run test", "Test again", not "Continue" or "Retry" when something more exact is true). The same action keeps the same name wherever it appears: "Remove" on the row, in the footer, in the toast and in Undo; "Open System Settings" in the banner and in the sheets; "Install driver" on the welcome step and after a failure. An error says what failed and what to do next. Empty states invite an action. No middle dots, no spaced em dashes, no apologies.

## What we avoided

- Warm cream backgrounds with a terracotta accent, and near-black with a single acid accent: the canvases are cool blue-grey, and colour comes from the cables.
- The SaaS card kit: one radius on everything, the same grey drop shadow under every card, gradient washes. Radii follow the object, cards are lifted by tone and a hairline, and shadows are reserved for what floats.
- Template chrome: all-caps eyebrow labels, `A · B · C` meta strings, labels built as `WORD — fragment`, monospace for small labels. Mono is for numbers only.
- The system font as the identity: SF Pro (and before it egui's default) made SoundRouter look like any other egui or macOS utility. The embedded fonts also make the snapshot tests render the same on every Mac.

## Licenses

Both fonts are under the SIL Open Font License 1.1, with no Reserved Font Name. Their license texts are in `app/assets/fonts/` and are copied into the app bundle's `Contents/Resources/Licenses/` by `packaging/bundle-app.sh`.
