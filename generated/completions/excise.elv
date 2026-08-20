
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
            cand --event-buffer 'Bounded worker-event capacity (16-4096)'
            cand --exclude 'Ordered gitignore-style exclusion pattern'
            cand --memory-mib 'Whole-process memory envelope in MiB'
            cand --theme 'Built-in semantic color theme'
            cand --keymap 'Keyboard preset; arrows and safety keys always work'
            cand --format 'Output mode; table and JSON never acquire a terminal'
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
