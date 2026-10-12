# Vivido shell integration for fish 3.
#
# Reports the prompt and command lifecycle with OSC 133 (A prompt, C command, D finish) and the
# working directory with OSC 7, so Vivido can wait for prompts, time commands, and open new
# terminals where you are. fish 4 reports both itself, so there this file only tidies up.
#
# Vivido puts this directory first in XDG_DATA_DIRS, and fish runs every vendor_conf.d file it
# finds there. To load the file by hand instead, add this to ~/.config/fish/config.fish:
#
#   set -q VIVIDO_SHELL_INTEGRATION_DIR; and source $VIVIDO_SHELL_INTEGRATION_DIR/fish/vendor_conf.d/vivido-shell-integration.fish

# Give XDG_DATA_DIRS back before anything this shell starts can see Vivido's entry.
if set -q VIVIDO_FISH_INJECT
    set -e VIVIDO_FISH_INJECT
    if set -q VIVIDO_FISH_XDG_DATA_DIRS
        set -gx XDG_DATA_DIRS $VIVIDO_FISH_XDG_DATA_DIRS
        set -e VIVIDO_FISH_XDG_DATA_DIRS
    else
        set -e XDG_DATA_DIRS
    end
end

status is-interactive; or exit
set -q __vivido_hooked; and exit
test (string match -r '^\d+' -- $version) -ge 4; and exit
set -g __vivido_hooked 1

function __vivido_report_cwd --on-variable PWD
    status is-command-substitution; and return
    printf '\e]7;file://%s%s\a' $hostname (string escape --style=url -- $PWD)
end

function __vivido_mark_prompt --on-event fish_prompt
    printf '\e]133;A\a'
end

function __vivido_mark_command --on-event fish_preexec
    printf '\e]133;C\a'
end

function __vivido_mark_finish --on-event fish_postexec
    printf '\e]133;D;%s\a' $status
end

__vivido_report_cwd
