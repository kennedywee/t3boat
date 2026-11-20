#!/usr/bin/env bash
# Runs on a Boat sandbox, as `user`. Idempotent, and restarts nothing: safe while agents work.
#   [T3BOAT_GH_UPLOAD_TOKEN=<token> [T3BOAT_GH_EVERYTHING=1]] gh-uploads.sh [all|upgrade|apply]
# `upgrade` is apt alone, `apply` the wrapper and the token: a disconnect runs only `apply`.
# T3BOAT_GH_EVERYTHING: the token is for all of gh and git, not only for uploads.
set -euo pipefail
mode="${1:-all}"
token="${T3BOAT_GH_UPLOAD_TOKEN:-}"
everything=0
[ -z "$token" ] || [ "${T3BOAT_GH_EVERYTHING:-}" != 1 ] || everything=1
boat_dir="$HOME/.t3boat"
mkdir -p "$boat_dir"
chmod 700 "$boat_dir"

# First, before anything below can fail.
if [ "$mode" != upgrade ]; then
  rm -f "$boat_dir/gh-upload-token.new"
  [ "$everything" = 1 ] || rm -f "$boat_dir/gh-everything"
  [ -n "$token" ] || rm -f "$boat_dir/gh-upload-token"
fi

# The image's gh predates `--attach`. Only GitHub's apt repo is refreshed.
gh_list=/etc/apt/sources.list.d/github-cli.list
if [ "$mode" != apply ] && [ -f "$gh_list" ]; then
  if ! { sudo timeout 120 apt-get update -qq -o Dir::Etc::sourcelist="$gh_list" -o Dir::Etc::sourceparts=- \
      -o APT::Get::List-Cleanup=0 -o DPkg::Lock::Timeout=60 &&
    sudo DEBIAN_FRONTEND=noninteractive timeout 180 apt-get install -y -qq --only-upgrade \
      -o DPkg::Lock::Timeout=60 gh; } >"$boat_dir/gh-upgrade.log" 2>&1; then
    echo "gh upgrade failed: see ~/.t3boat/gh-upgrade.log" >&2
  fi
fi
[ "$mode" != upgrade ] || exit 0
# Written whole, then renamed: a gh started meanwhile never reads half a file.
mkdir -p "$HOME/.local/bin"
cat >"$HOME/.local/bin/gh.t3boat-new" <<'WRAPPER'
#!/usr/bin/env bash
# t3boat: Boat's gh, with the user's own GitHub token for commands that upload a file (--attach).
# GitHub's upload server refuses the GitHub App token gh has here.
# With ~/.t3boat/gh-everything the token is for every command. Git asks `gh auth git-credential`
# for its password, finds this file first on PATH and so gets the same token.
real=/usr/bin/gh
token_file="$HOME/.t3boat/gh-upload-token"

if [ -e "$HOME/.t3boat/gh-everything" ] && [ -s "$token_file" ]; then
  case "${1:-} ${2:-}" in
    # gh refuses these while GH_TOKEN is set, and Boat signs its own login in with them.
    "auth login" | "auth logout" | "auth refresh" | "auth setup-git" | "auth switch") exec "$real" "$@" ;;
  esac
  GH_TOKEN="$(cat "$token_file")" exec "$real" "$@"
fi

uploads() {
  # `gh pr -R owner/repo edit 7` is `pr edit`.
  local command="" words=0 attach=1
  while [ $# -gt 0 ]; do
    case "$1" in
      --) break ;;
      --attach | --attach=*) attach=0 ;;
      # Flags that take a value: `--body '--attach'` is text, not an upload.
      -a | -b | -B | -F | -H | -l | -m | -p | -r | -R | -t | -T) shift ;;
      --assignee | --base | --blocked-by | --blocking | --body | --body-file | --head | --label) shift ;;
      --milestone | --parent | --project | --recover | --repo | --reviewer | --template | --title | --type) shift ;;
      --add-assignee | --add-blocked-by | --add-blocking | --add-label | --add-project | --add-reviewer | --add-sub-issue) shift ;;
      --remove-assignee | --remove-blocked-by | --remove-blocking | --remove-label | --remove-project) shift ;;
      --remove-reviewer | --remove-sub-issue) shift ;;
      -*) ;;
      *)
        if [ "$words" -lt 2 ]; then
          command="$command $1"
          words=$((words + 1))
        fi
        ;;
    esac
    shift
  done
  case "$command" in
    " pr create" | " pr new" | " pr edit" | " pr comment") ;;
    " issue create" | " issue new" | " issue edit" | " issue comment") ;;
    *) return 1 ;;
  esac
  return "$attach"
}

if uploads "$@"; then
  if [ -s "$token_file" ]; then
    GH_TOKEN="$(cat "$token_file")" exec "$real" "$@"
  fi
  echo "t3boat: uploads need \"GitHub login\" connected in t3boat's settings on your computer." >&2
fi
exec "$real" "$@"
WRAPPER
chmod 755 "$HOME/.local/bin/gh.t3boat-new"
mv -f "$HOME/.local/bin/gh.t3boat-new" "$HOME/.local/bin/gh"
if [ -n "$token" ]; then
  (umask 077 && printf '%s' "$token" >"$boat_dir/gh-upload-token.new")
  mv -f "$boat_dir/gh-upload-token.new" "$boat_dir/gh-upload-token"
  if [ "$everything" = 1 ]; then
    : >"$boat_dir/gh-everything"
    # Git must reach the wrapper through its credential helper. A failure gets this tried again.
    answer="$(printf 'protocol=https\nhost=github.com\n\n' | GIT_TERMINAL_PROMPT=0 git credential fill 2>/dev/null || true)"
    if [ "$(printf '%s\n' "$answer" | sed -n 's/^password=//p')" != "$token" ]; then
      echo "git on this machine does not use the GitHub login yet" >&2
      exit 4
    fi
  fi
  # A failure gets this tried again.
  if ! /usr/bin/gh pr edit --help 2>/dev/null | grep -q -- --attach; then
    echo "gh on this machine has no --attach yet" >&2
    exit 3
  fi
fi
