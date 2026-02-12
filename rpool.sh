# rpool shell integration
# Source this file in your .bashrc or .zshrc:
#   source /path/to/rpool/rpool.sh

# rp - wrapper for rpool that handles cd and build
rp() {
    # Parse args, extracting the subcommand while preserving order
    local args=()
    local subcmd=""
    while [[ $# -gt 0 ]]; do
        case "$1" in
            -p|--pool) args+=("$1" "$2"); shift 2 ;;
            -*) args+=("$1"); shift ;;
            *)
                if [[ -z "$subcmd" ]]; then
                    subcmd="$1"
                fi
                args+=("$1"); shift ;;
        esac
    done

    # Commands whose stdout is a path to cd into
    if [[ "$subcmd" == "ck" || "$subcmd" == "checkout" || "$subcmd" == "pr" || "$subcmd" == "new" || "$subcmd" == "cd" ]]; then
        local output
        output=$(rpool "${args[@]}")
        local ret=$?
        if [[ $ret -eq 0 && -d "$output" ]]; then
            cd "$output"
            # Auto-build after checkout/pr (not cd/new)
            if [[ "$subcmd" == "ck" || "$subcmd" == "checkout" || "$subcmd" == "pr" ]]; then
                rpool build
            fi
        fi
        return $ret
    else
        rpool "${args[@]}"
    fi
}

# Dynamic completions from the rpool binary
eval "$(rpool completions bash 2>/dev/null)"
