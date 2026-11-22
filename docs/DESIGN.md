# Design

SoundRouter patches audio between apps, microphones and virtual devices. The people using it record podcasts, stream, play music or sit on calls, and the one thing the window has to make obvious is who hears what. The visual language comes from the studio: patch bays, console strips, patch cables ending in jacks, LED level ladders, faders, tape.

Status: the tokens, fonts and egui style below are in place, and every screen reads them. The sidebar, footer, sheets, welcome, copy, the device summary card, the graph (source cables, jacks, LED ladders, empty states), the health banner and the permissions sheet are done.

## The one memorable thing

Colour-coded patch cables and segmented LED meters. Every output channel gets its own colour, a cable wears the colour of the channel it ends at, cables sag like real cables and end in round jacks. Everything around them is quiet, flat and disciplined, so the cables carry the picture.

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

### Cables and channel colours

Colour names a channel, not a source. The colour of output channel `n` (1-based in the UI) is `Tokens::channel(n - 1)`: eight hues from the Okabe-Ito set (the black swapped for violet and reordered so neighbours differ), one value per theme, each at least 3:1 against `raised` and `canvas` of its theme (`theme::tests::channel_palette` computes it). From channel 9 on the same eight hues come back mixed toward white (dark) or black (light) by 0, 30, 15 and 45 % per group of eight, so neighbouring channels never match up to 64.

| Channel | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 |
|---|---|---|---|---|---|---|---|---|
| Light | `#0072B2` | `#C26A00` | `#00875F` | `#B5568C` | `#9A7B00` | `#2B8FCB` | `#D0480A` | `#7550D8` |
| Dark | `#3D9BE0` | `#E69F00` | `#1FBF8F` | `#E08BBA` | `#F0E442` | `#56B4E9` | `#F0733A` | `#A58BFF` |

- **Cable.** Solid, in the colour of the channel it ends at: a stereo source into channels 1 and 2 shows two colours, a mono mic into L and R shows two. A cable into a monitor takes the monitor channel's own colour (its row number).
- **Output-side jacks** (output channels, monitors) wear their own channel's colour at all times: an open one is a thin ring in that colour on the card, a wired one is solid with a heavier ring and a soft halo.
- **Source jacks** follow their cables. With none the jack is a neutral grey ring; with one it is solid in the colour of the channel that cable ends at; with two or more it is a larger ring in equal arcs, one per destination in channel order, up to three, and from a fourth cable on a "+N" badge counts the rest. The ring stays honest about where the signal goes.
- **Source identity** (`theme::CABLES`, in order: Signal Blue `#3B6CFF`, Cable Magenta `#E8458B`, Tangerine `#FF8A1F`, Mint Tape `#19B98A`, Violet `#8B5CF6`, Cyan `#12AEDB`, Sun `#F2C21B`, Coral `#FF5C5C`; the same on both themes) is only the small dot before the card title (`cable_color(id number)`, stable when other sources come and go) and the icon tile. Device rows in the sidebar keep `cable_color(index)` too.
- **Never colour alone.** Row labels keep the channel number and side ("1 (L)"), and wired is also a heavier ring. Hovering a cable or a jack thickens every cable it touches, enlarges both jacks of each and shows a tooltip such as "Safari, channel 1 (L) to Channel 3 (L)", one line per cable.
- A muted source or a disabled device fades its cables and jacks into the canvas.

A cable is a cubic bezier whose handles run out of each end and sag in proportion to the horizontal run (`layout::wire_points`, capped at 36 px): 3 px of cable colour over a 1 px darker outline, ending in round jacks (a filled circle, a lighter ring, a dark hole; an empty socket is a raised circle with a ring). A hovered wire thickens and shows an x at its middle. Meters are 20-segment LED ladders, one segment per 3 dB from -57 to 0 dBFS: green to -12, amber to -3, red above, unlit segments in `track`, and the segment at the peak holds its light.

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

With macOS "Reduce motion" on, egui's animation time is 0 (`theme::apply_style`): the switch knob jumps and sheets appear without a fade. The setting is read every frame through `Os::reduce_motion`; tests never read the machine's.

## Hit targets and pointer feedback

Every control is reached by a pointer that is rarely exact, so the target is never the glyph. The rules, kept in `app/src/widgets.rs` (`hit`, `hit_rect`, `halo`, `slider`, `switch`, `x_button`) and checked by `app/src/hit_tests.rs`:

- **28 pt minimum.** Every button, switch, slider, port and menu row has an interaction rect of at least 28 x 28 (`MIN_HIT`; the macOS minimum is 24 and `hit_tests` fails below it), even when the glyph is 8 px: `hit(ui, visual, id)` allocates the larger rect centred on what is drawn. The one exception is the graph's ports, 28 wide and 24 tall, because channel rows sit 24 px apart.
- **4 px between neighbours.** Targets never overlap: in a sidebar row the X's target ends 4 px before the switch's, the mute button 4 px before the slider's. A click in the gap goes to the nearer control, never to both.
- **Bigger than the visible switch.** The sidebar switch reaches 6 px past its track on both sides; a card's switch also takes the "On"/"Off" word beside it.
- **One strip per slider.** A slider is a single drag target 28 px tall and as wide as its rail plus the knob; a click on the rail jumps the knob there, a press on the knob grabs it where it is (no jump), a drag keeps following the pointer above and below the strip. The knob is 21 px (22 pressed). Double-click or Option-click gives 100 % (0 dB); the arrow keys and VoiceOver step 1 %.
- **Rows select on press.** A row answers when the button goes down, not up. Controls on top of it (the name too) take their own presses; the name passes the press on to the row, and a double-click on it still renames. A switched-off device is greyed by colour only: its row, switch, mute and slider stay fully live.
- **Forgiving clicks.** egui counts a click only if the pointer moved at most 6 px and the press lasted under 0.8 s. The app sets 10 px and 1.2 s (`theme::apply_input`, `CLICK_SLOP`, `CLICK_SECS`), so a hand that drifts or hesitates still clicks.
- **The pointer says what it can do.** A pointing hand over everything clickable (stock buttons through `Visuals::interact_cursor`, painted ones through `hit`), a horizontal resize cursor over sliders. Glyph buttons show a quiet halo on hover and a firmer one while pressed, filled buttons darken, switches darken, the slider knob grows; all fade over 80 ms.
- **The first click counts.** A click on an inactive window both focuses it and acts: winit's `accepts_first_mouse` is on by default on macOS and eframe leaves it alone, so there is nothing to set.
- **Frames follow input.** egui repaints on every input event, and the worker never holds the UI's lock while it talks to the engine (`worker::tests::meters_only_for_enabled_devices_and_lock_never_held_over_backend`), so a click is handled in the frame after it arrives.

## Chrome and copy

### Sidebar

- **No vibrancy.** The sidebar is an opaque `panel`, in both themes. The macOS blur showed through a 55 % panel and moved the contrast of every row with the wallpaper behind the window, while the main pane next to it is flat `canvas`; a translucent strip beside a flat one looked like two apps. The window is still created transparent with the blur view under it (`main.rs`), but nothing translucent is painted over it, so it is invisible. Revisit only if the main pane becomes translucent too.
- **Title and traffic lights.** The window has no title bar and its content runs under the traffic lights, so the sidebar header is a 46 px band: the lights sit at the left, "Devices" starts at x = 84, past them, and a hairline closes the band. Nothing else is drawn in the band.
- **Rows.** The icon tile takes the device's cable colour (`cable_color(index)`, so a device keeps its colour until a device above it is removed) with a white loop glyph. The selected row is the `selected` fill plus a 3 px bar in the same colour on the straight part of its left edge. The name is the label role, "N channels" is a muted caption, the volume slider has a 21 px knob in a 28 px strip, rows are 84 px and the percentage is in the `Num` face. State dot, switch and remove button share the right edge.
- **No loop glyph next to "Devices".** Tried and dropped, by the mirror test: each row already carries the loop mark in its own colour, and a fourth copy in the header added nothing the tiles do not say.

### Footer

The sidebar footer and the window footer are both 44 px, so their hairlines are one line. Left to right in the sidebar: "New device", the one primary action, in `accent_ink` with a ⊕ and a `selected` pill on hover. In the window footer: the status text (or the "Removed … Undo" toast) from the left content edge, then "Audio delay: Balanced" (a chip that opens the delay menu), "Test setup" and a quiet "…" button. "Uninstall…" is inside that menu, under "Permissions" (the sheet below): it is destructive and used once, so it must not sit next to the buttons people press.

### Device summary card

One full-width card between the device title and the graph (`app/src/summary.rs`; `raised` with a hairline, radius 14, 24 px from each pane edge like the graph's cards). It is the layer that ties the sidebar row to the graph: the same device, read live, and the one place its state and master level sit together. It reads only the config, `EngineStatus` and the meters, and emits the sidebar's own `Edit::Volume` and `Edit::Mute`, so the card and the row never disagree.

- **Row 1** (the whole card when collapsed): the chevron ("Collapse device summary" / "Expand device summary"; the choice lasts for the session and is not saved), the status chip, the amber dropout dot while the device's counter grows ("N dropouts since start", same logic as the sidebar), the output ladders, the mute button ("Mute device") and the master fader ("Master volume") with its dB readout in the `Num` face.
- **Status chip.** A pill with a dot and the state in words: Running (`ok`), Starting and Waiting for permission (`warn`), Error (`bad`), and the quiet Not connected and Off (a hollow dot on `canvas`). Off wins over whatever the engine last said. The tooltip explains it, and for Error carries the failure text, which also fills row 2 in `bad_ink` with an ellipsis and the same tooltip.
- **Output level.** One stereo pair of the same 20-segment ladders as the graph (same -60..0 dBFS scale, same peak hold, 6 px high) per channel pair, at most four, then "+N" with a tooltip. Each pair takes 44 to 200 px; when the row is narrow the fader gives way first (down to 72 px), then pairs drop out. There is no clipping dot: the mixer counts clipped blocks (`Meters::clipped_blocks`), but neither `MeterReading` nor `EngineStatus` carries the count to the window.
- **Row 2:** "Delay 5.3 ms" with a dimmer "Apps 29 ms" (the sidebar's readout and tooltip), or "No delay reading yet" until audio runs, and "N sources, M outputs, K monitors" right-aligned (M counts output channels, as the Output channels column does). "Test setup" and "Audio delay" stay in the footer; the card only shows what they measure.
- **Layout.** Full card 76 px, strip 40 px; at the 1000 px minimum window the card is 702 px wide and nothing clips. Names and long text (an engine error) end in an ellipsis with the full text in a tooltip.

### Health banner and dropout dot

What the engine says about its own health (`EngineStatus::tap_silent`, `hogged`, `bluetooth_hfp`, `xruns`; the decisions are pure functions in `app/src/health.rs`) shows in two places and never blocks the window.

- **Banner.** One row above the footer, the width of the main pane: a `warn` dot, one sentence in body ink that wraps to a second line instead of clipping (so it holds at the 1000 px minimum window), an optional secondary pill, a quiet "+N" badge when more notices wait behind it ("N more notices" to a screen reader) and a close x ("Dismiss notice"). It sits on `raised` with a hairline on top, like the footer's hairline, no shadow: it is part of the frame, not a floating layer. One banner at a time, most important first: System Audio Recording denied while an app is captured, silent app captures, a device another app holds in exclusive use, Bluetooth in phone-call mode. A denied permission replaces the silent-capture notices it explains. Closing a banner hides it until the app quits; the next one takes its place. The capture notices carry **Open Privacy settings**, which opens the System Audio Recording pane directly; this is the one place that says "Privacy settings" rather than "System Settings" because the button lands on one pane, and the setup sheets keep "Open System Settings". The demo never shows banners (its audio is synthetic).
- **Dropout dot.** A 9 px `warn` dot on the corner of a device's icon tile, ringed in the row's own surface, with the tooltip "N dropouts since start". It shows while the engine's counter for that device grew in the last 10 s, and clears itself (at the next status refresh, so up to 2 s late). A counter seen for the first time is a baseline, not news. The delay menu ends with an "Audio health" line (total dropouts since start) and, while they grow, "Dropouts: choose a larger delay" in `warn_ink` (at the largest delay: "close busy apps or turn off devices you do not use").

### Permissions sheet

"Permissions" in the footer's "…" menu and in the menu-bar item opens a review sheet: System Audio Recording (apps), Microphone (inputs), Audio driver and Start at login, each with a status chip, a one-line why and, where something can be done, a secondary button ("Allow microphone", "Open System Settings", "Turn on", "Turn off"). The first two rows are the setup walkthrough's own rows, so their words and buttons are identical; the sheet has one primary button, "Done", and closing it does not count as finishing setup.

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
