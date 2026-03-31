function _harnx_fish
    set -l _old (commandline)
    if test -n $_old
        echo -n "⌛"
        commandline -f repaint
        commandline (harnx -e $_old)
    end
end
bind \ee _harnx_fish