#!/usr/bin/env bash
# Runs on a Boat sandbox, as `user`: by provision.sh, and by itself when "GitHub uploads" changes
# in t3boat's settings, since it touches nothing T3 or an agent's turn depends on. Idempotent.
#   [T3BOAT_GH_UPLOAD_TOKEN=<token>] gh-uploads.sh
set -euo pipefail
boat_dir="$HOME/.t3boat"
mkdir -p "$boat_dir"
chmod 700 "$boat_dir"

# GitHub uploads. Boat signs `gh` in with its GitHub App's token, which GitHub's upload server
# turns away, so `gh pr edit --attach shot.png` fails. With "GitHub uploads" connected in t3boat's
# settings the machine holds a second token, the user's own, and a `gh` ahead of Boat's on PATH
# uses it for the commands that upload and for nothing else.
# First the newest `gh` from GitHub's apt repo, which Boat's image already has: the image's own
# predates `--attach`. Only that repo is refreshed. A failed upgrade costs uploads, not the machine.
gh_list=/etc/apt/sources.list.d/github-cli.list
if [ -f "$gh_list" ]; then
  if ! { sudo timeout 120 apt-get update -qq -o Dir::Etc::sourcelist="$gh_list" -o Dir::Etc::sourceparts=- \
      -o APT::Get::List-Cleanup=0 -o DPkg::Lock::Timeout=60 &&
    sudo DEBIAN_FRONTEND=noninteractive timeout 180 apt-get install -y -qq --only-upgrade \
      -o DPkg::Lock::Timeout=60 gh; } >"$boat_dir/gh-upgrade.log" 2>&1; then
    echo "gh upgrade failed: see ~/.t3boat/gh-upgrade.log" >&2
  fi
fi
mkdir -p "$HOME/.local/bin"
cat >"$HOME/.local/bin/gh" <<'WRAPPER'
#!/usr/bin/env bash
# t3boat: runs Boat's gh. A command that uploads a file (--attach) runs with the user's own GitHub
# token from "GitHub uploads" in t3boat's settings, since GitHub's upload server turns away the
# GitHub App token gh has here. Everything else runs exactly as without this file.
real=/usr/bin/gh
token_file="$HOME/.t3boat/gh-upload-token"
for arg in "$@"; do
  case "$arg" in
    --) break ;;
    --attach | --attach=*)
      if [ -s "$token_file" ]; then
        GH_TOKEN="$(cat "$token_file")" exec "$real" "$@"
      fi
      echo "t3boat: uploads need \"GitHub uploads\" connected in t3boat's settings on your computer." >&2
      break
      ;;
  esac
done
exec "$real" "$@"
WRAPPER
chmod 755 "$HOME/.local/bin/gh"
if [ -n "${T3BOAT_GH_UPLOAD_TOKEN:-}" ]; then
  (umask 077 && printf '%s' "$T3BOAT_GH_UPLOAD_TOKEN" >"$boat_dir/gh-upload-token")
else
  rm -f "$boat_dir/gh-upload-token"
fi
