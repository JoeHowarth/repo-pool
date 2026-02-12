# rpool

Manage a pool of repository clones for faster branch switching.

## Why?

For repositories with large submodules and expensive builds, git worktrees don't help much - each worktree needs its own submodule checkout and build artifacts. `rpool` maintains a pool of full clones that preserve their build state, making branch switches fast via incremental compilation.

## Usage

```bash
rp ck <branch>        # checkout branch (assigns clone, cd's, builds)
rp pr <number>        # checkout PR by number
rp cd <clone>         # cd to a clone by name (works across all pools)
rp st                 # show pool status
rp drop               # unassign current clone for reuse
rp sync               # fetch all clones, refresh branch cache
rp pools              # list configured pools
rp build              # run configured build command
rp -p <pool> <cmd>    # operate on a specific pool from anywhere
```

### Multi-pool

Use `-p` to target a specific pool without needing to cd:

```bash
rp -p my-pool st          # status of a specific pool
rp -p my-pool ck feature  # checkout in a specific pool
```

### Migrating existing clones

If you have clones scattered across your home directory from before the `~/rppool/` structure:

```bash
rpool migrate                           # moves clones into ~/rppool/<pool>/
rpool migrate --clones-root ~/my-pools  # custom root
rpool migrate --keep main-clone         # keep a specific clone in place
```

The clone matching the pool name (e.g. `monad-bft` in the `monad-bft` pool) is kept in place by default.

## How it works

1. `rpool init` registers a pool and creates a directory under `~/rppool/<pool-name>/`
2. Clones in that directory with matching remotes are tracked
3. `rp ck <branch>` finds an available clone (unassigned or LRU), checks out the branch, updates submodules, and runs the configured build command
4. Clones stay assigned to branches until explicitly dropped
5. If all clones are assigned, the least recently used one is reassigned

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

Or from anywhere, specifying the repo URL:

```bash
rpool init --repo git@github.com:org/repo.git --name my-pool
```

Clones are stored under `~/rppool/<pool-name>/` by default.

### Options

```bash
rpool init --build-cmd "make"             # custom build command
rpool init --build-cmd "npm run build"    # for JS projects
rpool init --clones-root ~/my-pools       # custom root directory
rpool init --name mypool                  # custom pool name
```

Add more clones to the pool:

```bash
rp new                    # auto-named clone
rp new my-feature-clone   # named clone
```

## Tab completion

Tab completion is set up automatically via the shell integration. It provides dynamic completions for:

- Subcommands and flags
- Pool names (after `-p`)
- Clone names (for `cd`, `drop`)
- Branch names (for `checkout` -- from cached remote branches)

Run `rp sync` to refresh the branch cache after new branches are pushed upstream.

## Dev config files

On checkout, rpool automatically copies these files from another clone in the pool (useful for gitignored dev configs):

- `AGENTS.md`
- `CLAUDE.md`
- `.cargo/` directory

## Files

- `~/.config/rpool/config.json` - pool configurations
- `~/.config/rpool/state.json` - clone assignments and timestamps
- `~/.config/rpool/branch_cache.json` - cached remote branch names (updated by `sync`)
