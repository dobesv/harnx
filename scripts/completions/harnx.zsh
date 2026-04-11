#compdef harnx

autoload -U is-at-least

_harnx() {
    typeset -A opt_args
    typeset -a _arguments_options
    local ret=1

    if is-at-least 5.2; then
        _arguments_options=(-s -S -C)
    else
        _arguments_options=(-s -C)
    fi

    local context curcontext="$curcontext" state line
    local common=(
'-m[Select a LLM model]:MODEL:->models' \
'--model[Select a LLM model]:MODEL:->models' \
'--prompt[Use the system prompt]:PROMPT: ' \
'-s[Start or join a session]:SESSION:->sessions' \
'--session[Start or join a session]:SESSION:->sessions' \
'--empty-session[Ensure the session is empty]' \
'--save-session[Ensure the new conversation is saved to the session]' \
'-a[Start a agent]:AGENT:->agents' \
'--agent[Start a agent]:AGENT:->agents' \
'--agent-variable[Set agent variables]: : ' \
'--rag[Start a RAG]:RAG:->rags' \
'--rebuild-rag[Rebuild the RAG to sync document changes]' \
'--macro[Execute a macro]:MACRO:->macros' \
'--serve[Serve the LLM API and WebAPP]:ADDRESS: ' \
'--acp[Serve as an ACP agent over stdio]:AGENT:->agents' \
'*-f[Include files, directories, or URLs]:FILE:_files' \
'*--file[Include files, directories, or URLs]:FILE:_files' \
'-S[Turn off stream mode]' \
'--no-stream[Turn off stream mode]' \
'--dry-run[Display the message without sending it]' \
'--info[Display information]' \
'--sync-models[Sync models updates]' \
'--list-models[List all available chat models]' \
'--list-sessions[List all sessions]' \
'--list-agents[List all agents]' \
'--list-rags[List all RAGs]' \
'--list-macros[List all macros]' \
'*--mcp-root[Add MCP roots]:PATH:_directories' \
'-t[Enable tools or toolsets for this session]:TOOL: ' \
'--tool[Enable tools or toolsets for this session]:TOOL: ' \
'-h[Print help]' \
'--help[Print help]' \
'-V[Print version]' \
'--version[Print version]' \
'*::text -- Input text:' \
    )


    _arguments "${_arguments_options[@]}" $common \
        && ret=0 
    case $state in
        models|sessions|agents|rags|macros)
            local -a values expl
            values=( ${(f)"$(_call_program values harnx --list-$state)"} )
            _wanted values expl $state compadd -a values && ret=0
            ;;
    esac
    return ret
}

(( $+functions[_harnx_commands] )) ||
_harnx_commands() {
    local commands; commands=()
    _describe -t commands 'harnx commands' commands "$@"
}

if [ "$funcstack[1]" = "_harnx" ]; then
    _harnx "$@"
else
    compdef _harnx harnx
fi