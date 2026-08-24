# Deterministic hero demo for the README (-> assets/demo.gif).
#
# The fixture and its explicit TUI configuration are created in guarded
# temporary paths. Excise is pointed only at the generated fixture and the
# tape never enters deletion mode or sends a deletion confirmation. Cleanup is
# limited to those generated paths.
#
# Regenerate from the repository root after building the pinned binary:
#   cargo +1.88.0 build --release --locked && vhs tapes/demo.tape
# Requires vhs, ttyd, and ffmpeg on PATH.

Output assets/demo.gif

Require vhs
Require ttyd
Require ffmpeg

Set Shell "bash"
Set FontSize 16
Set Width 1100
Set Height 640
Set Padding 15
Set Theme "Catppuccin Mocha"
Set TypingSpeed 45ms
Set Framerate 24

# Build a disposable, deterministic fixture and launch the release binary.
Hide
Type `set -euo pipefail; umask 077; fixture="/tmp/excise-vhs-fixture"; if [[ -e "$fixture" || -L "$fixture" ]]; then printf 'refusing existing fixture path: %s\n' "$fixture" >&2; exit 1; fi; mkdir -m 700 "$fixture"; config="${fixture}/config.toml"; readonly fixture config; touch "$fixture/.excise-vhs-owned"; trap 'test -f "$fixture/.excise-vhs-owned" && rm -rf -- "$fixture"' EXIT; mkdir -p "$fixture/projects" "$fixture/cache"; printf 'release notes\n' > "$fixture/projects/README.txt"; printf 'fixture source\n' > "$fixture/projects/main.rs"; printf 'cache entry\n' > "$fixture/cache/index.txt"; printf 'version = 1\n[runtime]\nformat = "tui"\n' > "$config"`
Enter
Type `unset EXCISE_CONFIG EXCISE_ROOT EXCISE_SCAN_THREADS EXCISE_EVENT_BUFFER EXCISE_APPARENT_SIZE EXCISE_CROSS_FILESYSTEMS EXCISE_EXCLUDE EXCISE_MEMORY_MIB EXCISE_REDUCED_MOTION EXCISE_THEME EXCISE_ASCII EXCISE_MOUSE EXCISE_KEYMAP EXCISE_FORMAT EXCISE_OUTPUT EXCISE_PTY_TEST_MARKERS NO_COLOR; export COLORTERM=truecolor; export TERM=xterm-256color`
Enter
Type `clear`
Enter
Type `test -f "$fixture/.excise-vhs-owned" && ./target/release/excise --config "$config" --apparent-size --reduced-motion --scan-threads 1 "$fixture"`
Enter
Wait+Screen /COMPLETE/
Sleep 500ms
Show

# Let the treemap and safety-aware status settle before light navigation.
Sleep 1.5s
Down@350ms 2
Sleep 900ms
Right@250ms 4
Sleep 900ms
Type "+"
Sleep 900ms
Type "0"
Sleep 1.2s

# Quit inside a hidden block so the shell and cleanup are never recorded.
Hide
Type "q"
Sleep 500ms
Type "y"
Sleep 700ms
Type "exit"
Enter
Sleep 500ms
