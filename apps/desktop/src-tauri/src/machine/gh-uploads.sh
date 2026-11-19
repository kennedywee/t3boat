#!/usr/bin/env bash
# Runs on a Boat sandbox, as `user`. Idempotent, and restarts nothing: safe while agents work.
#   [T3BOAT_GH_UPLOAD_TOKEN=<token>] gh-uploads.sh [all|upgrade|apply]
# `upgrade` is apt alone, `apply` the wrapper and the token: a disconnect runs only `apply`.
set -euo pipefail
mode="${1:-all}"
boat_dir="$HOME/.t3boat"
mkdir -p "$boat_dir"
chmod 700 "$boat_dir"

# First, before anything below can fail.
if [ "$mode" != upgrade ]; then
  rm -f "$boat_dir/gh-upload-token.new"
  [ -n "${T3BOAT_GH_UPLOAD_TOKEN:-}" ] || rm -f "$boat_dir/gh-upload-token"
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
real=/usr/bin/gh
token_file="$HOME/.t3boat/gh-upload-token"

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
  echo "t3boat: uploads need \"GitHub uploads\" connected in t3boat's settings on your computer." >&2
fi
exec "$real" "$@"
WRAPPER
chmod 755 "$HOME/.local/bin/gh.t3boat-new"
mv -f "$HOME/.local/bin/gh.t3boat-new" "$HOME/.local/bin/gh"
if [ -n "${T3BOAT_GH_UPLOAD_TOKEN:-}" ]; then
  (umask 077 && printf '%s' "$T3BOAT_GH_UPLOAD_TOKEN" >"$boat_dir/gh-upload-token.new")
  mv -f "$boat_dir/gh-upload-token.new" "$boat_dir/gh-upload-token"
  # A failure gets this tried again.
  if ! /usr/bin/gh pr edit --help 2>/dev/null | grep -q -- --attach; then
    echo "gh on this machine has no --attach yet" >&2
    exit 3
  fi
fi
