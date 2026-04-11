_harnx() {
    local cur prev words cword i opts cmd
    COMPREPLY=()

    _get_comp_words_by_ref -n : cur prev words cword

    for i in ${words[@]}
    do
        case "${cmd},${i}" in
            ",$1")
                cmd="harnx"
                ;;
            *)
                ;;
        esac
    done

    case "${cmd}" in
        harnx)
            opts="-m -s -a -f -S -t -h -V --model --prompt --session --empty-session --save-session --agent --agent-variable --rag --rebuild-rag --macro --serve --acp --file --no-stream --dry-run --info --sync-models --list-models --list-sessions --list-agents --list-rags --list-macros --mcp-root --tool --help --version"
            if [[ ${cur} == -* || ${cword} -eq 1 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi

            case "${prev}" in
                -m|--model)
                    COMPREPLY=($(compgen -W "$("$1" --list-models)" -- "${cur}"))
                    __ltrim_colon_completions "$cur"
                    return 0
                    ;;
                --prompt)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -s|--session)
                    COMPREPLY=($(compgen -W "$("$1" --list-sessions)" -- "${cur}"))
                    __ltrim_colon_completions "$cur"
                    return 0
                    ;;
                -a|--agent)
                    COMPREPLY=($(compgen -W "$("$1" --list-agents)" -- "${cur}"))
                    __ltrim_colon_completions "$cur"
                    return 0
                    ;;
                --rag)
                    COMPREPLY=($(compgen -W "$("$1" --list-rags)" -- "${cur}"))
                    __ltrim_colon_completions "$cur"
                    return 0
                    ;;
                --macro)
                    COMPREPLY=($(compgen -W "$("$1" --list-macros)" -- "${cur}"))
                    __ltrim_colon_completions "$cur"
                    return 0
                    ;;
                --serve)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --acp)
                    COMPREPLY=($(compgen -W "$("$1" --list-agents)" -- "${cur}"))
                    __ltrim_colon_completions "$cur"
                    return 0
                    ;;
                -f|--file)
                    local oldifs
                    if [[ -v IFS ]]; then
                        oldifs="$IFS"
                    fi
                    IFS=$'\n'
                    COMPREPLY=($(compgen -f "${cur}"))
                    if [[ -v oldifs ]]; then
                        IFS="$oldifs"
                    fi
                    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
                        compopt -o filenames
                    fi
                    return 0
                    ;;
                --mcp-root)
                    local oldifs
                    if [[ -v IFS ]]; then
                        oldifs="$IFS"
                    fi
                    IFS=$'\n'
                    COMPREPLY=($(compgen -d "${cur}"))
                    if [[ -v oldifs ]]; then
                        IFS="$oldifs"
                    fi
                    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
                        compopt -o filenames
                    fi
                    return 0
                    ;;
                -t|--tool)
                    # Tools can be tool names or toolset names - no dynamic completion available
                    COMPREPLY=()
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
    complete -F _harnx -o nosort -o bashdefault -o default harnx
else
    complete -F _harnx -o bashdefault -o default harnx
fi