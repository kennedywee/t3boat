import { useEffect, useState } from "react";
import { call, errorText } from "../api";
import type { McpClient, Settings, UiState } from "../types";
import { AgentName, AgentNames } from "./AgentLogos";
import { Icon } from "./Icons";
import { Dots } from "./common";
import { SubHeader } from "./SubHeader";

/** A GitHub owner/name, as the backend accepts it. */
const FLEET_RE = /^[A-Za-z0-9_-][A-Za-z0-9._-]*\/[A-Za-z0-9_-][A-Za-z0-9._-]*$/;

/** Saved on Enter or when the field loses focus: each change re-provisions machines on wake. */
function FleetRepo({ value, onSave }: { value: string; onSave: (repo: string) => void }) {
  const [draft, setDraft] = useState(value);
  useEffect(() => setDraft(value), [value]);
  const repo = draft.trim();
  const valid = repo === "" || FLEET_RE.test(repo);
  const commit = () => {
    if (valid && repo !== value) onSave(repo);
  };
  return (
    <div className="setting field">
      <span className="caption">fleet</span>
      <label className="setting-name" htmlFor="fleet-repo">
        Fleet repo
      </label>
      <span className="prompt-input">
        <span className="prompt-sign" aria-hidden>
          gh
        </span>
        <input
          id="fleet-repo"
          value={draft}
          placeholder="owner/name"
          onChange={(e) => setDraft(e.target.value)}
          onBlur={commit}
          onKeyDown={(e) => e.key === "Enter" && commit()}
          spellCheck={false}
          autoComplete="off"
          aria-invalid={!valid}
          aria-describedby="fleet-repo-hint"
        />
      </span>
      <span id="fleet-repo-hint" className={`hint${valid ? "" : " is-error"}`}>
        {valid
          ? "Skills and agent setup every machine clones and installs once. To update a machine, ask an agent on it for a fleet sync. Empty: none."
          : "Use a GitHub owner/name, like kennedywee/fleet."}
      </span>
    </div>
  );
}

/** A Tailscale MagicDNS machine name (mybox.tailnet.ts.net), as the backend accepts it. */
const TAILSCALE_LABEL = "[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?";
const TAILSCALE_RE = new RegExp(`^${TAILSCALE_LABEL}\\.${TAILSCALE_LABEL}\\.ts\\.net\\.?$`, "i");

/** Saved on Enter, when the field loses focus, or once detected: each change serves the machines on the tailnet. */
function TailscaleHost({ value, onSave }: { value: string; onSave: (host: string) => void }) {
  const [draft, setDraft] = useState(value);
  useEffect(() => setDraft(value), [value]);
  const [detecting, setDetecting] = useState(false);
  const [detectError, setDetectError] = useState<string | null>(null);
  const host = draft.trim();
  const valid = host === "" || TAILSCALE_RE.test(host);
  const commit = (next = host) => {
    if ((next === "" || TAILSCALE_RE.test(next)) && next !== value) onSave(next);
  };
  const detect = () => {
    setDetecting(true);
    setDetectError(null);
    call("detect_tailscale_host", undefined)
      .then(
        (found) => {
          if (!found) return setDetectError("Tailscale isn't running on this computer, or has no MagicDNS name.");
          setDraft(found);
          commit(found);
        },
        (e) => setDetectError(errorText(e)),
      )
      .finally(() => setDetecting(false));
  };
  return (
    <div className="setting field">
      <span className="caption">tailscale</span>
      <label className="setting-name" htmlFor="tailscale-host">
        Tailscale name
      </label>
      <span className="prompt-input">
        <span className="prompt-sign" aria-hidden>
          ts
        </span>
        <input
          id="tailscale-host"
          value={draft}
          placeholder="mybox.tailnet.ts.net"
          onChange={(e) => {
            setDraft(e.target.value);
            setDetectError(null);
          }}
          onBlur={() => commit()}
          onKeyDown={(e) => e.key === "Enter" && commit()}
          spellCheck={false}
          autoComplete="off"
          aria-invalid={!valid}
          aria-describedby="tailscale-host-hint"
        />
        <button className="btn btn-link" disabled={detecting} aria-busy={detecting} onClick={detect}>
          {detecting ? <Dots /> : "Detect"}
        </button>
      </span>
      <span id="tailscale-host-hint" className={`hint${valid && !detectError ? "" : " is-error"}`} aria-live="polite">
        {detectError ??
          (valid
            ? "This computer's name on your tailnet. Machines are then served there with Tailscale Serve, and a phone pairs from a machine's page. Empty: this computer only."
            : "Use this computer's MagicDNS name, like mybox.tail1234.ts.net.")}
      </span>
    </div>
  );
}

const CLIENTS: { id: McpClient; name: string }[] = [
  { id: "claudeCode", name: "Claude Code" },
  { id: "codex", name: "Codex" },
  { id: "antigravity", name: "Antigravity" },
];

interface Props {
  state: UiState;
  onSave: (s: Settings) => void;
  /** never rejects; errors are shown by the caller */
  onAddMcp: (client: McpClient) => Promise<void>;
  /** never rejects; resolves once signed in or given up */
  onConnectUploads: () => Promise<void>;
  onDisconnectUploads: () => void;
  onGithubEverything: (on: boolean) => void;
  onQuit: () => void;
  onBack: () => void;
}

export function SettingsView({
  state,
  onSave,
  onAddMcp,
  onConnectUploads,
  onDisconnectUploads,
  onGithubEverything,
  onQuit,
  onBack,
}: Props) {
  // optimistic copy; the next "state" event is the source of truth
  const [s, setS] = useState(state.settings);
  useEffect(() => setS(state.settings), [state.settings]);

  // hidden by default so screen recordings don't leak the email
  const [showEmail, setShowEmail] = useState(false);
  const email = state.account?.identifier;
  const plan = state.account?.plan;

  const [adding, setAdding] = useState<McpClient | null>(null);
  const add = (client: McpClient) => {
    setAdding(client);
    onAddMcp(client).finally(() => setAdding(null));
  };
  const uploads = state.githubUploads;
  const [connecting, setConnecting] = useState(false);
  const connect = () => {
    setConnecting(true);
    onConnectUploads().finally(() => setConnecting(false));
  };

  // Antigravity is not installed on its own: T3 Code runs it once turned on in its settings.
  const missing = CLIENTS.filter((c) => c.id !== "antigravity" && state.mcp[c.id] === null).map((c) => c.name);
  const antigravityOff = state.mcp.antigravity === null;

  const change = (p: Partial<Settings>) => {
    const next = { ...s, ...p };
    setS(next);
    onSave(next);
  };

  return (
    <div className="view">
      <SubHeader title="Settings" onBack={onBack} />
      <div className="view-body settings">
        <p className="hint">
          Machines sleep 5 minutes after their agents finish and wake by themselves when you use them in T3 Code.
        </p>
        <label className="setting setting-row">
          <span className="setting-name">Start at login</span>
          <input
            type="checkbox"
            role="switch"
            className="switch"
            checked={s.startAtLogin}
            onChange={(e) => change({ startAtLogin: e.target.checked })}
          />
        </label>
        <FleetRepo value={s.fleetRepo ?? ""} onSave={(repo) => change({ fleetRepo: repo || null })} />
        <TailscaleHost value={s.tailscaleHost ?? ""} onSave={(host) => change({ tailscaleHost: host || null })} />
        <div className="setting">
          <span className="caption">github</span>
          <span className="setting-name">GitHub login</span>
          <div className="mcp-buttons">
            {uploads.connected ? (
              <>
                <span className="mcp-added">
                  <Icon name="check" size={10} />
                  <span>{uploads.login ? `Connected as ${uploads.login}` : "Connected"}</span>
                </span>
                <button className="btn btn-sm btn-quiet" onClick={onDisconnectUploads}>
                  Disconnect
                </button>
              </>
            ) : (
              <button className="btn btn-sm btn-outline" disabled={connecting} aria-busy={connecting} onClick={connect}>
                {connecting ? (
                  <>
                    <Dots /> Waiting for GitHub…
                  </>
                ) : (
                  "Connect GitHub"
                )}
              </button>
            )}
          </div>
          <span className="hint" aria-live="polite">
            {uploads.code ? (
              <>
                Enter <code className="step-code">{uploads.code}</code> on the GitHub page that opened.
              </>
            ) : uploads.connected ? (
              "Machines use this login to upload images and videos. Disconnecting takes it off them; to revoke it, remove GitHub CLI under GitHub → Settings → Applications."
            ) : (
              "Lets agents on machines attach images and videos to pull requests and issues (gh … --attach). GitHub refuses those uploads from the login Boat gives a machine, so this adds your own. It can also replace Boat's login for everything."
            )}
          </span>
          {uploads.connected && (
            <label className="toggle">
              <span className="toggle-text">
                <span className="toggle-name">Use for everything</span>
                <span className="hint">
                  {uploads.everything
                    ? "gh and git on machines act as you: every repo you can reach, with your full access"
                    : "gh and git on machines use Boat's login: only the repos its GitHub app can reach"}
                </span>
              </span>
              <input
                type="checkbox"
                role="switch"
                className="switch"
                checked={uploads.everything}
                onChange={(e) => onGithubEverything(e.target.checked)}
              />
            </label>
          )}
        </div>
        <div className="setting">
          <span className="caption">mcp</span>
          <span className="setting-name">
            Let <AgentNames names={CLIENTS.map((c) => c.name)} /> control t3boat
          </span>
          <div className="mcp-buttons">
            {CLIENTS.map((c) => {
              const added = state.mcp[c.id];
              return added ? (
                <span key={c.id} className="mcp-added">
                  <Icon name="check" size={10} />
                  <span>
                    Added to <AgentName name={c.name} />
                  </span>
                </span>
              ) : (
                <button
                  key={c.id}
                  className="btn btn-sm btn-outline"
                  disabled={added === null || adding !== null}
                  aria-busy={adding === c.id}
                  onClick={() => add(c.id)}
                >
                  {adding === c.id ? (
                    <>
                      <Dots /> Adding…
                    </>
                  ) : (
                    <span>
                      Add to <AgentName name={c.name} />
                    </span>
                  )}
                </button>
              );
            })}
          </div>
          <span className="hint">
            {missing.length || antigravityOff ? (
              <>
                {missing.length ? (
                  <>
                    <AgentNames names={missing} /> {missing.length === 1 ? "isn't" : "aren't"} installed on this computer
                    {antigravityOff ? ". " : null}
                  </>
                ) : null}
                {antigravityOff ? (
                  <>
                    Turn on <AgentName name="Antigravity" /> in T3 Code's settings first
                  </>
                ) : null}
              </>
            ) : (
              "Their agents can then create, wake and send work to machines"
            )}
          </span>
        </div>
      </div>
      <div className="view-foot settings-foot">
        <span className="setting-value" title="Boat account">
          {email && (
            <button className="account-email" onClick={() => setShowEmail((v) => !v)}>
              {showEmail ? email : "Show email"}
            </button>
          )}
          {email && plan && " · "}
          {plan}
        </span>
        <button className="btn btn-sm btn-quiet" onClick={onQuit}>
          Quit t3boat
        </button>
      </div>
    </div>
  );
}
