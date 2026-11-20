# GIF Search (GIPHY)

GIF picker tab: trending and search from GIPHY, using the API key from the central credential store.

- Native feature of Zaprust. Always present; find its switch in Settings (Features or AI category).


## Credentials

Keys are stored once in the central credential store
(Settings > AI); the feature never keeps its own keys.

- `giphy_api_key`: GIPHY API key

## Settings

Declared in the manifest and rendered by the app on the
plugin's settings page:

- `rating`: Content rating
- `result_limit`: Results per fetch


## Where it lives

This feature's logic is this crate; the app wires it into the UI. See [docs/FEATURE-MATRIX.md](../../docs/FEATURE-MATRIX.md) for the native feature layout.
