_harnx_bash() {
    if [[ -n "$READLINE_LINE" ]]; then
        READLINE_LINE=$(harnx -e "$READLINE_LINE")
        READLINE_POINT=${#READLINE_LINE}
    fi
}
bind -x '"\ee": _harnx_bash'