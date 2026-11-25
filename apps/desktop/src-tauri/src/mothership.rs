//! The "mothership": a Boat sandbox that runs `t3boatd` for the user's other machines. It puts
//! itself to sleep once no machine works, no scheduled work is near and no client has used it for
//! a while; a t3boat on another computer wakes it again.

/// Scheduled work this close keeps the mothership up: it wakes a machine 2 minutes before its
/// work is due, and that machine then idles 5 minutes before it sleeps.
pub const DUE_GUARD_SECS: u64 = 7 * 60;

/// What the app does right now, for deciding whether the mothership may sleep.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActivitySnapshot {
    /// A machine runs, wakes or goes to sleep, or one is being created.
    pub any_worker_active: bool,
    /// Seconds until the soonest scheduled work on any machine; none if nothing is due ahead.
    pub next_due_in_secs: Option<i64>,
    /// Seconds since a client last used the app, or since it started.
    pub idle_for_secs: u64,
}

/// Whether the mothership may sleep: nothing works, no scheduled work is due within
/// `due_guard_secs`, and no client used it for `idle_secs`.
pub fn should_sleep(snap: &ActivitySnapshot, idle_secs: u64, due_guard_secs: u64) -> bool {
    if snap.any_worker_active {
        return false;
    }
    if snap.next_due_in_secs.is_some_and(|due| due <= due_guard_secs as i64) {
        return false;
    }
    snap.idle_for_secs >= idle_secs
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDLE: u64 = 300;

    fn idle() -> ActivitySnapshot {
        ActivitySnapshot { any_worker_active: false, next_due_in_secs: None, idle_for_secs: IDLE }
    }

    fn sleeps(snap: ActivitySnapshot) -> bool {
        should_sleep(&snap, IDLE, DUE_GUARD_SECS)
    }

    #[test]
    fn sleeps_when_all_is_idle() {
        assert!(sleeps(idle()));
        assert!(sleeps(ActivitySnapshot { idle_for_secs: 10_000, ..idle() }));
    }

    #[test]
    fn stays_up_while_a_machine_is_active_or_being_created() {
        // `any_worker_active` covers both: a machine running or waking, and a create.
        assert!(!sleeps(ActivitySnapshot { any_worker_active: true, ..idle() }));
        assert!(!sleeps(ActivitySnapshot { any_worker_active: true, idle_for_secs: 10_000, ..idle() }));
    }

    #[test]
    fn stays_up_for_scheduled_work_soon() {
        assert!(!sleeps(ActivitySnapshot { next_due_in_secs: Some(60), ..idle() }));
        assert!(!sleeps(ActivitySnapshot { next_due_in_secs: Some(DUE_GUARD_SECS as i64), ..idle() }));
    }

    #[test]
    fn sleeps_with_scheduled_work_far_off() {
        assert!(sleeps(ActivitySnapshot { next_due_in_secs: Some(DUE_GUARD_SECS as i64 + 1), ..idle() }));
        assert!(sleeps(ActivitySnapshot { next_due_in_secs: Some(24 * 3600), ..idle() }));
    }

    #[test]
    fn stays_up_after_recent_client_activity() {
        assert!(!sleeps(ActivitySnapshot { idle_for_secs: 5, ..idle() }));
        assert!(!sleeps(ActivitySnapshot { idle_for_secs: IDLE - 1, ..idle() }));
    }

    #[test]
    fn stays_up_during_the_startup_grace() {
        // Right after a start no client has acted yet: the idle time counts from the start.
        assert!(!sleeps(ActivitySnapshot { idle_for_secs: 0, ..idle() }));
    }
}
