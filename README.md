# Strata

**Navigate every layer.**

Strata is an experimental, keyboard-first file manager for Linux. It is designed primarily for Omarchy while remaining portable to other modern Linux environments.

## North Star

![Strata design North Star showing Miller-column navigation, the places sidebar, and a Markdown preview](docs/assets/strata-north-star.png)

> This is the original product concept, not a screenshot of the current build. It guides Strata's navigation, information density, theming, and preview experience.

## Vision

- Miller-column navigation
- Folder peeking on hover
- Ultra-fast search
- Rich file previews
- Collapsible sidebar
- Compact and airy density modes
- List and grid views
- Omarchy and system theming
- Complete keyboard navigation

## Documentation

- [Product requirements](docs/prd.md) — product North Star
- [Roadmap](docs/roadmap.md) — milestone sequence and exit criteria
- [Work breakdown](docs/todo.md) — actionable project checklist
- [Architecture principles](docs/architecture.md) — boundaries and customization strategy
- [Prototype design reference](docs/design-reference.md) — visual tokens, motion, and interaction baseline
- [Themes](docs/themes.md) — custom theme schema and Omarchy Quattro integration
- [Unsafe code policy](docs/unsafe-code.md) — exception requirements and current inventory
- [Initial technical direction](docs/technical-direction.md) — original technical assessment

## Technology

- Rust
- GTK4
- GIO
- Native Wayland support

## Technical highlights

Strata is built around a small application model rather than placing filesystem logic in GTK widgets:

- **Native paths stay native.** Invalid UTF-8 names retain their original Linux path bytes and are converted only for display.
- **Navigation and peeking are separate.** Committed Miller columns participate in history; temporary hover peeks never mutate it.
- **Filesystem work is cancellable.** Directory requests carry generations, stream bounded batches, and reject stale results after rapid navigation.
- **Large directories stay virtualized.** Rows render through GTK list models and are exercised against deterministic fixtures containing up to 100,000 entries.
- **Monitoring is incremental.** Coalesced create, remove, move, and metadata events update sorted columns in place while ambiguous events safely fall back to a rescan.
- **Selection survives change.** Sorting, monitoring, and reloads preserve selection by native location rather than fragile row index.
- **Motion avoids layout churn.** Columns reserve their final width before animating, and horizontal reveal targets remain stable during deep navigation.
- **Failure is explicit.** Loading, empty, unavailable, and error states are distinct, with retry support that does not rewrite navigation history.

The architectural boundaries and performance workflow are documented in [`docs/architecture.md`](docs/architecture.md) and [`docs/performance-baseline.md`](docs/performance-baseline.md).

## Development

### Requirements

- Rust 1.85 or newer
- GTK 4.12 or newer
- GtkSourceView 5
- Poppler GLib
- Fontconfig
- A C toolchain and `pkg-config`
- GStreamer codec plugins for audio and video previews (at minimum the
  "good" plugin set for common containers such as MP4, plus `libav` for
  H.264/AAC decoding)

On Arch Linux:

```bash
sudo pacman -S --needed base-devel rust fontconfig gtk4 gtksourceview5 poppler-glib \
  gst-plugins-good gst-libav
```

Run Strata:

```bash
cargo run
```

For development, run Strata in auto-reload mode. The app rebuilds and restarts
when code or bundled assets change. On Arch, Debian/Ubuntu, and Fedora,
`start-dev` installs missing native dependencies (prompting for `sudo`) and
installs `cargo-watch` automatically when needed:

```bash
make start-dev
```

Run the standard quality checks:

```bash
./scripts/check.sh
```

The script always runs formatting, compilation, Clippy, and tests. It also runs dependency-policy and spelling checks when `cargo-deny` and `typos` are installed. CI runs the complete suite, including the minimum supported Rust version.

## Bundled assets

Strata includes a curated Lucide icon subset and the regular JetBrains Mono variable font. See [third-party notices](THIRD_PARTY_LICENSES.md) for versions, modifications, and complete attribution.

## Status

Strata is at the technical-spike stage. The first objective is to validate responsive Miller columns, cancellable hover peeking, incremental directory enumeration, and previews in very large directories.

## License

Strata is licensed under the [GNU General Public License v3.0 or later](LICENSE).
