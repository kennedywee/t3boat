#!/usr/bin/env bash
# Runs on a Boat sandbox, as `user`. Idempotent; also used to upgrade a machine.
#   T3BOAT_SECRET=<watchdog-secret> [T3BOAT_GH_UPLOAD_TOKEN=<token>] provision.sh <t3-version> <sleep-after-secs> <snapshot-name> <mint-upstream-token:0|1> <name> <fleet-repo|-> <machine-id> <revision> <sandbox-id>
# Expects watchdog.mjs and gh-uploads.sh next to it. Prints one JSON line: {"environmentId", "upstreamToken"?}.
set -euo pipefail

version="$1" sleep_after="$2" snapshot="$3" mint_upstream="$4" name="$5" fleet="${6:--}" machine_id="$7" revision="$8" sandbox_id="$9"
secret="$T3BOAT_SECRET"
here="$(cd "$(dirname "$0")" && pwd)"
t3="$HOME/.local/bin/t3"
boat_dir="$HOME/.t3boat"
units="$HOME/.config/systemd/user"
mkdir -p "$boat_dir" "$units" "$HOME/.t3/userdata"
chmod 700 "$boat_dir"

# T3 server pinned to the desktop app's version, as a systemd service that survives resumes.
# It is restarted only when something it runs on changed: a restart drops every T3 Code
# connection, and most upgrades only touch the watchdog.
restart_t3=0
if [ "$("$t3" --version 2>/dev/null || true)" != "t3 v$version" ]; then
  if ! curl -fsSL https://t3.codes/install.sh | T3CODE_VERSION="$version" sh >"$boat_dir/install.log" 2>&1; then
    echo "T3 install failed" >&2
    tail -n 40 "$boat_dir/install.log" >&2
    exit 1
  fi
  restart_t3=1
fi
settings="$HOME/.t3/userdata/settings.json"
before="$(cat "$settings" 2>/dev/null || true)"
node -e '
  const fs = require("fs"), f = process.argv[1];
  const s = fs.existsSync(f) ? JSON.parse(fs.readFileSync(f, "utf8")) : {};
  s.linkedEnvironmentsEnabled = true;
  fs.writeFileSync(f, JSON.stringify(s, null, 2));
' "$settings"
[ "$before" = "$(cat "$settings")" ] || restart_t3=1
# The machine's name, as other devices show it (T3 Connect, linked environments). T3 reads it
# from here as it starts; without it, they show the sandbox's hostname.
machine_info="PRETTY_HOSTNAME=$name"
if [ "$(cat /etc/machine-info 2>/dev/null || true)" != "$machine_info" ]; then
  echo "$machine_info" | sudo tee /etc/machine-info >/dev/null
  restart_t3=1
fi
# Git's author, so agents can commit: the GitHub account Boat signed the sandbox in to, with
# GitHub's noreply address (what GitHub itself uses when an email is private). It goes in git's
# XDG config, which Boat leaves alone; an author already set anywhere is kept.
if ! git config --global user.email >/dev/null 2>&1; then
  if gh_user="$(gh api user --jq '[.id, .login, (.name // .login)] | @tsv' 2>/dev/null)"; then
    IFS=$'\t' read -r gh_id gh_login gh_name <<<"$gh_user"
    if [ -n "${gh_id:-}" ] && [ -n "${gh_login:-}" ]; then
      mkdir -p "$HOME/.config/git"
      git config --file "$HOME/.config/git/config" user.name "${gh_name:-$gh_login}"
      git config --file "$HOME/.config/git/config" user.email "${gh_id}+${gh_login}@users.noreply.github.com"
    fi
  fi
fi

# t3boat retries it by itself: a failure here must not fail the machine.
bash "$here/gh-uploads.sh" || echo "GitHub uploads not set up yet" >&2
unset T3BOAT_GH_UPLOAD_TOKEN

"$t3" service install --allow-downgrade >/dev/null
systemctl --user daemon-reload
systemctl --user is-active --quiet t3code.service || restart_t3=1
[ "$restart_t3" = 0 ] || systemctl --user restart t3code.service

ready=0
for _ in $(seq 1 120); do
  if curl -sf -o /dev/null http://127.0.0.1:3773/.well-known/t3/environment; then ready=1; break; fi
  sleep 0.5
done
if [ "$ready" != 1 ]; then
  echo "T3 server did not start" >&2
  tail -n 40 "$HOME/.t3/userdata/logs/boot-service.log" >&2 || true
  exit 1
fi

# Watchdog: its own read-only T3 token, kept across upgrades. Its config is also the machine's
# record of itself: another computer's t3boat reads it to add the machine there (adopt.sh).
config="$boat_dir/config.json"
watchdog_token="$(node -e 'try{process.stdout.write(require(process.argv[1]).t3Token||"")}catch{}' "$config")"
if [ -z "$watchdog_token" ]; then
  watchdog_token="$("$t3" auth session issue --token-only --ttl 3650d --label t3boat-watchdog --scope orchestration:read)"
fi
SECRET="$secret" TOKEN="$watchdog_token" SLEEP="$sleep_after" SNAPSHOT="$snapshot" \
  MACHINE_ID="$machine_id" SANDBOX_ID="$sandbox_id" NAME="$name" VERSION="$version" REVISION="$revision" node -e '
  const e = process.env;
  require("fs").writeFileSync(process.argv[1], JSON.stringify({
    secret: e.SECRET, t3Token: e.TOKEN, sleepAfterSecs: Number(e.SLEEP), snapshot: e.SNAPSHOT,
    machineId: e.MACHINE_ID, sandboxId: e.SANDBOX_ID, name: e.NAME, t3Version: e.VERSION, revision: e.REVISION
  }, null, 2), { mode: 0o600 });
' "$config"
install -m 600 "$here/watchdog.mjs" "$boat_dir/watchdog.mjs"
cat >"$units/t3boat-watchdog.service" <<'UNIT'
[Unit]
Description=t3boat watchdog: stops this sandbox when idle
After=t3code.service

[Service]
ExecStart=/bin/bash -lc 'exec node --no-warnings %h/.t3boat/watchdog.mjs'
Restart=always
RestartSec=5
StandardOutput=append:%h/.t3boat/watchdog.log
StandardError=append:%h/.t3boat/watchdog.log

[Install]
WantedBy=default.target
UNIT
systemctl --user daemon-reload
systemctl --user enable t3boat-watchdog.service >/dev/null 2>&1
systemctl --user restart t3boat-watchdog.service

# Fleet: the user's skills and agent setup (t3boat's "Fleet repo" setting, `-` for none). The machine
# clones the repo once and runs its install.sh. After that the machine changes only when the user
# asks an agent for a "fleet sync" (the fleet's own skill): nothing pulls on a timer. Boat signs git
# in to GitHub a moment after a resume, so a failed clone retries.
cat >"$boat_dir/fleet-install.sh" <<'INSTALL'
#!/usr/bin/env bash
# t3boat: clones the fleet repo to ~/fleet and installs it, once. Run by t3boat-fleet.service; logs
# to ~/.t3boat/fleet.log. A checkout that is already there is left alone.
set -uo pipefail
boat_dir="$HOME/.t3boat"
repo="$(cat "$boat_dir/fleet-repo" 2>/dev/null || true)"
[ -n "$repo" ] && [ "$repo" != "-" ] || exit 0
dir="$HOME/fleet" url="https://github.com/$repo.git" log="$boat_dir/fleet.log"
export GIT_TERMINAL_PROMPT=0
say() { echo "$(date -Is) $*" >>"$log"; }
[ -f "$log" ] && tail -n 400 "$log" >"$log.tmp" && mv "$log.tmp" "$log"
# The URL as written, not as rewritten by git's insteadOf rules.
if [ -d "$dir/.git" ] && [ "$(git -C "$dir" config --get remote.origin.url 2>/dev/null)" != "$url" ]; then
  aside="$dir.old.$(date +%s).$$"
  mv "$dir" "$aside" || { say "could not move $dir aside"; exit 1; }
  say "fleet repo is now $repo: the old checkout is in $aside"
fi
# Already cloned and installed: updates are the user's call.
[ -d "$dir/.git" ] && [ -f "$boat_dir/fleet-installed" ] && exit 0
if [ ! -d "$dir/.git" ]; then
  git clone -q "$url" "$dir" 2>>"$log" || { say "clone of $repo failed"; exit 1; }
fi
head="$(git -C "$dir" rev-parse HEAD)"
bash "$dir/install.sh" >>"$log" 2>&1 || { say "install.sh failed at ${head:0:7}"; exit 1; }
echo "$head" >"$boat_dir/fleet-installed"
say "installed $repo at ${head:0:7}"
INSTALL
chmod 700 "$boat_dir/fleet-install.sh"
cat >"$units/t3boat-fleet.service" <<'UNIT'
[Unit]
Description=t3boat fleet: clone the fleet repo and install it, once
After=network-online.target

[Service]
Type=oneshot
ExecStart=/bin/bash -lc 'exec %h/.t3boat/fleet-install.sh'
Restart=on-failure
RestartSec=120
UNIT
# Machines provisioned before this had a timer that pulled every 15 minutes.
systemctl --user disable --now t3boat-fleet.timer >/dev/null 2>&1 || true
rm -f "$units/t3boat-fleet.timer" "$boat_dir/fleet-sync.sh"
systemctl --user daemon-reload
if [ "$fleet" = "-" ]; then
  rm -f "$boat_dir/fleet-repo" "$boat_dir/fleet-installed"
else
  if [ "$(cat "$boat_dir/fleet-repo" 2>/dev/null || true)" != "$fleet" ]; then
    echo "$fleet" >"$boat_dir/fleet-repo"
    rm -f "$boat_dir/fleet-installed"
  fi
  # Without waiting, so provisioning is not held up by a clone.
  systemctl --user start --no-block t3boat-fleet.service
fi

upstream_token=""
if [ "$mint_upstream" = 1 ]; then
  upstream_token="$("$t3" auth session issue --token-only --ttl 3650d --label t3boat)"
fi
environment_id="$(curl -sf http://127.0.0.1:3773/.well-known/t3/environment | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>process.stdout.write(JSON.parse(s).environmentId))')"
ENV_ID="$environment_id" TOK="$upstream_token" node -e '
  const o = { environmentId: process.env.ENV_ID };
  if (process.env.TOK) o.upstreamToken = process.env.TOK;
  console.log(JSON.stringify(o));
'
