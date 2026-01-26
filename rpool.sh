# rpool shell integration
# Source this file in your .bashrc or .zshrc:
#   source /path/to/rpool/rpool.sh

# rp - wrapper for rpool that handles cd and build
rp() {
    if [[ "$1" == "ck" || "$1" == "checkout" || "$1" == "pr" || "$1" == "new" ]]; then
        local output
        output=$(rpool "$@")
        local ret=$?
        if [[ $ret -eq 0 && -d "$output" ]]; then
            cd "$output"
            if [[ "$1" == "ck" || "$1" == "checkout" || "$1" == "pr" ]]; then
                rpool build
            fi
        fi
        return $ret
    else
        rpool "$@"
    fi
}

# Completions for bash
if [[ -n "$BASH_VERSION" ]]; then
    _rp_completions() {
        local cur="${COMP_WORDS[COMP_CWORD]}"
        local prev="${COMP_WORDS[COMP_CWORD-1]}"

        if [[ $COMP_CWORD -eq 1 ]]; then
            COMPREPLY=($(compgen -W "checkout ck status st drop pr sync new pools rm-pool init build" -- "$cur"))
        fi
    }
    complete -F _rp_completions rp
    complete -F _rp_completions rpool
fi
