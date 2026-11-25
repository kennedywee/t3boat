import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { call, errorText, useMachineDetails, useLatestThread } from "../api";
import type { ClientToken, MachineDetails as Details, LocalProject, MachineView, PortView, Size, TailnetPairing } from "../types";
import {
  ACTIONS,
  autoSleep,
  duration,
  hourly,
  STAY_AWAKE,
  keepAwake,
  machineKey,
  money,
  powerAction,
  openIn,
  parsePorts,
  primaryAction,
  resizeHint,
  resizeTo,
  seedOf,
  shortDate,
  signInTo,
  sizeOf,
  timeLeft,
  type Action,
} from "../machine";
import { AgentName } from "./AgentLogos";
import { BoatIcon } from "./BoatIcon";
import { DeleteErrorNote, Dots, useArmed, useDetail, useEscape, useSecsLeft } from "./common";
import { Icon } from "./Icons";
import { ProjectPicker } from "./ProjectPicker";
import { SubHeader } from "./SubHeader";

interface Props {
  machine: MachineView;
  sizes: Size[];
  /** this computer's Tailscale name; null: phones can't pair over the tailnet */
  tailscaleHost: string | null;
  /** latest UiState; every new one triggers a details refetch */
  version: unknown;
  pending: string | null;
  deleteError: string | null;
  /** opened from the row's "Add a project…" or "Add one": start with the project picker open */
  pickProject: boolean;
  onAction: (a: Action) => void;
  onAddProject: (p: LocalProject) => void;
  onDismissDeleteError: () => void;
  onError: (text: string) => void;
  /** a failed change made from this view */
  onFailed: (text: string) => void;
  onBack: () => void;
}

export function MachineDetails({
  machine: m,
  sizes,
  tailscaleHost,
  version,
  pending,
  deleteError,
  pickProject,
  onAction,
  onAddProject,
  onDismissDeleteError,
  onError,
  onFailed,
  onBack,
}: Props) {
  const d = useMachineDetails(m.id, version, onError);
  const busy = !!pending;
  const gone = m.state === "missing";
  const latestThread = useLatestThread(m.id);
  const primary = primaryAction(m) ?? (m.paired ? openIn(latestThread) : ACTIONS.pair);
  const [primaryArmed, armPrimary] = useArmed();

  const runPrimary = () =>
    primary.cmd === "delete_machine" ? armPrimary(() => onAction(primary)) : onAction(primary);

  return (
    <div className={`view details state-${m.state}`}>
      <SubHeader title={m.name} onBack={onBack} />
      <div className="view-body details-body">
        <Glance m={m} size={sizeOf(sizes, m.size)} d={d} />
        {gone ? null : <Power m={m} busy={busy} onAction={onAction} />}
        <Agents m={m} d={d} pending={pending} onAction={onAction} />
        <Connect m={m} d={d} pending={pending} onAction={onAction}>
          <TailnetDevices m={m} tailscaleHost={tailscaleHost} version={version} onError={onFailed} />
        </Connect>
        <Projects m={m} pickProject={pickProject} onAddProject={onAddProject} />
        {gone ? null : <Ports m={m} d={d} onError={onFailed} />}
        <MachineMore
          m={m}
          sizes={sizes}
          busy={busy}
          deleteError={deleteError}
          onAction={onAction}
          onDismissDeleteError={onDismissDeleteError}
        />
        <p className="details-meta">
          {d ? (
            <>
              <span>T3 {d.t3Version}</span>
              <span className="sep">·</span>
              <span>created {shortDate(d.createdAt)}</span>
              <span className="sep">·</span>
              <span className="selectable details-id">{m.id}</span>
            </>
          ) : (
            <span className="skel skel-meta" />
          )}
        </p>
      </div>
      <div className="view-foot details-foot">
        <button
          className={`btn btn-lg ${primaryArmed ? "btn-danger" : primary.solid ? "btn-solid" : "btn-outline"}`}
          disabled={busy}
          aria-busy={busy}
          onClick={runPrimary}
        >
          {pending ? (
            <>
              <Dots /> {pending}
            </>
          ) : primaryArmed ? (
            "Confirm"
          ) : (
            <>
              {primary.label}
              {primary.cmd === "open_machine" && !primary.solid ? <Icon name="open" size={10} /> : null}
            </>
          )}
        </button>
      </div>
    </div>
  );
}

/** What a user scans for first: is it working, what is it, what does it cost. */
function Glance({ m, size, d }: { m: MachineView; size: Size | undefined; d: Details | null }) {
  // "Working · 2 threads · 3 subagents" → headline "Working", the rest underneath
  const detail = useDetail(m);
  const [headline, ...rest] = detail.split(" · ");
  return (
    <section className="glance" aria-label={`${m.name}: ${detail}`}>
      <div className="glance-top">
        <div className="glance-boat">
          <BoatIcon state={m.state} seed={seedOf(machineKey(m))} />
        </div>
        <div className="glance-text">
          <span className="glance-state">
            <i className="state-dot" aria-hidden />
            {headline}
          </span>
          {rest.length ? <span className="glance-detail">{rest.join(" · ")}</span> : null}
          <span className="glance-spec">
            <span className="chip">{m.size}</span>
            {size ? (
              <>
                <span>{size.spec}</span>
                <span className="glance-rate">{hourly(size.hourly)}</span>
              </>
            ) : null}
          </span>
        </div>
      </div>
      <div className="glance-costs">
        <Stat label="today" value={d && money(d.costToday)} />
        <Stat label="this month" value={d && money(d.costMonth)} />
        <Stat label="awake today" value={d && duration(d.awakeSecsToday)} />
      </div>
    </section>
  );
}

function Stat({ label, value }: { label: string; value: string | null }) {
  return (
    <div className="cost">
      <span className="caption">{label}</span>
      <span className="cost-value">{value ?? <span className="skel skel-value" />}</span>
    </div>
  );
}

function Section({ title, count, extra, children }: { title: string; count?: number; extra?: ReactNode; children: ReactNode }) {
  return (
    <section className="section">
      <div className="section-head">
        <span className="caption">
          {title}
          {count ? <span className="count">{count}</span> : null}
        </span>
        <span className="section-rule" />
        {extra}
      </div>
      {children}
    </section>
  );
}

function Power({ m, busy, onAction }: { m: MachineView; busy: boolean; onAction: (a: Action) => void }) {
  const power = powerAction(m);
  // optimistic, so the switch moves on click; the next state is the source of truth
  const [want, setWant] = useState<boolean | null>(null);
  useEffect(() => {
    if (!busy) setWant(null);
  }, [busy]);
  const on = want ?? !m.keepAwakeForever;

  return (
    <Section title="power">
      <label className="toggle">
        <span className="toggle-text">
          <span className="toggle-name">Auto sleep</span>
          <span className="hint">
            {on ? "Sleeps 5 min after agents finish; asleep is free" : "Stays up and bills until you turn this back on"}
          </span>
        </span>
        <input
          type="checkbox"
          role="switch"
          className="switch"
          checked={on}
          disabled={busy}
          onChange={(e) => {
            setWant(e.target.checked);
            onAction(autoSleep(e.target.checked));
          }}
        />
      </label>
      <div className="power-row">
        <button className="btn btn-sm btn-quiet" disabled={busy || !power} onClick={() => power && onAction(power)}>
          {(power ?? ACTIONS.sleep).label}
        </button>
        {on ? <StayAwake m={m} busy={busy} onAction={onAction} /> : null}
      </div>
    </Section>
  );
}

/** Like caffeinate: a one-time "stay up at least this long", which simply runs out. */
function StayAwake({ m, busy, onAction }: { m: MachineView; busy: boolean; onAction: (a: Action) => void }) {
  const [open, setOpen] = useState(false);
  const close = useCallback(() => setOpen(false), []);
  useEscape(open, close);
  const left = useSecsLeft(m.keepAwakeUntil);

  if (m.keepAwakeUntil !== null) {
    return (
      <div className="stay-awake is-on" role="status">
        <i className="state-dot" aria-hidden />
        <span className="stay-awake-left">Awake · {timeLeft(left)} left</span>
        <button className="btn btn-link" disabled={busy} onClick={() => onAction(keepAwake(0))}>
          Stop
        </button>
      </div>
    );
  }
  return (
    <div className="stay-awake">
      <button
        className={`btn btn-sm btn-quiet stay-awake-btn${open ? " is-open" : ""}`}
        aria-expanded={open}
        disabled={busy}
        onClick={() => setOpen((o) => !o)}
      >
        Stay awake for…
        <Icon name="chevron" size={8} />
      </button>
      {open ? (
        <div className="segmented stay-awake-options" role="menu">
          {STAY_AWAKE.map((min) => (
            <button
              key={min}
              role="menuitem"
              onClick={() => {
                setOpen(false);
                onAction(keepAwake(min));
              }}
            >
              {duration(min * 60)}
            </button>
          ))}
        </div>
      ) : null}
    </div>
  );
}

function Agents({
  m,
  d,
  pending,
  onAction,
}: {
  m: MachineView;
  d: Details | null;
  pending: string | null;
  onAction: (a: Action) => void;
}) {
  const [signingIn, setSigningIn] = useState<string | null>(null);
  return (
    <Section title="agents">
      <ul className="lines">
        {d ? (
          d.agents.map((a) => {
            const running = signingIn === a.name && pending === signInTo(a.name).busy;
            return (
              <li key={a.name} className="line">
                <span className="line-main">
                  <AgentName name={a.name} />
                </span>
                {a.signedIn ? (
                  <span className="line-ok">
                    <Icon name="check" size={10} /> signed in
                  </span>
                ) : m.signedOut.includes(a.name) ? (
                  <button
                    className="btn btn-sm btn-outline"
                    disabled={!!pending || m.state === "missing"}
                    aria-busy={running}
                    onClick={() => {
                      setSigningIn(a.name);
                      onAction(signInTo(a.name));
                    }}
                  >
                    {running ? (
                      <>
                        <Dots /> Signing in…
                      </>
                    ) : (
                      "Sign in"
                    )}
                  </button>
                ) : (
                  <span className="caption">signed out</span>
                )}
              </li>
            );
          })
        ) : (
          <Skeleton rows={2} />
        )}
      </ul>
    </Section>
  );
}

/** T3 Connect: the user's phone or a browser reach the machine with their T3 account. */
function Connect({
  m,
  d,
  pending,
  onAction,
  children,
}: {
  m: MachineView;
  d: Details | null;
  pending: string | null;
  onAction: (a: Action) => void;
  /** other ways in, after T3 Connect */
  children?: ReactNode;
}) {
  const running = pending === ACTIONS.connect.busy;
  return (
    <Section title="other devices">
      <ul className="lines">
        {d ? (
          <>
            <li className="line">
              <span className="line-main">T3 Connect</span>
              {d.connect ? (
                <span className="line-ok">
                  <Icon name="check" size={10} /> on
                </span>
              ) : (
                <button
                  className="btn btn-sm btn-outline"
                  disabled={!!pending || m.state === "missing"}
                  aria-busy={running}
                  onClick={() => onAction(ACTIONS.connect)}
                >
                  {running ? (
                    <>
                      <Dots /> {ACTIONS.connect.busy}
                    </>
                  ) : (
                    ACTIONS.connect.label
                  )}
                </button>
              )}
            </li>
            {d.connect ? null : (
              <li className="line">
                <span className="line-main line-dim">T3 Code on your phone or in a browser, with your T3 account</span>
              </li>
            )}
          </>
        ) : (
          <Skeleton rows={1} />
        )}
        {children}
      </ul>
    </Section>
  );
}

/**
 * Pairing a phone over the tailnet with a QR code, and the devices paired that way. Revoking one
 * unpairs that device only.
 */
function TailnetDevices({
  m,
  tailscaleHost,
  version,
  onError,
}: {
  m: MachineView;
  tailscaleHost: string | null;
  /** latest UiState: a device pairing or a revoke arrives as a new one */
  version: unknown;
  onError: (text: string) => void;
}) {
  const [devices, setDevices] = useState<ClientToken[]>([]);
  const [pairing, setPairing] = useState<(TailnetPairing & { before: string[] }) | null>(null);
  const [starting, setStarting] = useState(false);
  const [copied, setCopied] = useState(false);
  const fail = (e: unknown) => onError(errorText(e));

  useEffect(() => {
    let live = true;
    call("list_client_tokens", { id: m.id }).then(
      (tokens) => live && setDevices(tokens.filter((t) => t.tailnet)),
      () => {}, // the details view already reports a machine it can't read
    );
    return () => {
      live = false;
    };
  }, [m.id, version]);

  // the phone paired: the code is spent, so the QR code goes
  useEffect(() => {
    if (pairing && devices.some((t) => !pairing.before.includes(t.id))) setPairing(null);
  }, [pairing, devices]);

  const pair = () => {
    setStarting(true);
    call("pair_machine_tailnet", { id: m.id })
      .then((p) => setPairing({ ...p, before: devices.map((t) => t.id) }), fail)
      .finally(() => setStarting(false));
  };
  const copy = () =>
    navigator.clipboard.writeText(pairing?.url ?? "").then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    }, fail);

  return (
    <>
      {tailscaleHost ? (
        <li className="line">
          <span className="line-main">Phone on your tailnet</span>
          {pairing ? (
            <button className="btn btn-link" onClick={() => setPairing(null)}>
              Done
            </button>
          ) : (
            <button
              className="btn btn-sm btn-outline"
              disabled={starting || m.state === "missing"}
              aria-busy={starting}
              onClick={pair}
            >
              {starting ? (
                <>
                  <Dots /> Starting…
                </>
              ) : (
                "Pair phone"
              )}
            </button>
          )}
        </li>
      ) : null}
      {pairing ? (
        <li className="line pair-qr">
          <img src={`data:image/svg+xml;charset=utf-8,${encodeURIComponent(pairing.svg)}`} alt="QR code of the pairing link" />
          <span className="hint">
            Scan this QR code or paste the link in T3 Mobile's Add environment screen (not in a browser). It works once,
            within 15 minutes. If the phone's T3 Code already has this machine through T3 Connect, remove it there first,
            or add this link to it as another route.
          </span>
          <div className="port-share">
            <span className="caption selectable port-host" title={pairing.url}>
              {new URL(pairing.url).host}
            </span>
            <button className="btn btn-link" onClick={copy}>
              {copied ? "Copied" : "Copy link"}
            </button>
          </div>
        </li>
      ) : null}
      {devices.map((t) => (
        <TailnetDevice key={t.id} id={m.id} t={t} onError={onError} />
      ))}
    </>
  );
}

function TailnetDevice({ id, t, onError }: { id: string; t: ClientToken; onError: (text: string) => void }) {
  const [armed, arm] = useArmed();
  const [revoking, setRevoking] = useState(false);
  const revoke = () => {
    setRevoking(true);
    call("revoke_client_token", { id, tokenId: t.id })
      .catch((e) => onError(errorText(e)))
      .finally(() => setRevoking(false));
  };
  return (
    <li className="line">
      <span className="line-main">{t.label ?? "Device"}</span>
      {t.createdAt ? <span className="caption">paired {shortDate(t.createdAt)}</span> : null}
      <button
        className={`btn btn-link${armed ? " is-danger" : ""}`}
        disabled={revoking}
        aria-busy={revoking}
        onClick={() => arm(revoke)}
      >
        {revoking ? <Dots /> : armed ? "Confirm" : "Revoke"}
      </button>
    </li>
  );
}

/** Its T3 projects, and adding one the way T3 Code's "Add to another machine" does. */
function Projects({
  m,
  pickProject,
  onAddProject,
}: {
  m: MachineView;
  pickProject: boolean;
  onAddProject: (p: LocalProject) => void;
}) {
  const gone = m.state === "missing";
  if (gone && !m.projects.length) return null;
  return (
    <Section title="projects" count={m.projects.length}>
      <ul className="lines">
        {m.projects.map((p) => (
          <li key={p.id} className="line">
            <span className="line-main" title={p.title}>
              {p.title}
            </span>
            {p.status ? (
              <span className="line-status caption" title={p.status}>
                <Dots /> {p.status}
              </span>
            ) : null}
          </li>
        ))}
        {!m.projects.length && !gone ? (
          <li className="line">
            <span className="line-main line-dim">Cloned from its git remote, .env files included</span>
          </li>
        ) : null}
        {gone ? null : (
          <li className="line line-send">
            <ProjectPicker m={m} initialOpen={pickProject} onAdd={onAddProject} />
          </li>
        )}
      </ul>
    </Section>
  );
}

/** Its dev servers in this computer's browser: each forwarded port is `localhost:<port>` here too. */
function Ports({ m, d, onError }: { m: MachineView; d: Details | null; onError: (text: string) => void }) {
  const up = m.state === "working" || m.state === "idle";
  const [busy, setBusy] = useState<Record<number, boolean>>({});
  const [scanning, setScanning] = useState(false);
  const [text, setText] = useState("");

  // a ref: a new `onError` must not start another scan
  const failed = useRef(onError);
  failed.current = onError;
  const scan = useCallback(() => {
    setScanning(true);
    call("scan_ports", { id: m.id })
      .catch((e) => failed.current(errorText(e)))
      .finally(() => setScanning(false));
  }, [m.id]);
  // on open, and again when it wakes: what listens changes while it sleeps for nobody
  useEffect(() => {
    if (up) scan();
  }, [up, scan]);

  const forward = (ports: number[], on: boolean) => {
    if (!ports.length) return Promise.resolve(false);
    setBusy((b) => ({ ...b, ...Object.fromEntries(ports.map((p) => [p, on])) }));
    return call("forward_ports", { id: m.id, ports, on })
      .then(
        () => true,
        (e) => (onError(errorText(e)), false),
      )
      .finally(() => setBusy((b) => Object.fromEntries(Object.entries(b).filter(([p]) => !ports.includes(Number(p))))));
  };

  const add = () => {
    try {
      forward(parsePorts(text), true).then((ok) => ok && setText(""));
    } catch (e) {
      onError(errorText(e));
    }
  };

  const ports = d?.ports ?? [];
  const rest = ports.filter((p) => p.listening && !p.forwarded).map((p) => p.port);
  const working = Object.keys(busy).length > 0;
  return (
    <Section
      title="ports"
      count={ports.filter((p) => p.forwarded).length}
      extra={
        <>
          {rest.length > 1 ? (
            <button className="btn btn-link" disabled={working} onClick={() => forward(rest, true)}>
              Forward all
            </button>
          ) : null}
          {up ? (
            <button className="btn btn-link" disabled={scanning} onClick={scan}>
              {scanning ? "Looking…" : "Rescan"}
            </button>
          ) : null}
        </>
      }
    >
      <ul className="lines">
        {d ? ports.map((p) => <PortLine key={p.port} id={m.id} p={p} want={busy[p.port]} onForward={forward} onError={onError} />) : <Skeleton rows={2} />}
        {d && !ports.length ? (
          <li className="line">
            <span className="line-main line-dim">
              {up ? (scanning ? "Looking for dev servers…" : "No dev servers found") : "Dev servers show once it is awake"}
            </span>
          </li>
        ) : null}
        <li className="line">
          <form
            className="port-add"
            onSubmit={(e) => {
              e.preventDefault();
              add();
            }}
          >
            <input
              value={text}
              onChange={(e) => setText(e.target.value)}
              placeholder="Port or range: 5173, 3000-3010"
              aria-label="Port or range to forward"
              spellCheck={false}
            />
            <button className="btn btn-sm btn-quiet" disabled={!text.trim() || working}>
              Forward
            </button>
          </form>
        </li>
      </ul>
      <span className="hint port-hint">
        The switch brings a port to the same port on this computer. A link opens anywhere, if the app accepts its host name.
      </span>
    </Section>
  );
}

function PortLine({
  id,
  p,
  want,
  onForward,
  onError,
}: {
  id: string;
  p: PortView;
  /** the switch's position while a change runs */
  want: boolean | undefined;
  onForward: (ports: number[], on: boolean) => void;
  onError: (text: string) => void;
}) {
  const [linking, setLinking] = useState(false);
  const [copied, setCopied] = useState(false);
  const note = p.error ?? (p.listening ? p.name : "nothing listening");
  const fail = (e: unknown) => onError(errorText(e));

  const link = (on: boolean) => {
    setLinking(true);
    call("link_port", { id, port: p.port, on })
      .catch(fail)
      .finally(() => setLinking(false));
  };
  const copy = () =>
    navigator.clipboard.writeText(p.link ?? "").then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    }, fail);

  return (
    <li className="line port-line">
      <div className="port-top">
        <span className="line-main port" title={p.error ?? undefined}>
          {p.live ? (
            <button
              className="btn btn-link port-open"
              onClick={() => call("open_port", { id, port: p.port, link: false }).catch(fail)}
            >
              localhost:{p.port}
              <Icon name="open" size={8} />
            </button>
          ) : (
            <span className="port-number">{p.port}</span>
          )}
          {note ? <span className={`caption port-note${p.error ? " is-error" : ""}`}>{note}</span> : null}
        </span>
        {want !== undefined || linking ? <Dots /> : null}
        {p.link || !p.listening ? null : (
          <button className="btn btn-link" disabled={linking} onClick={() => link(true)}>
            Link
          </button>
        )}
        <input
          type="checkbox"
          role="switch"
          className="switch"
          aria-label={`Forward port ${p.port} to this computer`}
          checked={want ?? p.forwarded}
          disabled={want !== undefined}
          onChange={(e) => onForward([p.port], e.target.checked)}
        />
      </div>
      {p.link ? (
        <div className="port-share">
          <span className="caption selectable port-host" title="Opens on any device. The token in the link is the access.">
            {new URL(p.link).host}
          </span>
          <button className="btn btn-link" onClick={() => call("open_port", { id, port: p.port, link: true }).catch(fail)}>
            Open
          </button>
          <button className="btn btn-link" onClick={copy}>
            {copied ? "Copied" : "Copy"}
          </button>
          <button className="btn btn-link" disabled={linking} onClick={() => link(false)}>
            Remove
          </button>
        </div>
      ) : null}
    </li>
  );
}

/** The rare stuff: size and delete, folded away. */
function MachineMore({
  m,
  sizes,
  busy,
  deleteError,
  onAction,
  onDismissDeleteError,
}: {
  m: MachineView;
  sizes: Size[];
  busy: boolean;
  deleteError: string | null;
  onAction: (a: Action) => void;
  onDismissDeleteError: () => void;
}) {
  const ref = useRef<HTMLDetailsElement>(null);
  const [deleteArmed, armDelete] = useArmed();
  const size = sizeOf(sizes, m.size);
  const gone = m.state === "missing";

  // a refused delete (from here or the list) must be visible
  useEffect(() => {
    if (deleteError && ref.current) ref.current.open = true;
  }, [deleteError]);

  return (
    <details ref={ref} className="section more">
      <summary className="section-head">
        <span className="caption">machine</span>
        <span className="section-rule" />
        <span className="caption more-peek">
          {gone ? "gone" : size ? `${m.size} · ${hourly(size.hourly)}` : m.size}
        </span>
        <Icon name="chevron" size={8} />
      </summary>
      <div className="more-body">
        {gone ? null : (
          <div className="more-group">
            <span className="toggle-name">Size</span>
            <div className="segmented" role="radiogroup" aria-label="Size">
              {sizes.map((s) => (
                <button
                  key={s.id}
                  role="radio"
                  aria-checked={s.id === m.size}
                  className={s.id === m.size ? "is-on" : undefined}
                  disabled={busy}
                  onClick={() => s.id !== m.size && onAction(resizeTo(s.id))}
                >
                  {s.id}
                  <small>{s.spec}</small>
                  <small>{hourly(s.hourly)}</small>
                </button>
              ))}
            </div>
            <span className="hint">{resizeHint(m)}</span>
          </div>
        )}
        <div className="more-group">
          <button
            className={`btn btn-sm ${deleteArmed ? "btn-danger" : "btn-quiet is-danger"} delete-btn`}
            disabled={busy}
            onClick={() => armDelete(() => onAction(ACTIONS.del))}
          >
            {deleteArmed ? "Click again to delete forever" : "Delete machine…"}
          </button>
          {deleteError ? (
            <DeleteErrorNote
              error={deleteError}
              busy={busy}
              onDeleteAnyway={() => onAction(ACTIONS.deleteAnyway)}
              onKeep={onDismissDeleteError}
            />
          ) : null}
        </div>
      </div>
    </details>
  );
}

function Skeleton({ rows }: { rows: number }) {
  return (
    <>
      {Array.from({ length: rows }, (_, i) => (
        <li key={i} className="line" aria-hidden>
          <span className="skel" style={{ width: `${40 - i * 10}%` }} />
        </li>
      ))}
    </>
  );
}
