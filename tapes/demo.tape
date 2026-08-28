# Deterministic current-main hero demo for the README. It writes assets/demo-main.gif.
#
# The fixture and its explicit terminal interface configuration are created in guarded
# temporary paths. Excise is pointed only at the generated fixture and the
# tape never enters deletion mode or sends a deletion confirmation. Cleanup is
# limited to those generated paths.
#
# Regenerate from the repository root after building the pinned binary:
#   cargo +1.88.0 build --release --locked && cargo demo
# Requires vhs, ttyd, ffmpeg, ffprobe, and gifsicle on PATH.
#
# `vhs tapes/demo.tape` alone renders a correct but much heavier GIF. The xtask
# adds the palette and quantisation pass that makes the asset publishable.
#
# The geometry below matches the width the README displays, so the published
# recording is never resampled. Fixture sizes span a decade so the map's colour
# band is legible in the first frame.

Output assets/demo-main.gif

Require vhs
Require ttyd
Require ffmpeg
Require ffprobe

Set Shell "bash"
Set FontSize 13
Set Width 900
Set Height 524
Set Padding 12
Set Theme "Catppuccin Mocha"
Set TypingSpeed 45ms
Set Framerate 24

# Build a disposable, deterministic fixture and launch the release binary.
Hide
Type `set -euo pipefail; umask 077; fixture="/tmp/excise-vhs-fixture"; if [[ -e "$fixture" || -L "$fixture" ]]; then printf 'refusing existing fixture path: %s\n' "$fixture" >&2; exit 1; fi; mkdir -m 700 "$fixture"; config="${fixture}/config.toml"; readonly fixture config; touch "$fixture/.excise-vhs-owned"; trap 'test -f "$fixture/.excise-vhs-owned" && rm -rf -- "$fixture"' EXIT; mkdir -p "$fixture/media" "$fixture/backups" "$fixture/cache" "$fixture/downloads" "$fixture/projects/src"; head -c 6291456 /dev/zero > "$fixture/media/capture.mov"; head -c 3355443 /dev/zero > "$fixture/backups/snapshot.tar"; head -c 2097152 /dev/zero > "$fixture/cache/index.bin"; head -c 1258291 /dev/zero > "$fixture/downloads/toolchain.pkg"; head -c 786432 /dev/zero > "$fixture/projects/target.bin"; head -c 262144 /dev/zero > "$fixture/projects/src/main.rs"; head -c 98304 /dev/zero > "$fixture/projects/src/lib.rs"; head -c 340992 /dev/zero > "$fixture/notes.md"; printf 'version = 1\n[runtime]\nformat = "tui"\n' > "$config"`
Enter
Type `unset EXCISE_CONFIG EXCISE_ROOT EXCISE_SCAN_THREADS EXCISE_EVENT_BUFFER EXCISE_APPARENT_SIZE EXCISE_CROSS_FILESYSTEMS EXCISE_EXCLUDE EXCISE_MEMORY_MIB EXCISE_REDUCED_MOTION EXCISE_THEME EXCISE_ASCII EXCISE_MOUSE EXCISE_KEYMAP EXCISE_FORMAT EXCISE_OUTPUT EXCISE_PTY_TEST_MARKERS NO_COLOR; export COLORTERM=truecolor; export TERM=xterm-256color`
Enter
Type `clear`
Enter
Type `test -f "$fixture/.excise-vhs-owned" && ./target/release/excise --config "$config" --apparent-size --scan-threads 1 "$fixture"`
Enter
Wait+Screen /COMPLETE/
Sleep 500ms
Show

# The map arms its own cursor on the largest entry, so the demo starts by
# moving it onto a folder worth opening rather than by creating a selection.
Sleep 1.0s
Right@350ms 1
Sleep 900ms

# The movement worth showing: opening an entry zooms the map into it, and
# leaving contracts the folder back into the rectangle it came from.
#
# Every state this tape is meant to show is asserted with Wait+Screen. VHS
# injects keystrokes through a browser terminal and drops one occasionally. A
# silent drop would publish a recording missing the state it exists to show.
# The assertions turn that into a failed render instead.
Enter
Wait+Screen /snapshot\.tar/
Sleep 1.4s
Escape
Wait+Screen /media\//
Sleep 1.1s

# Each zoom drops the largest remaining fixture entry. In apparent-size mode,
# the resulting `backups/` and `cache/` details are unique viewport states.
Type "+"
Wait+Screen /3\.2M · 41%/
Sleep 900ms
Type "+"
Wait+Screen /2\.0M · 43%/
Sleep 700ms
Type "0"
Wait+Screen /media\//
Sleep 900ms
Right@350ms 1
Wait+Screen /apparent 3\.2M · entries 1/
Sleep 1.1s

# Surface the contextual command help before the final hidden exit.
Type "?"
Wait+Screen /EXCISE COMMANDS/
Sleep 1.1s
Type "?"
Sleep 600ms

# Quit inside a hidden block so the shell and cleanup are never recorded.
Hide
Type "q"
Wait+Screen /Quit Excise\?/
Sleep 400ms
Type "y"
Sleep 500ms
Type "exit"
Enter
Sleep 400ms
