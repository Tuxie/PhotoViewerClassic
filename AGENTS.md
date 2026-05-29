# Photo Viewer Classic

A traditional, simple and FAST cross-platform desktop photo viewer application.

## Tech stack

| Language | Rust v1.96+ |
| GUI toolkit | Slint v1.16+ |
| Image decoding | image-rs/image |
| HEIC support | imazen/heic |
| Target platforms | Windows 11, macOS, Linux |

## Core principles

Extremely fast to start and display the first image provided as a parameter.
Any initialization not absolutely required for displaying the first image (such as loading fonts, extracting exif, support for other image formats, etc) must happen asynchronously.
Supports common modern image types: JPEG, PNG, WebP, GIF, HEIC, AVIF.

## UI

Minimal. Bottom toolbar with buttons for Previous, Next, Rotate Left, Rotate Right, Exit.

## Keyboard navigation

| **Key** | **Action** |
| E | Rotate Counter-clockwise |
| R | Rotate clockwise |
| Left or H | Previous image |
| Right or L | Next image |
| Up or K | Zoom in |
| Down or J | Zoom out |
| Shift + Left/Right/Up/Down/H/L/K/J | Move Left/Right/Up/Down |
| Z | Toggle Zoom-to-Fit / Zoom 1:1 / Last manually set zoom |
| F | Toggle full-screen |
| I | Toogle information overlay |
| T | Edit tags/keywords |
| M | Context menu |
| Esc | Cancel / Back / Quit |
| Enter | Confirm |
| Q | Quit |

## Mouse navigation

| **Mouse Action** | **Action** |
| Scrollwheel up | Zoom in |
| Scrollwheel down | Zoom out |
| Left button + drag | Move |
| Right button | Context menu |
| Hover near left edge | Previous image overlay button appears |
| Hover near right edge | Next image overlay button appears |
| Hover near bottom | Overlay toolbar appears |

## Tag/keyword editing

- Add, remove and change basic tags in JPEG, PNG, WebP, GIF, HEIC and AVIF images in ways that make Windows 11's indexer make them searchable with the built-in Windows search.
- Simple modal overlay window with pre-focused search bar on top and a list pre-populated with the file's tags.
- Typing displays all known tags filtered by characters appearing in that order, case insensitive. For example, searching for abc matches tags by "*a*b*c*".
- Pressing Down or Tab in the search field navigates to the match list, then Up/Down navigates entries. Space selects. Tab moves focus back to the search field. Enter saves. Esc cancels (confirm to discard if there were changes).
- Tags are saved in `$PVC_HOME` (`%APPDATA%\PhotoViewerClassic` on Windows, `~/.config/pvc` on Linux and macOS) in the file `tags.txt` which contains all tags ever added.

## Development rules

- Sleeping and/or polling is absolutely forbidden in the happy path. It is only allowed when recovering after error situations. Emit and/or react to events, don't poll. This includes in tests.
- The GUI must be snappy and responsive at all times. The GUI may not be sluggish just because something heavy is going on in the background.
