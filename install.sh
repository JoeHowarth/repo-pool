#!/usr/bin/env bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SHELL_INTEGRATION="$SCRIPT_DIR/rpool.sh"

echo "Building rpool..."
cargo build --release --manifest-path="$SCRIPT_DIR/Cargo.toml"

echo "Installing binary to ~/.cargo/bin..."
mkdir -p ~/.cargo/bin
cp "$SCRIPT_DIR/target/release/rpool" ~/.cargo/bin/

# Detect shell config file
if [[ -n "$ZSH_VERSION" ]] || [[ "$SHELL" == */zsh ]]; then
    SHELL_RC="$HOME/.zshrc"
else
    SHELL_RC="$HOME/.bashrc"
fi

# Check if already sourced
SOURCE_LINE="source \"$SHELL_INTEGRATION\""
if grep -qF "rpool.sh" "$SHELL_RC" 2>/dev/null; then
    echo "Shell integration already configured in $SHELL_RC"
else
    echo ""
    echo "Add shell integration to $SHELL_RC? This enables:"
    echo "  - 'rp' command wrapper (auto cd + build)"
    echo "  - Tab completion"
    echo ""
    read -p "Add to $SHELL_RC? [Y/n] " -n 1 -r
    echo ""
    if [[ ! $REPLY =~ ^[Nn]$ ]]; then
        echo "" >> "$SHELL_RC"
        echo "# rpool - repository pool manager" >> "$SHELL_RC"
        echo "$SOURCE_LINE" >> "$SHELL_RC"
        echo "Added to $SHELL_RC"
    else
        echo "Skipped. To add manually, add this line to your shell rc:"
        echo "  $SOURCE_LINE"
    fi
fi

echo ""
echo "Installation complete!"
echo ""
echo "Reload your shell or run:"
echo "  source $SHELL_RC"
echo ""
echo "Then initialize a pool in your repo directory:"
echo "  cd /path/to/your/repo"
echo "  rpool init"
echo ""
echo "Usage:"
echo "  rp ck <branch>    # checkout branch, cd, build"
echo "  rp pr <number>    # checkout PR by number"
echo "  rp st             # show pool status"
echo "  rp new            # add new clone to pool"
echo "  rp drop           # unassign current clone"
echo "  rp sync           # fetch all clones"
