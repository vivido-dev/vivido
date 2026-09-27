# Vivido shell integration: the first startup file of a zsh that Vivido started.
#
# Vivido points ZDOTDIR here so that zsh reads this file instead of the user's .zshenv. It puts
# ZDOTDIR back at once, so every later startup file comes from the usual place, runs the user's
# .zshenv, and then loads the integration into interactive shells.

if [[ -n ${VIVIDO_ZSH_ZDOTDIR+set} ]]; then
    builtin export ZDOTDIR="$VIVIDO_ZSH_ZDOTDIR"
    builtin unset VIVIDO_ZSH_ZDOTDIR
else
    builtin unset ZDOTDIR
fi

{
    # zsh reads $HOME/.zshenv when ZDOTDIR is unset, skips unreadable files, and so do we.
    builtin typeset _vivido_file="${ZDOTDIR-$HOME}/.zshenv"
    [[ -r $_vivido_file && ! -d $_vivido_file ]] && builtin source -- "$_vivido_file"
} always {
    if [[ -o interactive ]]; then
        # %x is this file, :A resolves it, and :h is its directory.
        _vivido_file="${${(%):-%x}:A:h}/vivido-integration.zsh"
        [[ -r $_vivido_file ]] && builtin source -- "$_vivido_file"
    fi
    builtin unset _vivido_file
}
