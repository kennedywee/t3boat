// t3boat watchdog. Runs on the Boat sandbox next to the T3 server and stops the sandbox once no
// agent works and nobody used it for `sleepAfterSecs`. It lives here, not on the laptop, so
// machines finish their work and then sleep even when the laptop is closed.
//
// Going to sleep, it first saves the disk as a Boat snapshot: Boat deletes a stopped sandbox's
// disk after 14 days, the saved snapshot stays, and t3boat restores the machine from it.
//
// Then it "drains": it publishes a manifest of the final orchestration state so the laptop
// proxy can keep answering T3 Code truthfully while the machine sleeps. The proxy captures it
// and acks; without an ack (laptop gone) the watchdog stops after a grace period. Several
// computers can run t3boat for one machine: the stop waits a moment for each to capture.

import { createServer } from "node:http";
import { readFileSync, writeFileSync } from "node:fs";
import { execFile } from "node:child_process";
import { DatabaseSync } from "node:sqlite";
import { availableParallelism, homedir } from "node:os";
import { join } from "node:path";

const HOME = homedir();
const CONFIG_PATH = join(HOME, ".t3boat", "config.json");
// On disk, so it survives watchdog restarts (upgrades); a stop ends it (see BOOT_ID).
const KEEP_AWAKE_PATH = join(HOME, ".t3boat", "keep-awake-until");
const MAX_KEEP_AWAKE_MINUTES = 24 * 60;
const DB_PATH = join(HOME, ".t3", "userdata", "statev2.sqlite");
const T3_ORIGIN = "http://127.0.0.1:3773";
const PORT = 3774;
const TICK_MS = 5_000;
const DRAIN_GRACE_MS = 20_000;
// A tick that arrives this late means the VM was frozen: a warm resume after a stop.
const RESUMED_GAP_MS = 30_000;
// Still running this long after asking Boat to stop: the stop failed or a fast resume undid it.
const STOP_SETTLE_MS = 90_000;
// A save takes about 30 s. One that hangs must not keep the machine up.
const SNAPSHOT_TIMEOUT_MS = 120_000;

const config = JSON.parse(readFileSync(CONFIG_PATH, "utf8"));
// Boat's `self` needs BOAT_ID, which some boots lack. Restored from a snapshot, the machine runs on
// a new sandbox: BOAT_ID names it when set, and the provisioning that follows a restore rewrites
// the config.
const SANDBOX_ID = process.env.BOAT_ID || config.sandboxId;
const log = (...args) => console.log(new Date().toISOString(), ...args);

/** @type {"active" | "saving" | "draining" | "stopping"} */
let phase = "active";
/** Last time anything held the machine up, CPU load included: the idle countdown starts here. */
let lastActivityAt = Date.now();
/** Last time an agent, the user or a keep-awake held it up: CPU load holds it only so long after. */
let lastUsedAt = Date.now();
/** Agent runs in flight: top-level threads and subagents. */
let working = { threads: 0, subagents: 0 };
// T3 not answering: an agent might be mid-turn, so count it as working, but only this long, so a
// broken server cannot keep the machine billing.
const UNKNOWN_BUSY_MS = 30 * 60_000;
let unknownSince = 0;
// T3 boots after this watchdog: until it first answers it is starting, not failing, and no agent
// can be mid-turn on it.
let t3Answered = false;
const unknownBusy = () => unknownSince > 0 && Date.now() - unknownSince < UNKNOWN_BUSY_MS;
const agentsWorking = () => working.threads + working.subagents > 0;
const agentsBusy = () => agentsWorking() || unknownBusy();

/**
 * One busy check for the tick, "Sleep now" and the delete guard. A failure starts (or continues)
 * "can't tell".
 */
async function refreshWorking() {
  try {
    working = await checkWorking();
    unknownSince = 0;
    t3Answered = true;
  } catch (e) {
    working = { threads: 0, subagents: 0 };
    if (t3Answered) unknownSince ||= Date.now();
    log(e.message);
  }
}
// A build or job started outside an agent turn (in the background, in a terminal) shows as CPU
// use; an idle dev server does not. Counted in cores, so a single-threaded build counts on any
// size, and smoothed over ~30s so a short spike doesn't count. It holds the machine up for at most
// CPU_HOLD_MS after the last use, so a busy dev server or a stray loop cannot keep it billing;
// longer jobs take a keep-awake.
const CPU_BUSY_CORES = 0.5;
const CPU_HOLD_MS = 30 * 60_000;
const CPU_COUNT = availableParallelism();
/** Cores in use. */
let cpuLoad = 0;
/** "Sleep now" was asked for: CPU load no longer holds the machine up, agents still do. */
let sleepRequested = false;
let cpuPrev = readCpu();
const cpuBusy = () => !sleepRequested && cpuLoad > CPU_BUSY_CORES && Date.now() - lastUsedAt < CPU_HOLD_MS;
const isBusy = () => agentsBusy() || cpuBusy();

/** Busy and total jiffies across all cores since boot, from /proc/stat. */
function readCpu() {
  const fields = readFileSync("/proc/stat", "utf8").split("\n")[0].trim().split(/\s+/).slice(1).map(Number);
  const idle = fields[3] + (fields[4] ?? 0);
  const total = fields.reduce((a, b) => a + b, 0);
  return { busy: total - idle, total };
}

function sampleCpu() {
  const now = readCpu();
  const total = now.total - cpuPrev.total;
  const usage = total > 0 ? ((now.busy - cpuPrev.busy) / total) * CPU_COUNT : 0;
  cpuPrev = now;
  cpuLoad = cpuLoad * 0.8 + usage * 0.2;
}
let manifest = null;
let drainStartedAt = 0;
// The t3boat apps connected lately, by the id each sends, and those that acked this drain.
const CLIENT_SEEN_MS = 15_000;
const OTHERS_WAIT_MS = 10_000;
const clients = new Map();
let acks = new Set();
const othersCapturing = () =>
  [...clients].some(([id, seenAt]) => Date.now() - seenAt < CLIENT_SEEN_MS && !acks.has(id));
/** Counts the saves begun: one called off and begun again must not finish for the later one. */
let saves = 0;
let lastTickAt = Date.now();
let stopRequestedAt = 0;
/** "Keep awake": no sleeping before this (ms since epoch), working or not; Infinity = until stopped. */
// A stop ends any hold: after a cold resume (new boot) the stored one is void. A warm resume
// keeps the boot, so the resume check in tick() ends it there.
const BOOT_ID = readFileSafe("/proc/sys/kernel/random/boot_id").trim();
const [storedUntil, storedBoot] = readFileSafe(KEEP_AWAKE_PATH).split(" ");
let keepAwakeUntil = storedBoot === BOOT_ID ? Number(storedUntil) || 0 : 0;
const keptAwake = () => Date.now() < keepAwakeUntil;

function readFileSafe(path) {
  try {
    return readFileSync(path, "utf8");
  } catch {
    return "";
  }
}

/** null: until stopped; 0: off. */
function setKeepAwake(minutes) {
  keepAwakeUntil = minutes === null ? Infinity : minutes > 0 ? Date.now() + minutes * 60_000 : 0;
  writeFileSync(KEEP_AWAKE_PATH, `${keepAwakeUntil} ${BOOT_ID}`);
}

// Work T3 starts on its own at a set time: an automation, a webhook run it held back, an agent
// resuming after a usage limit. Due this soon, the machine waits for it instead of sleeping and
// waking again. Overdue this long, it is not coming (T3 runs due work within seconds), or the
// time stays behind in T3's rows after it ran.
const DUE_SOON_MS = 5 * 60_000;
const OVERDUE_MS = 2 * 60_000;
/** When T3 next starts work on its own, ms since epoch; null for never. t3boat wakes the machine for it. */
let nextDueAt = null;
const dueSoon = () => nextDueAt !== null && nextDueAt - Date.now() < DUE_SOON_MS;

// Events only a person causes, from any client: T3 Code through t3boat, but also a phone or
// another computer through T3 Connect, which t3boat never sees.
const PERSON_EVENTS = [
  "thread.created",
  "thread.visit-recorded",
  "thread.visited",
  "thread.marked-unread",
  "thread.archived",
  "thread.unarchived",
  "thread.deleted",
  "thread.pinned",
  "thread.unpinned",
  "thread.pin-reordered",
  "thread.snoozed",
  "thread.unsnoozed",
  "thread.tags-updated",
  "thread.runtime-mode-updated",
  "thread.interaction-mode-updated",
  "thread.model-selection-updated",
];
/** Last orchestration event looked at; null until the first look. */
let eventCursor = null;

/**
 * Reads T3's database for the tick: sets `nextDueAt`, and says whether a person acted since the
 * last tick. A table this T3 version lacks has nothing due.
 */
function readT3State() {
  const db = new DatabaseSync(DB_PATH, { readOnly: true });
  try {
    const times = (sql) => {
      try {
        return db.prepare(sql).all().map((r) => (typeof r.t === "string" ? Date.parse(r.t) : r.t));
      } catch {
        return [];
      }
    };
    const recovering = `FROM orchestration_v2_projection_threads
      WHERE deleted_at IS NULL AND json_extract(payload_json, '$.limitRecovery.autoResume') = 1`;
    const due = [
      // Parsed here, not compared as text: T3 notes expanded years sort first.
      ...times(`SELECT next_run_at AS t FROM scheduled_tasks
        WHERE enabled = 1 AND next_run_at IS NOT NULL AND last_run_status <> 'running'`),
      ...times(`SELECT MIN(due_at) AS t FROM scheduled_task_webhook_pending WHERE state = 'pending'`),
      ...times(`SELECT json_extract(payload_json, '$.limitRecovery.resetAt') AS t ${recovering}`),
      ...times(`SELECT json_extract(payload_json, '$.snoozedUntil') AS t ${recovering}`),
    ].filter((t) => Number.isFinite(t) && Date.now() - t < OVERDUE_MS);
    nextDueAt = due.length > 0 ? Math.min(...due) : null;

    if (eventCursor === null) {
      eventCursor = db.prepare("SELECT MAX(sequence) AS s FROM orchestration_events").get()?.s ?? 0;
      return false;
    }
    const events = db
      .prepare(
        `SELECT MAX(sequence) AS last, SUM(event_type IN (${PERSON_EVENTS.map(() => "?").join(",")})) AS person
         FROM orchestration_events WHERE sequence > ?`,
      )
      .get(...PERSON_EVENTS, eventCursor);
    eventCursor = events?.last ?? eventCursor;
    return (events?.person ?? 0) > 0;
  } finally {
    db.close();
  }
}

const LIVE_STATUSES = new Set(["queued", "preparing", "starting", "running", "waiting"]);

/** Any agent run in flight, including one waiting on an approval: stopping would kill it. */
async function checkWorking() {
  const res = await fetch(`${T3_ORIGIN}/api/orchestration/shell`, {
    headers: { authorization: `Bearer ${config.t3Token}`, "x-t3-orchestration-protocol": "2" },
    signal: AbortSignal.timeout(10_000),
  });
  if (!res.ok) throw new Error(`shell snapshot: HTTP ${res.status}`);
  const shell = await res.json();
  const live = (shell.threads ?? []).filter(
    (t) =>
      t.activeRunId != null ||
      t.activityRunStatus != null ||
      LIVE_STATUSES.has(t.status) ||
      t.pendingRuntimeRequest != null ||
      (t.pendingBackgroundTasks?.length ?? 0) > 0,
  );
  const subagents = live.filter((t) => t.lineage?.relationshipToParent === "subagent").length;
  return { threads: live.length - subagents, subagents };
}

/** The orchestration high-water marks T3 itself compares resume cursors against. */
function readManifest() {
  const db = new DatabaseSync(DB_PATH, { readOnly: true });
  try {
    const global = db
      .prepare(
        `SELECT MAX(sequence) AS s FROM orchestration_events
         WHERE aggregate_kind = 'project' OR (application_event_version = 2 AND aggregate_kind = 'thread')`,
      )
      .get();
    const rows = db
      .prepare(
        `SELECT stream_id AS id, MAX(sequence) AS s FROM orchestration_events
         WHERE application_event_version = 2 AND aggregate_kind = 'thread' GROUP BY stream_id`,
      )
      .all();
    return {
      sequence: global?.s ?? 0,
      threads: Object.fromEntries(rows.map((r) => [r.id, r.s])),
    };
  } finally {
    db.close();
  }
}

/** Saves the snapshot, then drains, unless the machine is used meanwhile. */
async function save() {
  phase = "saving";
  const mine = ++saves;
  log("saving snapshot", config.snapshot);
  // A machine that cannot save still sleeps: it must not bill on, and Boat keeps its disk 14 days.
  await boat(["snapshot", SANDBOX_ID, config.snapshot], SNAPSHOT_TIMEOUT_MS).catch((e) => log("snapshot failed", e.message));
  if (phase === "saving" && mine === saves) drain();
}

function drain() {
  try {
    manifest = readManifest();
  } catch (e) {
    log("manifest unavailable", e.message);
    manifest = null;
  }
  phase = "draining";
  acks = new Set();
  drainStartedAt = Date.now();
  log("draining", manifest?.sequence);
}

/** Someone uses the machine: it stays up, and CPU load may hold it up again. */
function touch(reason) {
  lastUsedAt = Date.now();
  hold(reason);
}

/** Keeps the machine up and restarts the idle countdown, calling off a sleep in progress. */
function hold(reason) {
  sleepRequested = false;
  lastActivityAt = Date.now();
  if (phase !== "active") log(`back to active: ${reason}`);
  phase = "active";
  manifest = null;
}

/** Runs the Boat CLI, in a login shell for its PATH. */
function boat(args, timeout) {
  return new Promise((resolve, reject) => {
    execFile("bash", ["-lc", 'boat "$@" --json', "boat", ...args], { timeout }, (err, stdout, stderr) =>
      err ? reject(new Error(stderr.trim() || stdout.trim() || err.message)) : resolve(stdout),
    );
  });
}

function stopSelf() {
  phase = "stopping";
  stopRequestedAt = Date.now();
  log("stopping sandbox");
  boat(["stop", SANDBOX_ID]).catch((e) => {
    log("boat stop failed", e.message);
    touch("stop failed");
  });
}

async function tick() {
  const now = Date.now();
  if (now - lastTickAt > RESUMED_GAP_MS) {
    setKeepAwake(0);
    touch("resumed");
  }
  lastTickAt = now;
  if (phase === "stopping") {
    if (now - stopRequestedAt > STOP_SETTLE_MS) touch("still running after stop");
    return;
  }

  sampleCpu();
  await refreshWorking();
  let personActed = false;
  try {
    personActed = readT3State();
  } catch (e) {
    log("T3 state unavailable", e.message);
  }
  if (agentsWorking()) return touch("agent working");
  if (personActed) return touch("used in T3");
  if (keptAwake()) return touch("kept awake");
  if (cpuBusy()) return hold("cpu busy");
  if (dueSoon()) return hold("work due");
  // Can't tell: don't sleep, but it isn't activity either, so flaky checks can't hold it up forever.
  if (unknownBusy()) return;

  const idleMs = now - lastActivityAt;
  // Not awaited: ticks go on during the save, so an agent starting to work calls it off.
  if (phase === "active" && idleMs >= config.sleepAfterSecs * 1000) save();
  else if (phase === "draining" && now - drainStartedAt >= DRAIN_GRACE_MS) finishDrain();
}

/** Stops, unless anything happened since the manifest: then someone uses the machine after all. */
function finishDrain() {
  let current = null;
  try {
    current = readManifest();
  } catch {}
  if (manifest && current && current.sequence !== manifest.sequence) return touch("new events");
  stopSelf();
}

function status() {
  return {
    phase,
    busy: isBusy(),
    cpuBusy: !agentsBusy() && cpuBusy(),
    agentsUnknown: unknownBusy(),
    workingThreads: working.threads,
    workingSubagents: working.subagents,
    keepAwakeForever: keepAwakeUntil === Infinity,
    keepAwakeSecs: Number.isFinite(keepAwakeUntil) ? Math.max(0, Math.ceil((keepAwakeUntil - Date.now()) / 1000)) : 0,
    idleSecs: Math.floor((Date.now() - lastActivityAt) / 1000),
    sleepAfterSecs: config.sleepAfterSecs,
    // Another computer's t3boat may have changed it.
    t3Version: config.t3Version ?? null,
    nextDueAt,
    workDue: dueSoon(),
    // While stopping, it names the sequence the machine stops at: the proxy trusts its
    // capture only when they match.
    manifest: phase === "active" ? null : manifest,
  };
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    let body = "";
    req.on("data", (c) => {
      body += c;
      if (body.length > 64_000) req.destroy();
    });
    req.on("end", () => resolve(body));
    req.on("error", reject);
  });
}

const server = createServer(async (req, res) => {
  const reply = (code, value) => {
    res.writeHead(code, { "content-type": "application/json" });
    res.end(value === undefined ? "" : JSON.stringify(value));
  };
  if (req.headers.authorization !== `Bearer ${config.secret}`) return reply(401, { error: "unauthorized" });
  const client = req.headers["x-t3boat-client"];
  if (client) clients.set(client, Date.now());
  try {
    if (req.method === "GET" && req.url === "/status") return reply(200, status());
    if (req.method === "GET" && req.url === "/status?fresh") {
      // The delete guard: agents as of now, not as of the last tick.
      await refreshWorking();
      return reply(200, status());
    }
    if (req.method === "POST" && req.url === "/activity") {
      // { resumed }: the proxy just resumed the sandbox, so the stop asked for here is over. A
      // tick only notices a freeze of RESUMED_GAP_MS or more.
      const { resumed } = JSON.parse((await readBody(req)) || "{}");
      if (resumed) {
        setKeepAwake(0);
        touch("resumed");
        return reply(200, status());
      }
      // Too late to call off a stop: the proxy waits for it and resumes the machine.
      if (phase === "stopping") return reply(409, { error: "stopping", ...status() });
      touch("user activity");
      return reply(200, status());
    }
    if (req.method === "POST" && req.url === "/drain") {
      // "Sleep now" from the tray. Never mid-turn; it does end a keep-awake.
      await refreshWorking();
      // A busy CPU alone doesn't refuse: "Sleep now" is the user's explicit call.
      if (agentsBusy()) {
        const error = agentsWorking() ? "an agent is working" : "could not check agents";
        return reply(409, { error, ...status() });
      }
      setKeepAwake(0);
      if (phase === "active") save();
      sleepRequested = true;
      return reply(200, status());
    }
    if (req.method === "POST" && req.url === "/keep-awake") {
      // { minutes }: null = until stopped, 0 ends it. Counted on this machine's clock, never the laptop's.
      const { minutes } = JSON.parse((await readBody(req)) || "{}");
      // Too late during a stop (checked after the body: a stop can begin meanwhile): the proxy
      // waits for it, resumes the machine and asks again.
      if (phase === "stopping") return reply(409, { error: "stopping", ...status() });
      const valid = minutes === null || (Number.isInteger(minutes) && minutes >= 0 && minutes <= MAX_KEEP_AWAKE_MINUTES);
      if (!valid) return reply(400, { error: "minutes must be null (until stopped) or 0 to " + MAX_KEEP_AWAKE_MINUTES });
      setKeepAwake(minutes);
      if (minutes !== 0) touch("keep awake");
      return reply(200, status());
    }
    if (req.method === "POST" && req.url === "/sleep-ack") {
      // The proxy captured this state: no need to wait out the grace period.
      const { sequence } = JSON.parse((await readBody(req)) || "{}");
      const acked = () => phase === "draining" && manifest && sequence === manifest.sequence;
      if (!acked()) return reply(200, status());
      if (client) acks.add(client);
      const waitUntil = Date.now() + OTHERS_WAIT_MS;
      while (acked() && othersCapturing() && Date.now() < waitUntil) await new Promise((r) => setTimeout(r, 250));
      if (!acked()) return reply(200, status());
      // A run started since the last tick (an automation, not via the proxy) shows only here.
      await refreshWorking();
      if (agentsWorking()) touch("agent working");
      // Can't tell: the next tick decides. A hold during the check calls the stop off.
      else if (!unknownBusy() && acked()) finishDrain();
      return reply(200, status());
    }
    reply(404, { error: "not found" });
  } catch (e) {
    reply(400, { error: e.message });
  }
});

server.listen(PORT, "127.0.0.1", () => log(`watchdog on ${PORT}, sleep after ${config.sleepAfterSecs}s`));
// One tick at a time: a slow T3 check must never finish after a newer tick started a stop.
async function loop() {
  await tick().catch((e) => log("tick failed", e.message));
  setTimeout(loop, TICK_MS);
}
loop();

// Boat's free trial runs every sandbox with an auto-stop of at most 2 hours (t3boat asks for
// none; see `boat.rs`). This watchdog decides when the machine sleeps, so while it is up the
// deadline is kept that far ahead: a long agent run is not cut off. With no deadline (a paid
// plan) there is nothing to do.
const TRIAL_TTL_SECS = 7200;
const RENEW_EVERY_MS = 10 * 60_000;
async function renewDeadline() {
  if (phase === "stopping") return;
  const out = await boat(["info", SANDBOX_ID], 20_000);
  const line = out.trim().split("\n").reverse().find((l) => l.startsWith("{"));
  const sandbox = JSON.parse(line ?? "{}").sandbox ?? {};
  // Only the trial's own shape: a deadline from a TTL of at most 2 hours. A machine on a plan
  // runs without one, and this must never add one there.
  const ttl = sandbox.ttlSeconds;
  if (!sandbox.archiveAfter || typeof ttl !== "number" || ttl > TRIAL_TTL_SECS) return;
  await boat(["extend", SANDBOX_ID, "--ttl", String(TRIAL_TTL_SECS)], 20_000);
}
async function renewLoop() {
  await renewDeadline().catch((e) => log("renewing the trial's auto-stop failed", e.message));
  setTimeout(renewLoop, RENEW_EVERY_MS);
}
renewLoop();
