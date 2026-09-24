# Robust terminal-session feature demo for the README.
# It shows the explicit exit choice before cancelling it, then cleans up safely.
Output assets/features/terminal-sessions.rendered.gif
Require vhs
Require ttyd
Require ffmpeg
Require ffprobe
Set Shell "bash"
Set FontSize 11
Set Width 480
Set Height 300
Set Padding 8
Set Theme "Catppuccin Mocha"
Set TypingSpeed 35ms
Set Framerate 24

Hide
Type `set -euo pipefail; repo="$PWD"; fixture="$(bash "$repo/tapes/features/fixture.sh" terminal-sessions)"; readonly repo fixture; trap 'test -f "$fixture/.excise-vhs-owned" && rm -rf -- "$fixture"' EXIT; cd "$fixture"; unset EXCISE_CONFIG EXCISE_ROOT EXCISE_SCAN_THREADS EXCISE_EVENT_BUFFER EXCISE_APPARENT_SIZE EXCISE_CROSS_FILESYSTEMS EXCISE_EXCLUDE EXCISE_MEMORY_MIB EXCISE_TEMPORARY_STORAGE_MIB EXCISE_SCAN_STORE_MIB EXCISE_SCAN_STORE_RESERVE_MIB EXCISE_SCAN_STORE_DIR EXCISE_REDUCED_MOTION EXCISE_THEME EXCISE_ASCII EXCISE_MOUSE EXCISE_KEYMAP EXCISE_FORMAT EXCISE_OUTPUT EXCISE_PTY_TEST_MARKERS NO_COLOR; export COLORTERM=truecolor TERM=xterm-256color`
Enter
Type `clear`
Enter
Type `test -f "$fixture/.excise-vhs-owned" && "$repo/target/release/excise" --config "$fixture/config.toml" --scan-threads 1 "$fixture"`
Enter
Wait+Screen /COMPLETE/
Show
Sleep 700ms
Type "q"
Wait+Screen /Quit Excise\?/
Sleep 1500ms
Escape
Wait+Screen /COMPLETE/
Sleep 900ms
Hide
Type "q"
Wait+Screen /Quit Excise\?/
Type "y"
Sleep 300ms
Type "exit"
Enter
