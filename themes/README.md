# Themes

Zaprust has two theme layers:

- **Built-in: the dynamic system.** There are no fixed built-in
  palettes. Pick a mode (Dark, Light, or System / Auto) and one seed
  color, and the app generates the whole palette from that seed:
  every color in the UI comes from the 18 semantic tokens below,
  derived tonally so surfaces, bubbles, badges and highlights all
  belong to the same family.
- **Custom: one JSON file.** No code, no build step, the app reads
  the values and recolors everything through the same tokens.

## Contribute a theme

1. Copy `template.json` (a partial theme that extends a base) as a
   starting point.
2. Give it a `"name"` and set your colors as `#RRGGBB`.
3. A **full** theme must define all 18 tokens below. A **custom**
  theme may define any subset and name its base with
  `"extends": "dark" | "light"`, missing tokens fall back to the
  base (the default seed's generated palette for that mode).
4. Open a pull request adding the file to this folder. Please include a
   screenshot of the chat screen with your theme applied.

## Tokens

| Token | Used for |
|---|---|
| `bg_app` | App background behind panels |
| `bg_panel` | Panels, cards, headers |
| `bg_panel_alt` | Raised surfaces: chips, composer, hovers |
| `bg_chat` | Chat area background (flat) |
| `text_primary` | Main text |
| `text_secondary` | Secondary text, timestamps |
| `text_faint` | Hints, placeholders, quiet icons |
| `accent` | Primary accent: send button, active states, links |
| `bubble_in` | Incoming message bubble |
| `bubble_out` | Outgoing message bubble |
| `badge_bg` | Unread badge background |
| `badge_text` | Unread badge text |
| `border` | Hairline borders and dividers |
| `system_chip` | Centered system pills (date, encryption notice) |
| `doodle` | Legacy token, still carried by the format; the chat background is flat and nothing paints it |
| `danger` | Destructive actions and errors |
| `tick_read` | Read-receipt ticks (a fixed blue in every generated palette) |
| `system_text` | Text inside system pills |

## Rules

- Colors are `#RRGGBB` only (no alpha, no gradients, themes stay flat).
- Text tokens must stay readable on their backgrounds (aim for WCAG AA
  contrast for `text_primary` on `bg_panel` / `bubble_in` / `bubble_out`).
- Themes never carry code, fonts, or images.

## Built-ins: the dynamic system

Three modes: `dark`, `light`, and `auto` (follows the system). The
palette for a mode is generated from one seed color by
`Tokens::dynamic` in core: the seed's hue drives tinted neutral
surfaces (low saturation), a legible accent (lightness adjusted
until it clears contrast on the panel), a deep or pale tint of the
seed for outgoing bubbles, a fixed semantic color for danger
(red family), read ticks that are always the same blue whatever
the seed, and an unread badge that follows the seed hue, so it
wears the user's chosen accent. Preset seeds ship in Settings (Green,
Blue, Purple, Rose, Amber, Teal); any other color works as a custom
seed. Every preset in both schemes is contrast-verified
programmatically in core tests (text vs bg/surface/bubbles, badge
pair, on-accent and on-danger pairs, monotonic layering).

Older settings files that still name a retired built-in (nord,
monokai, catppuccin, tokyo-night, gruvbox) load as Dark instead of
failing.

## Saturation & Contrast (user controls)

Settings → Appearance has Saturation (0.00–1.50) and Contrast
(0.80–1.20) sliders applied on top of the generated palette by
`core::theme::Tokens::adjust`, a
pure function: saturation scales each color's HSL saturation
around its own hue; contrast scales each channel's distance
from the theme's `bg_app` color (the background itself is the
pivot and maps to itself). Alpha is preserved, results clamp,
and (1.0, 1.0) is the identity, all unit/property tested.
