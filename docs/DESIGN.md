# Design

SoundRouter patches audio between apps, microphones and virtual devices. The people using it record podcasts, stream, play music or sit on calls, and the one thing the window has to make obvious is who hears what. The visual language comes from the studio: patch bays, console strips, patch cables ending in jacks, LED level ladders, faders, tape.

Status: the tokens, fonts and egui style below are in place, and every screen reads them. Per-source cable colours (wires are still coloured by left/right channel), sagging cables with jacks, segmented LED meters and the plug-in pop are the next step, as are the remaining `·` strings in the Audio delay menu and the undo toast.

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

## What we avoided

- Warm cream backgrounds with a terracotta accent, and near-black with a single acid accent: the canvases are cool blue-grey, and colour comes from the cables.
- The SaaS card kit: one radius on everything, the same grey drop shadow under every card, gradient washes. Radii follow the object, cards are lifted by tone and a hairline, and shadows are reserved for what floats.
- Template chrome: all-caps eyebrow labels, `A · B · C` meta strings, labels built as `WORD — fragment`, monospace for small labels. Mono is for numbers only.
- The system font as the identity: SF Pro (and before it egui's default) made SoundRouter look like any other egui or macOS utility. The embedded fonts also make the snapshot tests render the same on every Mac.

## Licenses

Both fonts are under the SIL Open Font License 1.1, with no Reserved Font Name. Their license texts are in `app/assets/fonts/` and are copied into the app bundle's `Contents/Resources/Licenses/` by `packaging/bundle-app.sh`.
