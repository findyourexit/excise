_excise() {
    local i cur prev opts cmd
    COMPREPLY=()
    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
        cur="$2"
    else
        cur="${COMP_WORDS[COMP_CWORD]}"
    fi
    prev="$3"
    cmd=""
    opts=""

    for i in "${COMP_WORDS[@]:0:COMP_CWORD}"
    do
        case "${cmd},${i}" in
            ",$1")
                cmd="excise"
                ;;
            *)
                ;;
        esac
    done

    case "${cmd}" in
        excise)
            opts="-a -d -h -V --config --apparent-size --scan-threads --event-buffer --cross-filesystems --exclude --memory-mib --temporary-storage-mib --reduced-motion --theme --ascii --mouse --keymap --format --output --disable-delete-confirmation --help --version"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 1 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --scan-threads)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --event-buffer)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --exclude)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --memory-mib)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --temporary-storage-mib)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --theme)
                    COMPREPLY=($(compgen -W "excise-dark excise-light high-contrast monochrome dracula tokyo-night catppuccin-mocha catppuccin-latte gruvbox-dark gruvbox-light nord solarized-dark solarized-light one-dark monokai" -- "${cur}"))
                    return 0
                    ;;
                --keymap)
                    COMPREPLY=($(compgen -W "vim custom emacs" -- "${cur}"))
                    return 0
                    ;;
                --format)
                    COMPREPLY=($(compgen -W "tui table json" -- "${cur}"))
                    return 0
                    ;;
                --output)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
    esac
}

if [[ "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 || "${BASH_VERSINFO[0]}" -gt 4 ]]; then
    complete -F _excise -o nosort -o bashdefault -o default excise
else
    complete -F _excise -o bashdefault -o default excise
fi
