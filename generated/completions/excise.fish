complete -c excise -l config -d 'Read configuration from FILE' -r -F
complete -c excise -l scan-threads -d 'Scanner worker count (1-32)' -r
complete -c excise -l event-buffer -d 'Bounded worker-event capacity (16-4096)' -r
complete -c excise -l exclude -d 'Ordered gitignore-style exclusion pattern' -r
complete -c excise -l memory-mib -d 'Whole-process memory envelope in MiB' -r
complete -c excise -l theme -d 'Built-in semantic color theme' -r -f -a "excise-dark\t''
excise-light\t''
high-contrast\t''
monochrome\t''
dracula\t''
tokyo-night\t''
catppuccin-mocha\t''
catppuccin-latte\t''
gruvbox-dark\t''
gruvbox-light\t''
nord\t''
solarized-dark\t''
solarized-light\t''
one-dark\t''
monokai\t''"
complete -c excise -l keymap -d 'Keyboard preset; arrows and safety keys always work' -r -f -a "vim\t''
custom\t''
emacs\t''"
complete -c excise -l format -d 'Output mode; table and JSON never acquire a terminal' -r -f -a "tui\t''
table\t''
json\t''"
complete -c excise -l output -d 'Write a noninteractive report to FILE instead of stdout' -r -F
complete -c excise -s a -l apparent-size -d 'Show apparent file sizes instead of allocated bytes'
complete -c excise -l cross-filesystems -d 'Permit traversal across filesystem boundaries'
complete -c excise -l reduced-motion -d 'Disable nonessential motion'
complete -c excise -l ascii -d 'Use ASCII-only symbols and borders'
complete -c excise -l mouse -d 'Enable mouse capture and selection'
complete -c excise -s d -l disable-delete-confirmation -d 'Do not ask for confirmation before deleting'
complete -c excise -s h -l help -d 'Print help'
complete -c excise -s V -l version -d 'Print version'
