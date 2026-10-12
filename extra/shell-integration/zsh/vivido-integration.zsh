# Vivido shell integration for zsh.
#
# Reports the prompt and command lifecycle with OSC 133 (A prompt, B input, C command, D finish)
# and the working directory with OSC 7, so Vivido can wait for prompts, time commands, and open
# new terminals where you are.
#
# Vivido loads this file itself through its own .zshenv. To load it by hand instead, for example
# in a zsh that Vivido did not start, add this to ~/.zshrc:
#
#   [[ -n $VIVIDO_SHELL_INTEGRATION_DIR ]] && source "$VIVIDO_SHELL_INTEGRATION_DIR/zsh/vivido-integration.zsh"

[[ -o interactive ]] || builtin return 0
(( ${+_vivido_state} )) && builtin return 0

# 1 while a command runs: set by preexec, so an empty line reports no finish.
builtin typeset -gi _vivido_state=0
builtin typeset -g _vivido_cwd=''
builtin typeset -g _vivido_mark_b=$'%{\e]133;B\a%}'

_vivido_report_cwd() {
    builtin emulate -L zsh -o extended_glob -o no_multibyte
    [[ $PWD == "$_vivido_cwd" ]] && builtin return 0
    _vivido_cwd=$PWD
    # Without multibyte, each character is one byte of the UTF-8 path.
    builtin local encoded="${PWD//(#m)[^A-Za-z0-9\/._~-]/%${(l:2::0:)$(( [##16] #MATCH ))}}"
    builtin print -rn -- $'\e]7;file://'"$HOST$encoded"$'\a'
}

_vivido_precmd() {
    # Every precmd function sees the command's status; read it and the user's prompt options
    # before emulate replaces them.
    builtin local -i ret=$?
    builtin local percent=0
    [[ -o prompt_percent ]] && percent=1
    builtin emulate -L zsh
    if (( _vivido_state )); then
        builtin print -rn -- $'\e]133;D;'"$ret"$'\a'
        _vivido_state=0
    fi
    builtin print -rn -- $'\e]133;A\a'
    _vivido_report_cwd
    # Stay the last precmd function so the input mark follows whatever themes did to PS1.
    if [[ ${precmd_functions[-1]} != _vivido_precmd ]]; then
        precmd_functions=(${precmd_functions:#_vivido_precmd} _vivido_precmd)
    fi
    if (( percent )) && [[ $PS1 != *"$_vivido_mark_b" ]]; then
        # A trailing lone % would swallow the mark's %{ as a literal.
        [[ $PS1 == *[^%]% || $PS1 == % ]] && PS1+=%
        PS1+=$_vivido_mark_b
    fi
}

_vivido_preexec() {
    builtin print -rn -- $'\e]133;C\a'
    _vivido_state=1
}

# Hook in at the first prompt, after .zshrc and its themes and plugins have set theirs up.
_vivido_install() {
    builtin emulate -L zsh
    precmd_functions=(${precmd_functions:#_vivido_install} _vivido_precmd)
    builtin typeset -ga preexec_functions chpwd_functions
    preexec_functions+=(_vivido_preexec)
    chpwd_functions+=(_vivido_report_cwd)
    builtin unfunction _vivido_install
    # zsh does not call a function added to the list it is walking; mark this prompt now. A hook
    # from .zshrc that runs after this one and rebuilds PS1 drops the first input mark, but A
    # already reports the prompt, and every later prompt carries both.
    _vivido_precmd
}
builtin typeset -ga precmd_functions
precmd_functions+=(_vivido_install)
