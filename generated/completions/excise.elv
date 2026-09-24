
use builtin;
use str;

set edit:completion:arg-completer[excise] = {|@words|
    fn spaces {|n|
        builtin:repeat $n ' ' | str:join ''
    }
    fn cand {|text desc|
        edit:complex-candidate $text &display=$text' '(spaces (- 14 (wcswidth $text)))$desc
    }
    var command = 'excise'
    for word $words[1..-1] {
        if (str:has-prefix $word '-') {
            break
        }
        set command = $command';'$word
    }
    var completions = [
        &'excise'= {
            cand --config 'Read configuration from FILE'
            cand --scan-threads 'Scanner worker count (1-32)'
            cand --event-buffer 'Maximum queued worker events (16 to 4096)'
            cand --exclude 'Ordered gitignore-style exclusion pattern'
            cand --memory-mib 'Whole-process memory limit in MiB'
            cand --temporary-storage-mib 'Directory-plan and deletion-result storage limit per session (at least 2 MiB)'
            cand --scan-store-mib 'Private scan data and page-index storage limit per session (at least 2 MiB, capped by scratch-volume free space)'
            cand --scan-store-dir 'Parent directory for the private scan-storage session'
            cand --scan-store-reserve-mib 'Scratch space kept outside scan-storage files (defaults to 25 percent of free space)'
            cand --theme 'Built-in color theme'
            cand --keymap 'Keyboard preset. Arrows and safety keys always work'
            cand --format 'Output mode. Table and JSON modes do not require a terminal'
            cand --output 'Write a noninteractive report to FILE instead of stdout'
            cand -a 'Show apparent file sizes instead of allocated bytes'
            cand --apparent-size 'Show apparent file sizes instead of allocated bytes'
            cand --cross-filesystems 'Permit traversal across filesystem boundaries'
            cand --reduced-motion 'Disable nonessential motion'
            cand --ascii 'Use ASCII-only symbols and borders'
            cand --mouse 'Enable mouse capture and selection'
            cand -d 'Do not ask for confirmation before deleting'
            cand --disable-delete-confirmation 'Do not ask for confirmation before deleting'
            cand -h 'Print help'
            cand --help 'Print help'
            cand -V 'Print version'
            cand --version 'Print version'
        }
    ]
    $completions[$command]
}
