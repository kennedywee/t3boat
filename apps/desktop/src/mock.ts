// Browser-only stand-in for the Tauri backend so `bun run dev` works without Tauri.
// URL params shape the starting state: ?setup=installBoat|signInBoat|installT3|starting
// ?machines=0|many|<count>  ?busy=1  ?mcp=mixed (Claude Code added, Codex and Antigravity not installed)
// ?uploads=1 (GitHub login connected), with &everything=1 (used for all of gh and git)
// ?projects=0|error (T3 Code has no projects with a remote / listing them fails)
// ?ready=1 (a new machine comes up in T3 Code with its agents signed in and acme on it, as the website shows it)
import type { Backend } from "./api";
import { timeLapse } from "./pixel/clock";
import type { BoatSandbox, CommandArgs, CommandName, LocalProject, MachineDetails, MachineProject, MachineView, PortView, Setup, UiState } from "./types";

const params = new URLSearchParams(location.search);

type Seed = Omit<MachineView, "countdown" | "signedOut" | "projects" | "keepAwakeUntil" | "keepAwakeForever"> &
  Partial<Pick<MachineView, "countdown" | "signedOut" | "projects">>;
const machine = (m: Seed): MachineView => ({
  countdown: null,
  signedOut: [],
  projects: [],
  keepAwakeUntil: null,
  keepAwakeForever: false,
  ...m,
});

const onMachine = (title: string, repoKey: string): MachineProject => ({ id: title, title, repoKey, status: null });

/** T3 Code's projects with a git remote: pinned first, then most recent; most have no favicon. */
const LOCAL_PROJECTS: LocalProject[] = [
  { id: "p1", title: "t3code", path: "/Users/you/Documents/Dev/t3code", repoKey: "github.com/pingdotgg/t3code", icon: null },
  { id: "p2", title: "website", path: "/Users/you/Documents/Dev/website", repoKey: "github.com/you/website", icon: null },
  { id: "p3", title: "acme", path: "/Users/you/Documents/Dev/acme", repoKey: "github.com/you/acme", icon: null },
  { id: "p4", title: "dotfiles", path: "/Users/you/dotfiles", repoKey: "github.com/you/dotfiles", icon: null },
];

/** An idle machine's detail: sleeps 5 minutes from now. */
const idle = () => ({ detail: "Idle · sleeps in 5m", countdown: { template: "Idle · sleeps in {left}", endsAt: Date.now() + 5 * 60_000 } });

const WAKING = { state: "waking", detail: "Waking up…", countdown: null } as const;

/** Kept awake until `endsAt` (unix ms). */
const keptAwake = (endsAt: number) => ({
  state: "idle" as const,
  keepAwakeForever: false,
  keepAwakeUntil: endsAt,
  detail: "Kept awake",
  countdown: { template: "Kept awake · {left} left", endsAt },
});

const seedMachines: MachineView[] = [
  // first: asleep until its T3 Code sends a turn
  machine({ id: "sb_e4d1", name: "devbox", size: "default", state: "asleep", detail: "Asleep · free", paired: true, projects: [onMachine("acme", "github.com/you/acme")] }),
  machine({
    id: "sb_7f2a",
    name: "t3-main",
    size: "default",
    state: "working",
    detail: "Working · 2 threads · 3 subagents",
    paired: true,
    projects: [onMachine("t3code", "github.com/pingdotgg/t3code"), onMachine("website", "github.com/you/website")],
  }),
  machine({
    id: "sb_19c0",
    name: "side-quest",
    size: "small",
    state: "idle",
    ...idle(),
    paired: true,
    signedOut: ["Claude", "Codex"],
  }),
  machine({ id: "sb_02bb", name: "scratch", size: "small", state: "idle", detail: "Idle · not in T3 Code yet", paired: false }),
];

const manyExtra: MachineView[] = [
  machine({ id: "sb_aa01", name: "docs-site", size: "small", state: "waking", detail: "Waking up…", paired: true }),
  machine({ id: "sb_aa02", name: "infra", size: "default", state: "error", detail: "Couldn't reach machine", paired: true }),
  machine({ id: "sb_aa03", name: "old-spike", size: "small", state: "missing", detail: "Deleted on Boat", paired: true }),
  machine({ id: "", name: "new-api", size: "default", state: "creating", detail: "Installing T3 server…", paired: false }),
];

/** What listens on each machine, and what is forwarded. */
const mockPorts: Record<string, PortView[]> = {
  sb_7f2a: [
    { port: 4000, name: "flowfokus-server", listening: true, forwarded: true, live: true, error: null, link: null },
    { port: 4001, name: "flowfokus-portal", listening: true, forwarded: true, live: true, error: null, link: "https://box-node-caf79c034da33549-4001.on.boat.dev?_token=mock" },
    { port: 4002, name: "flowfokus-site", listening: true, forwarded: false, live: false, error: null, link: null },
    { port: 54329, name: "flowfokus-db", listening: true, forwarded: false, live: false, error: null, link: null },
  ],
};

/** Machines with T3 Connect on. */
const connectOn = new Set(["sb_7f2a"]);

/** Per-machine extras get_machine_details adds on top of MachineView. */
const seedDetails: Record<string, { awake: number; month: number; days: number }> = {
  sb_7f2a: { awake: 8040, month: 6.4, days: 13 },
  sb_19c0: { awake: 1260, month: 1.1, days: 4 },
  sb_e4d1: { awake: 0, month: 3.9, days: 30 },
};

function seed(): UiState {
  const m = params.get("machines");
  const all = [...seedMachines, ...manyExtra];
  const count = Number(m);
  const machines =
    m === "many"
      ? all
      : count >= 0 && m
        ? Array.from({ length: count }, (_, i) => {
            const base = all[i % all.length];
            const lap = Math.floor(i / all.length);
            return lap ? { ...base, id: base.id && `${base.id}-${lap}`, name: `${base.name}-${lap + 1}` } : base;
          })
        : seedMachines;
  return {
    setup: (params.get("setup") as Setup | null) ?? "ready",
    setupBusy: params.get("busy") === "1",
    signInCode: params.get("busy") === "1" && params.get("setup") === "signInBoat" ? "XM22-RFXJ" : null,
    account: { identifier: "you@example.com", plan: "Pro" },
    machines: structuredClone(machines),
    costs: { today: 0.42, month: 12.8, startsToday: 3, startsLimit: 50 },
    settings: { startAtLogin: true, fleetRepo: null },
    githubUploads: { connected: params.get("uploads") === "1", login: params.get("uploads") === "1" ? "you" : null, everything: params.get("everything") === "1", code: null },
    mcp: params.get("mcp") === "mixed" ? { claudeCode: true, codex: null, antigravity: null } : { claudeCode: false, codex: false, antigravity: false },
    sizes: [
      { id: "small", spec: "2 vCPU · 4 GB", hourly: 0.018 },
      { id: "default", spec: "4 vCPU · 8 GB", hourly: 0.036 },
      { id: "large", spec: "8 vCPU · 16 GB", hourly: 0.072 },
    ],
  };
}

const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** The mock, with T3 Code's side of a machine: what the real backend sees through its proxy. */
export interface MockBackend extends Backend {
  /**
   * A turn sent from T3 Code, a stage at a time for a demo that steps through it: the
   * machine wakes for the message, an agent works on it, and once that is done it idles.
   */
  turn(id: string, stage: "wake" | "work" | "done"): void;
  /**
   * Time flies for an idle machine: its countdown runs out within `ms`, the sea a time lapse
   * meanwhile, then it goes to sleep.
   */
  later(id: string, ms: number): Promise<void>;
}

let shared: MockBackend | null = null;
/** One per page: the UI and whoever plays T3 Code next to it share its state. */
export const mockBackend = (): MockBackend => (shared ??= create());

function create(): MockBackend {
  let state = seed();
  const listeners = new Set<(s: UiState) => void>();

  const emit = () => {
    state = structuredClone(state);
    listeners.forEach((l) => l(state));
  };
  const patch = (id: string, p: Partial<MachineView>) => {
    state.machines = state.machines.map((m) => (m.id === id ? { ...m, ...p } : m));
    emit();
  };
  const find = (id: string) => {
    const m = state.machines.find((x) => x.id === id);
    if (!m) throw "machine not found";
    return m;
  };

  const setupOrder: Setup[] = ["installBoat", "signInBoat", "installT3", "ready"];

  // sandboxes made outside t3boat (?others=0 for none)
  let others: BoatSandbox[] =
    new URLSearchParams(location.search).get("others") === "0"
      ? []
      : [
          { id: "bx_p4s8kye2", name: "t3boat_skye", state: "stopped", type: "default", vcpu: 4, memoryGB: 8, createdAt: "2026-11-16T08:30:00Z", adoptable: true },
          { id: "bx_yq5xkd3s", name: "alpha-trial", state: "stopped", type: "default", vcpu: 4, memoryGB: 8, createdAt: "2026-11-18T13:46:23Z", adoptable: false },
          { id: "bx_k2m9qp7r", name: "scratch", state: "running", type: "small", vcpu: 2, memoryGB: 4, createdAt: "2026-11-17T09:12:00Z", adoptable: false },
        ];

  const handlers: { [C in CommandName]: (a: CommandArgs<C>) => Promise<unknown> } = {
    get_state: async () => structuredClone(state),

    async create_machine({ name, size, projects }) {
      if (state.machines.some((m) => m.name === name)) throw `a machine called "${name}" already exists`;
      const row = machine({ id: "", name, size, state: "creating", detail: "Creating sandbox…", paired: false });
      state.machines = [...state.machines, row];
      emit();
      const steps = ["Creating sandbox…", "Installing T3 server…", "Starting T3 server…", "Asking T3 Code to add it…"];
      for (const step of steps) {
        state.machines = state.machines.map((m) => (m.name === name ? { ...m, detail: step } : m));
        emit();
        await wait(1800);
      }
      const id = `sb_${Math.random().toString(16).slice(2, 6)}`;
      state.machines = state.machines.map((m) =>
        m.name === name
          ? {
              ...m,
              id,
              state: "idle",
              ...idle(),
              ...(params.get("ready") === "1"
                ? { paired: true, projects: projects?.length ? [] : [onMachine("acme", "github.com/you/acme")] }
                : { signedOut: ["Claude", "Codex"] }),
            }
          : m,
      );
      state.costs.startsToday += 1;
      emit();
      for (const p of projects ?? []) await handlers.add_project({ id, projectId: p });
    },

    async local_projects() {
      await wait(400);
      if (params.get("projects") === "error") throw "T3 Code is not running";
      return params.get("projects") === "0" ? [] : LOCAL_PROJECTS;
    },

    async add_project({ id, projectId }) {
      const p = LOCAL_PROJECTS.find((x) => x.id === projectId);
      if (!p) throw "no such project";
      const set = (status: string | null) =>
        patch(id, { projects: [...find(id).projects.filter((x) => x.repoKey !== p.repoKey), { id: p.id, title: p.title, repoKey: p.repoKey, status }] });
      set("Waking the machine");
      await wait(900);
      for (const pct of [8, 35, 71, 100]) {
        set(`Receiving objects · ${pct}% · 3.1 MiB/s`);
        await wait(700);
      }
      set("Checking out files");
      await wait(600);
      set(null);
    },

    async omarchy_theme() {
      return null;
    },

    async other_sandboxes() {
      await wait(400);
      return others.map((s) => ({ ...s }));
    },

    async set_sandbox_running({ id, running }) {
      const s = others.find((o) => o.id === id);
      if (!s) throw `no sandbox ${id}`;
      s.state = running ? "starting" : "stopping";
      await wait(1800);
      s.state = running ? "running" : "stopped";
    },

    async adopt_machine({ id }) {
      const s = others.find((o) => o.id === id && o.adoptable);
      if (!s) throw `${id} is not a t3boat machine from another computer`;
      const name = (s.name ?? "").replace(/^t3boat_/, "");
      if (state.machines.some((m) => m.name === name)) throw `a machine called ${name} already exists`;
      const row = machine({ id: "", name, size: s.type ?? "default", state: "creating", detail: "Waking the machine", paired: false });
      state.machines = [...state.machines, row];
      emit();
      await wait(2400);
      state.machines = state.machines.map((m) => (m.name === name ? { ...m, detail: "Reading the machine" } : m));
      emit();
      await wait(1800);
      others = others.filter((o) => o.id !== id);
      state.machines = state.machines.map((m) => (m.name === name ? { ...m, id, state: "idle", ...idle() } : m));
      emit();
    },

    async wake_machine({ id }) {
      find(id);
      patch(id, WAKING);
      await wait(2600);
      patch(id, { state: "idle", ...idle() });
      state.costs.startsToday += 1;
      emit();
    },

    async sleep_machine({ id }) {
      if (find(id).state === "working") throw "an agent is working";
      patch(id, { state: "sleeping", detail: "Going to sleep…", countdown: null });
      await wait(1800);
      // A stop ends any keep-awake, as on a real machine.
      patch(id, { state: "asleep", detail: "Asleep · free", keepAwakeUntil: null, keepAwakeForever: false });
    },

    async keep_awake({ id, minutes }) {
      await wait(600);
      patch(
        id,
        minutes === null
          ? { keepAwakeForever: true, keepAwakeUntil: null, state: "idle", detail: "Awake until you stop it", countdown: null }
          : minutes
            ? keptAwake(Date.now() + minutes * 60_000)
            : { keepAwakeForever: false, keepAwakeUntil: null, ...idle() },
      );
    },

    async resize_machine({ id, size }) {
      const m = find(id);
      if (m.state === "idle") {
        patch(id, { state: "sleeping", detail: "Moving to " + size + "…", countdown: null });
        await wait(1600);
        patch(id, WAKING);
        await wait(2200);
        patch(id, { size, state: "idle", ...idle() });
      } else {
        patch(id, { size });
      }
    },

    async turn_on_connect({ id }) {
      await wait(4000);
      find(id);
      connectOn.add(id);
      emit(); // the details view refetches on every state event
    },

    async sign_in_agent({ id, agent }) {
      await wait(3000);
      patch(id, { signedOut: find(id).signedOut.filter((a) => a !== agent) });
    },

    async open_machine({ id }) {
      const m = find(id);
      if (!m.paired) {
        await wait(1200);
        patch(id, { paired: true, ...idle() });
      }
      // pretend the user finishes the agent sign-ins in T3 Code
      if (m.signedOut.length) setTimeout(() => patch(id, { signedOut: [] }), 4000);
    },

    async pair_machine({ id }) {
      find(id);
      await wait(1200);
      patch(id, { paired: true });
    },

    async delete_machine({ id, discard }) {
      if (find(id).state === "working" && !discard) {
        await wait(400);
        throw "work is still running on it (an agent or a build); wait for it to finish, or delete and discard";
      }
      await wait(500);
      state.machines = state.machines.filter((x) => x.id !== id);
      emit();
    },

    async latest_thread({ id }) {
      await wait(150);
      return id === "sb_7f2a" ? "th_1" : null;
    },

    async get_machine_details({ id }): Promise<MachineDetails> {
      const m = find(id);
      await wait(350);
      const size = state.sizes.find((s) => s.id === m.size);
      const extra = seedDetails[id] ?? { awake: 0, month: 0, days: 0 };
      const awake = extra.awake + (m.state === "working" || m.state === "idle" ? 300 : 0);
      const costToday = (awake / 3600) * (size?.hourly ?? 0);
      return {
        id,
        costToday,
        costMonth: extra.month + costToday,
        awakeSecsToday: awake,
        agents: ["Claude", "Codex"].map((name) => ({ name, signedIn: !m.signedOut.includes(name) })),
        t3Version: "0.0.21",
        createdAt: Math.floor(Date.now() / 1000) - extra.days * 86400,
        connect: connectOn.has(id),
        ports: mockPorts[id] ?? [],
      };
    },

    async scan_ports() {
      await wait(400);
    },

    async forward_ports({ id, ports, on }) {
      await wait(700);
      const known = (mockPorts[id] ??= []);
      for (const port of ports) {
        if (on && port === 4002) throw "port 4002 is in use on this computer";
        const row = known.find((p) => p.port === port);
        if (row) Object.assign(row, { forwarded: on, live: on });
        else if (on) known.push({ port, name: null, listening: false, forwarded: true, live: true, error: null, link: null });
      }
      mockPorts[id] = known.filter((p) => p.listening || p.forwarded || p.link).sort((a, b) => a.port - b.port);
      emit();
    },

    async link_port({ id, port, on }) {
      await wait(900);
      const row = (mockPorts[id] ?? []).find((p) => p.port === port);
      if (!row?.listening && on) throw `nothing listens on port ${port} of the machine yet`;
      if (row) row.link = on ? `https://box-node-caf79c034da33549-${port}.on.boat.dev?_token=mock` : null;
      emit();
      return row?.link ?? null;
    },

    async open_port() {},

    async add_mcp({ client }) {
      if (state.mcp[client] === null) throw "not installed";
      await wait(900);
      state.mcp = { ...state.mcp, [client]: true };
      emit();
    },

    async connect_github_uploads() {
      state.githubUploads = { connected: false, login: null, everything: false, code: "9F3A-C0DE" };
      emit();
      await wait(4000);
      state.githubUploads = { connected: true, login: "you", everything: false, code: null };
      emit();
    },

    async disconnect_github_uploads() {
      state.githubUploads = { connected: false, login: null, everything: false, code: null };
      emit();
    },

    async set_github_everything({ on }) {
      state.githubUploads = { ...state.githubUploads, everything: on };
      emit();
    },

    async update_settings({ settings }) {
      state.settings = settings;
      emit();
    },

    async setup_action({ action }) {
      state.setupBusy = true;
      emit();
      await wait(1600);
      state.setupBusy = false;
      if (action === "recheck" || action === state.setup) {
        state.setup = setupOrder[setupOrder.indexOf(state.setup) + 1] ?? "ready";
      }
      emit();
    },

    async quit_app() {
      console.info("[mock] quit_app");
    },
  };

  // The watchdog: a timed keep-awake that ran out falls back to the idle countdown, and an
  // idle machine sleeps when its countdown runs out.
  setInterval(() => {
    const now = Date.now();
    state.machines
      .filter((m) => m.state === "idle" && m.keepAwakeUntil !== null && m.keepAwakeUntil <= now)
      .forEach((m) => patch(m.id, { keepAwakeUntil: null, ...idle() }));
    state.machines
      .filter((m) => m.state === "idle" && !m.keepAwakeUntil && m.countdown && m.countdown.endsAt <= now)
      .forEach((m) => void handlers.sleep_machine({ id: m.id }));
  }, 500);

  if (state.setup === "starting") {
    setTimeout(() => {
      state.setup = "installBoat";
      emit();
    }, 1500);
  }

  return {
    async call(cmd, args) {
      await wait(120);
      return (handlers[cmd] as (a: typeof args) => Promise<never>)(args);
    },
    subscribe(cb) {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    turn(id, stage) {
      if (stage === "wake") state.costs.startsToday += 1;
      patch(
        id,
        stage === "wake"
          ? WAKING
          : stage === "work"
            ? { state: "working", detail: "Working · 1 thread", countdown: null }
            : { state: "idle", ...idle() },
      );
    },
    async later(id, ms) {
      const { countdown } = find(id);
      const left = countdown ? countdown.endsAt - Date.now() : 0;
      const ticks = Math.round(ms / 100);
      timeLapse(left / ms);
      for (let i = 1; countdown && i <= ticks; i++) {
        await wait(ms / ticks);
        // woken, slept or deleted meanwhile: no more time to skip
        if (state.machines.find((m) => m.id === id)?.state !== "idle") break;
        patch(id, { countdown: { ...countdown, endsAt: Date.now() + (left * (ticks - i)) / ticks } });
      }
      timeLapse(1);
      if (state.machines.find((m) => m.id === id)?.state === "idle") await handlers.sleep_machine({ id });
    },
  };
}
