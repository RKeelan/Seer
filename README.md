# Seer

[![CI](https://github.com/RKeelan/Seer/actions/workflows/ci.yml/badge.svg)](https://github.com/RKeelan/Seer/actions/workflows/ci.yml)

Seer is a fast image viewer for Windows that shows the images it was asked to open and nothing else, in one window, and that agents can drive and read back from.

## Using Seer

Seer runs on Windows 11, x64. It is two programs: `seerw.exe` is the viewer, which owns the window, and `seer.exe` is the command line that drives it.

### Installing

Run `seer install` from wherever `seer.exe` and `seerw.exe` are. It installs Seer for the current user, without administrator rights:

- It copies both programs to `%LOCALAPPDATA%\Programs\Seer` and adds that directory to your `PATH`, so terminals opened afterwards find `seer`.
- It registers Seer for the image types it reads, so it appears in Explorer's Open with menu and in Settings > Apps > Default apps. It never makes itself the default: choose it there, or in Open with > Choose another app, for each type you want it to open.
- It registers the open verb so that Explorer hands a whole selection to one `seerw.exe` through COM, in Explorer's order, and the window selects the first of them. The COM class is a local server in `seerw.exe`, registered for the current user only.

Installing again replaces the programs, even while the viewer is running; the running viewer keeps its old version until it is next started. `seer uninstall` removes the programs, the `PATH` entry and the registrations, and leaves other programs' registrations for the same types alone.

- `seer --version` and `seerw --version` print the program's name and version.
- `seerw <files…>` opens a window showing the first image, fitted to the window and never enlarged, with the others a key press away. A large image appears at window size first and sharpens once its full-resolution decode finishes. The title bar shows the image's file name and size in pixels.
- While an image is shown, the images either side of it are decoded in the background, so moving to one shows it at once. Decoded images are kept in memory up to a quarter of the machine's memory, at most 2 GB; beyond that, those shown longest ago are dropped and decoded again when needed. Images further away are not decoded until they are near the one shown.
- With more than one image open, a strip of thumbnails along the bottom of the window shows them all in order, the shown one outlined, and each group bracketed under its name. The strip scrolls to keep the shown image in view; the wheel over it scrolls it, and clicking a thumbnail shows that image. Thumbnails are decoded on a thread of their own at background priority, so they never hold up the shown image, and only those the strip shows are decoded. They are small, so they are kept for as long as their image is open.
- `seerw` with no files starts the viewer empty. It shows its window once it has been sent an image and drawn it, or after a second with nothing to show.
- `seerw --headless` runs the viewer without ever showing its window or taking the focus, for tests. It answers the command line like any viewer, and also accepts `seer debug input`.
- `--no-activate` shows the window without taking the focus.

The window follows Windows' light or dark setting for apps, title bar included, and switches when the setting changes. A headless viewer is always dark.

Seer reads every still format the Windows Imaging Component can decode: JPEG, PNG, BMP, GIF (the first frame), TIFF, ICO and JPEG XR, plus WebP, HEIC, AVIF and JPEG XL when their codec extensions from the Microsoft Store are installed. An image whose codec is missing shows a message naming the extension to install. EXIF orientation is honoured. Colours are treated as sRGB; embedded colour profiles are ignored.

When an open image's file is written, Seer reloads it once the writing stops, keeping the zoom, position, rotation and marks; if the size changed, the same relative point stays in the middle. A file that is locked or only partly written is tried again after a short wait, and the old image stays on screen meanwhile. Until the new file is decoded, `seer list` reports the image as `preview`. An image that is neither shown nor next to the one shown is reloaded only once it is. A deleted file stays open showing its last image, and `seer list` reports it as missing. Seer closes each file once it has read it, so it never stops another program from writing or deleting it, and it watches only the open files, through change notifications on their folders; it never lists a folder.

### The command line

There is only ever one viewer window. The first `seerw.exe` to start owns it; any later one hands its files to the first, brings its window to the front, and exits. Many started at once all end up in the one window, in the order they reach it. `seer.exe` talks to the viewer over a named pipe that only the current user can open and that refuses connections from other machines.

- `seer open [--group G] [--caption C]… [--compare] [--activate] <files…>` adds images to the window, starting the viewer if none is running, and reports the id given to each. The nth `--caption` belongs to the nth file. Without `--activate` the window does not take the focus: a running viewer flashes its taskbar button, and a new one shows behind the active window.
- `seer list` reports the open images (id, path, caption, group, size, load state and whether the file is missing), which one is selected, the zoom, the visible region and whether compare view is on. An image not decoded yet, or dropped from memory, is `loading` until it is near the one shown.
- `seer close <id or path>…` removes images, and `seer close --all` removes them all. A bare number is an id; write `.\3` for a file named `3`. Naming an image that is not open is not an error: it is reported on stderr and in the JSON's `not_found`.
- `seer clear <group>` removes a group's images.
- `seer marks [--wait] [--timeout <seconds>] [--clear]` reports the marks the user has drawn: for each, its id, its image's id and path, its rectangle (`x`, `y`, `width`, `height` in the image's own pixels, whatever the zoom and rotation) and its note, in the order of the images and then the order the marks were made. `--wait` answers only once the user presses Ctrl+Enter, `--timeout` gives up after that many seconds (which may be fractional) with exit code 5, and `--clear` removes the marks once reported.
- `seer screenshot <out.png>` saves what the window shows.
- `seer quit` closes the viewer.
- `seer install` and `seer uninstall` install and remove Seer for the current user (see [Installing](#installing)).

Every command takes `--json`, which prints the result as JSON on stdout; errors then also appear there as `{"error":{"code":"…","message":"…"}}`. Relative paths are resolved against the current directory before they are sent.

`--instance <name>`, or the `SEER_INSTANCE` environment variable, picks which viewer to talk to (default `default`). Viewers on different instances are independent, which lets tests run without disturbing the viewer in use.

Exit codes:

- 0: success.
- 1: unexpected failure.
- 2: usage error.
- 3: no viewer is running (every command except `open`, `install` and `uninstall`).
- 4: one or more files could not be opened; the others were.
- 5: `marks --wait` timed out.

Two hidden commands exist for testing:

- `seer debug input <event>…` feeds synthetic input to a viewer started with `seerw --headless`. Each argument is a JSON event or array of events, such as `{"type":"key","key":"right"}` or `{"type":"pointer_down","button":"right","at":{"x":120,"y":80}}`; an argument of `-` reads events from stdin, one JSON value per line.
- `seer debug timings` reports the viewer's start-up timings in milliseconds from process creation. It also times the latest move to another image: `navigated` when the selection moved, `navigation_presented` when a frame showing the image was presented, and `navigation_decoded` when the decode that frame waited for finished, which is absent when the image had been decoded ahead.

### Keys and mouse

- The arrow keys, Page Up and Page Down move to the previous or next image; Home and End move to the first and last.
- The wheel, a touchpad pinch and `+` and `-` zoom about the pointer (the keys about the middle). `0` fits the image to the window, never enlarging a small one, and `1` shows it at 100%.
- Dragging with the left or middle button pans.
- `R` turns the view a quarter turn clockwise and Shift+`R` anticlockwise. The file is never changed.
- Delete or Ctrl+W closes the shown image.
- Dragging with the right button draws a mark. `M` toggles mark mode, in which a left drag draws too; `M` or Escape leaves it. Releasing opens a note field beside the box: Enter keeps the mark and Escape discards it. Click a mark or the note drawn above it to select it, Delete to remove it, and double-click either or press F2 to edit its note. A selected mark is drawn in blue rather than red. In a headless viewer, `seer debug input` types a note with a `note_text` event, and screenshots show the field.
- Ctrl+Enter finishes marking, answering any agent waiting on `seer marks --wait`. While one waits, the title bar and a banner across the window say so, and the banner's Done button does the same as Ctrl+Enter.
- When the shown image belongs to a group, `C` toggles compare view, which shows the group side by side with one shared zoom, pan and rotation, in a grid from four images up, each with its caption (or file name) beneath. The selected image's tile is outlined, and clicking a tile selects it. `S` switches between that and a split view, which draws the image and the next in its group over the same area, divided by a line you can drag, with each one's caption in its top corner. `open --compare` opens the group straight into compare view. In single view the caption shows in the title bar.

## Developing Seer

Seer is a native Win32 program written in Rust. Building it needs the stable Rust toolchain and the Windows SDK's resource compiler, which comes with the Visual Studio C++ build tools; measuring test coverage also needs [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov).

Both executables are thin entry points over one library: `src/bin/seer.rs` calls `seer::cli` and `src/bin/seerw.rs` calls `seer::viewer`. The viewer's behaviour lives in `src/model`, which has no Win32 types: `model::app::App` is a state machine that takes each input (a key, a pointer event, a pipe request, a decoded image) with the time it arrived, and returns the effects for the window to carry out and a plain-data scene for the renderer to draw. A whole session can be driven in its tests without a window. Release builds link the C runtime statically (see `.cargo/config.toml`), so they need no redistributable.

- `cargo build` builds both programs.
- `cargo run --bin seer` and `cargo run --bin seerw` run them.
- `cargo test` runs the tests. Test images are generated by the tests through the Windows Imaging Component, never checked in. The end-to-end tests in `tests/` drive headless viewers through `seer.exe`, each on an instance of its own, so they never touch the desktop or a viewer already running; AGENTS.md describes the technique.
- `cargo llvm-cov --all-targets --ignore-filename-regex 'src[\\/]bin[\\/]' --fail-under-lines 75` runs the tests and fails if they cover less than 75% of lines. CI runs this check.
- `cargo fmt` formats the code, and `cargo clippy --all-targets -- -D warnings` lints it.
- `cargo build --release` then `cargo run --release --example bench` measures the cold start: it generates a 12-megapixel JPEG, starts the release `seerw.exe` on it twenty times, and prints the median and worst time from process creation to the first frame presented and to the window being shown, with the median of each phase between. `--headless` leaves the window hidden and `--runs <n>` changes the count. The target is a median under 150 ms on a mid-range desktop. `seerw --exit-after-first-frame` is what makes this possible: it prints the start-up timings as JSON and exits once the first frame is presented.

The first frame is the part of start-up that matters most, and the `seer::viewer` module documentation describes how start-up is arranged to reach it quickly.

CI runs on pull requests and on demand, on Windows.

## Licence

Seer is released under the [MIT licence](LICENSE).
