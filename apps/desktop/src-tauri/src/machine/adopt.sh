#!/usr/bin/env bash
# Runs on a t3boat machine that another computer created. Reads the machine's record of itself and
# issues this computer its own T3 token. Changes nothing else. Prints one JSON line.
set -euo pipefail

t3="$HOME/.local/bin/t3"
config="$HOME/.t3boat/config.json"
[ -f "$config" ] || { echo "not a t3boat machine: it has no ~/.t3boat/config.json" >&2; exit 1; }

ready=0
for _ in $(seq 1 120); do
  if curl -sf -o /dev/null http://127.0.0.1:3773/.well-known/t3/environment; then ready=1; break; fi
  sleep 0.5
done
[ "$ready" = 1 ] || { echo "T3 server did not answer" >&2; exit 1; }

token="$("$t3" auth session issue --token-only --ttl 3650d --label t3boat)"
TOKEN="$token" VERSION="$("$t3" --version)" node -e '
  const c = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
  fetch("http://127.0.0.1:3773/.well-known/t3/environment").then((r) => r.json()).then((d) =>
    console.log(JSON.stringify({
      machineId: c.machineId ?? null,
      name: c.name ?? null,
      watchdogSecret: c.secret,
      snapshot: c.snapshot,
      revision: c.revision ?? null,
      t3Version: process.env.VERSION.replace(/^t3 v/, "").trim(),
      environmentId: d.environmentId,
      upstreamToken: process.env.TOKEN,
    })),
  );
' "$config"
