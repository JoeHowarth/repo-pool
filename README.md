# rpool

Manage a pool of repository clones for faster branch switching.

## Why?

For repositories with large submodules and expensive builds, git worktrees don't help much - each worktree needs its own submodule checkout and build artifacts. `rpool` maintains a pool of full clones that preserve their build state, making branch switches fast via incremental compilation.

## Installation

```bash
git clone <repo-url> ~/tools/rpool
cd ~/tools/rpool
./install.sh
```

This will:
- Build and install the `rpool` binary to `~/.cargo/bin`
- Add shell integration to your `.bashrc` or `.zshrc`

## Setup

Initialize a pool from within your repository:

```bash
cd /path/to/your/repo
rpool init
```

This creates a pool configuration and detects existing clones in the parent directory.

### Options

```bash
rpool init --build-cmd "make"           # custom build command
rpool init --build-cmd "npm run build"  # for JS projects
rpool init --base ~/projects            # custom base directory
rpool init --name mypool                # custom pool name
```

Add more clones to the pool:

```bash
rp new                    # auto-named clone
rp new my-feature-clone   # named clone
```

## Usage

```bash
rp ck <branch>      # checkout branch (assigns clone, cd's, builds)
rp pr <number>      # checkout PR by number
rp st               # show pool status
rp drop             # unassign current clone for reuse
rp sync             # fetch all clones
rp pools            # list configured pools
rp build            # run configured build command
```

## How it works

1. `rpool init` registers the current repo's parent directory as a pool
2. Clones in that directory with matching remotes are tracked
3. `rp ck <branch>` finds an available clone (unassigned or LRU), checks out the branch, updates submodules, and runs the configured build command
4. Clones stay assigned to branches until explicitly dropped
5. If all clones are assigned, the least recently used one is reassigned

## Dev config files

On checkout, rpool automatically copies these files from another clone in the pool (useful for gitignored dev configs):

- `AGENTS.md`
- `CLAUDE.md`
- `.cargo/` directory

## Files

- `~/.config/rpool/config.json` - pool configurations
- `~/.config/rpool/state.json` - clone assignments and timestamps
