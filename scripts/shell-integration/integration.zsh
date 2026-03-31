_harnx_zsh() {
    if [[ -n "$BUFFER" ]]; then
        local _old=$BUFFER
        BUFFER+="⌛"
        zle -I && zle redisplay
        BUFFER=$(harnx -e "$_old")
        zle end-of-line
    fi
}
zle -N _harnx_zsh
bindkey '\ee' _harnx_zsh