def _harnx_nushell [] {
    let _prev = (commandline)
    if ($_prev != "") {
        print '⌛'
        commandline edit -r (harnx -e $_prev)
    }
}

$env.config.keybindings = ($env.config.keybindings | append {
        name: harnx_integration
        modifier: alt
        keycode: char_e
        mode: [emacs, vi_insert]
        event:[
            {
                send: executehostcommand,
                cmd: "_harnx_nushell"
            }
        ]
    }
)