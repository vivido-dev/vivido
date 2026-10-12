# Vivido shell integration for bash 4.4 or newer.
#
# Reports the prompt and command lifecycle with OSC 133 (A prompt, B input, C command, D finish)
# and the working directory with OSC 7, so Vivido can wait for prompts, time commands, and open
# new terminals where you are.
#
# Vivido loads this file itself: it starts bash in POSIX mode with ENV naming this file, and the
# file then runs the startup files bash would normally have read. To load it by hand instead, for
# example in a bash that Vivido did not start, put this at the top of ~/.bashrc:
#
#   [ -n "$VIVIDO_SHELL_INTEGRATION_DIR" ] && . "$VIVIDO_SHELL_INTEGRATION_DIR/bash/vivido.bash"

[[ $- == *i* ]] || builtin return 0

if [[ -n ${VIVIDO_BASH_INJECT-} ]]; then
    # Vivido started this shell with --posix so that bash would read ENV and nothing else. Undo
    # everything that took, then read the startup files in bash's own order (INVOCATION, bash(1)).
    __vivido_flags=$VIVIDO_BASH_INJECT
    builtin unset ENV VIVIDO_BASH_INJECT
    if [[ -n ${VIVIDO_BASH_ENV+set} ]]; then
        builtin export ENV=$VIVIDO_BASH_ENV
        builtin unset VIVIDO_BASH_ENV
    fi
    builtin set +o posix
    # POSIX mode turns this on and leaving POSIX mode does not turn it off.
    builtin shopt -u inherit_errexit 2>/dev/null
    # POSIX mode names ~/.sh_history as the default history file.
    if [[ -n ${VIVIDO_BASH_HISTFILE-} ]]; then
        HISTFILE=$HOME/.bash_history
        builtin unset VIVIDO_BASH_HISTFILE
    fi

    if builtin shopt -q login_shell; then
        if [[ $__vivido_flags != *--noprofile* ]]; then
            [[ -r /etc/profile ]] && builtin source /etc/profile
            for __vivido_file in "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.profile"; do
                if [[ -r $__vivido_file ]]; then
                    builtin source "$__vivido_file"
                    break
                fi
            done
        fi
    elif [[ $__vivido_flags != *--norc* ]]; then
        # The system-wide bashrc is a build option; these are the names distributions use for it.
        # macOS's /etc/bashrc belongs to Apple's bash, which never reads it for other builds.
        for __vivido_file in /etc/bash.bashrc /etc/bash/bashrc /etc/bashrc; do
            [[ $__vivido_file == /etc/bashrc && $OSTYPE == darwin* ]] && continue
            if [[ -r $__vivido_file ]]; then
                builtin source "$__vivido_file"
                break
            fi
        done
        __vivido_file=${VIVIDO_BASH_RCFILE:-$HOME/.bashrc}
        [[ -r $__vivido_file ]] && builtin source "$__vivido_file"
    fi
    builtin unset __vivido_file __vivido_flags VIVIDO_BASH_RCFILE
fi

# PS0, which marks the start of a command, arrived in bash 4.4.
if ((BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 4))); then
    builtin return 0
fi
[[ -z ${__vivido_hooked-} ]] || builtin return 0
__vivido_hooked=1

# Set by PS0 when a command runs, so an empty line does not report a finish.
__vivido_ran=
__vivido_cwd=
__vivido_ps1_mark='\[\e]133;B\a\]'
# The subscript of an array that is never set evaluates an assignment and expands to nothing.
__vivido_ps0_mark='\e]133;C\a${__vivido_unset[__vivido_ran=1]-}'

__vivido_report_cwd() {
    # Percent-encode byte by byte; the C locale makes each character one byte. Under `builtin`,
    # `local` is an ordinary command whose arguments split, so every expansion is quoted.
    builtin local LC_ALL=C path="$PWD" encoded='' char code i
    for ((i = 0; i < ${#path}; i++)); do
        char=${path:i:1}
        case $char in
            [A-Za-z0-9/._~-]) encoded+=$char ;;
            *)
                builtin printf -v code '%d' "'$char"
                builtin printf -v char '%%%02X' $((code & 255))
                encoded+=$char
                ;;
        esac
    done
    builtin printf '\e]7;file://%s%s\a' "$HOSTNAME" "$encoded"
}

__vivido_precmd() {
    # The status of the user's command: saved before earlier PROMPT_COMMAND work when that is a
    # string, or restored for this entry by bash when PROMPT_COMMAND is an array.
    builtin local status="${__vivido_status-$?}"
    builtin unset __vivido_status
    if [[ -n $__vivido_ran ]]; then
        builtin printf '\e]133;D;%s\a' "$status"
        __vivido_ran=
    fi
    builtin printf '\e]133;A\a'
    if [[ $PWD != "$__vivido_cwd" ]]; then
        __vivido_cwd=$PWD
        __vivido_report_cwd
    fi
    # Prompt hooks such as starship rebuild PS1 every time, so mark it again whenever it lost
    # the mark. Without promptvars PS0 cannot flag a command, and would print the flag verbatim.
    # PS0 is unset by default, which `set -u` would turn into an error.
    [[ ${PS1-} == *"$__vivido_ps1_mark" ]] || PS1+=$__vivido_ps1_mark
    if builtin shopt -q promptvars && [[ ${PS0-} != *"$__vivido_ps0_mark"* ]]; then
        PS0+=$__vivido_ps0_mark
    fi
}

__vivido_return() {
    builtin return "$1"
}

# Run last, after every hook that sets PS1, while still seeing the user's command status.
if [[ -z ${PROMPT_COMMAND[*]-} ]]; then
    PROMPT_COMMAND='__vivido_precmd 2>/dev/null'
elif [[ $(builtin declare -p PROMPT_COMMAND 2>/dev/null) == "declare -a"* ]]; then
    PROMPT_COMMAND+=('__vivido_precmd 2>/dev/null')
else
    PROMPT_COMMAND=$'__vivido_status=$?; __vivido_return "$__vivido_status" 2>/dev/null\n'"$PROMPT_COMMAND"$'\n__vivido_precmd 2>/dev/null'
fi
