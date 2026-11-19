import { useCallback, useEffect, useState } from "react";
import { call, errorText } from "../api";
import { seedOf } from "../machine";
import type { BoatSandbox, MachineState } from "../types";
import { BoatIcon } from "./BoatIcon";
import { Dots } from "./common";

/** How often the list is read again while it is on screen. */
const REFRESH_MS = 30_000;

/** Boat's state word as the boat icon's state. */
function iconState(state: string): MachineState {
  if (state === "stopped") return "asleep";
  if (state === "stopping") return "sleeping";
  if (state === "starting" || state === "resuming") return "waking";
  return "idle";
}

const running = (s: BoatSandbox) => s.state !== "stopped" && s.state !== "stopping";

function sizeText(s: BoatSandbox): string {
  const parts = [s.type, s.vcpu ? `${s.vcpu} vCPU` : null, s.memoryGB ? `${s.memoryGB} GB` : null];
  return parts.filter(Boolean).join(" · ");
}

interface Props {
  /** an action failed: show it as a toast */
  onError: (message: string) => void;
}

/**
 * The Boat account's sandboxes that are not machines on this computer. One made with the Boat
 * CLI or website can only be started and stopped. A t3boat machine that another computer
 * created can be added here. Hidden when there are none.
 */
export function OtherSandboxes({ onError }: Props) {
  const [list, setList] = useState<BoatSandbox[]>([]);
  const [busy, setBusy] = useState<Record<string, boolean>>({});

  const load = useCallback(() => {
    call("other_sandboxes", undefined).then(setList, () => {
      // keep what is shown: Boat being slow once is not worth a toast
    });
  }, []);

  useEffect(() => {
    load();
    const timer = window.setInterval(load, REFRESH_MS);
    return () => window.clearInterval(timer);
  }, [load]);

  const toggle = (s: BoatSandbox) => {
    setBusy((b) => ({ ...b, [s.id]: true }));
    call("set_sandbox_running", { id: s.id, running: !running(s) })
      .catch((e) => onError(errorText(e)))
      .finally(() => {
        setBusy((b) => ({ ...b, [s.id]: false }));
        load();
      });
  };

  const adopt = (s: BoatSandbox) => {
    setBusy((b) => ({ ...b, [s.id]: true }));
    call("adopt_machine", { id: s.id })
      .catch((e) => onError(errorText(e)))
      .finally(() => {
        setBusy((b) => ({ ...b, [s.id]: false }));
        load();
      });
  };

  if (list.length === 0) return null;

  return (
    <section className="others" aria-label="Other Boat sandboxes">
      <div className="list-head">
        <span className="caption">
          other boat sandboxes <span className="count">{list.length}</span>
        </span>
        <span className="caption others-note">not on this computer</span>
      </div>
      <ul className="list others-list">
        {list.map((s) => {
          const on = running(s);
          return (
            <li key={s.id} className={`row state-${iconState(s.state)}`}>
              <BoatIcon state={iconState(s.state)} seed={seedOf(s.id)} />
              <div className="row-text">
                <div className="row-title">
                  <span className="row-name">{s.name ?? s.id}</span>
                  <span className="chip">{s.state}</span>
                </div>
                <div className="row-detail">
                  {s.adoptable ? "t3boat machine from another computer" : s.id}
                  {sizeText(s) ? ` · ${sizeText(s)}` : ""}
                </div>
              </div>
              {s.adoptable ? (
                <button className="btn btn-sm" disabled={busy[s.id]} onClick={() => adopt(s)}>
                  {busy[s.id] ? <Dots /> : "Add here"}
                </button>
              ) : (
                <button className="btn btn-sm" disabled={busy[s.id]} onClick={() => toggle(s)}>
                  {busy[s.id] ? <Dots /> : on ? "Stop" : "Start"}
                </button>
              )}
            </li>
          );
        })}
      </ul>
    </section>
  );
}
