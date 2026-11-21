// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! One timer thread for any number of deadlines: `max_execution_time` and
//! `hard_timeout` of each VM, a server's keep-alive, idle, read and write
//! timeouts, `request_terminate_timeout`, `pm.process_idle_timeout`.
//!
//! A [`Timer`] owns an action and at most one pending deadline. Arming it
//! again replaces the deadline; the old heap entry stays behind and is
//! skipped when it comes up (its generation no longer matches), so a
//! keep-alive timer that is pushed back on every read costs one heap push,
//! and the heap is rebuilt from the live deadlines when stale entries
//! outnumber them. Dropping the timer cancels it.
//!
//! Actions run on the timer thread, one at a time, outside the lock: they
//! must be short and must not block (set a flag, wake a task, shut a socket
//! down). An action may arm or disarm timers; it re-arms its own through
//! the [`Expired`] it is given (`hard_timeout`, periodic work), so a timer
//! never has to own a handle to itself.
//!
//! php-src arms a POSIX timer per thread instead (`timer_create` with
//! `SIGEV_THREAD_ID`, `Zend/zend_max_execution_timer.c`), which needs a signal
//! per expiry; one thread waiting on the nearest deadline needs none.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// A timer's action.
type Action = Arc<dyn Fn(&Expired<'_>) + Send + Sync>;

/// A timer's slot: its action, its pending deadline and the generation of
/// the heap entry that stands for it.
struct Slot {
    action: Action,
    deadline: Option<Instant>,
    generation: u64,
}

/// The deadlines, nearest first: `(deadline, timer id, generation)`.
type Queue = BinaryHeap<Reverse<(Instant, u64, u64)>>;

#[derive(Default)]
struct State {
    queue: Queue,
    slots: HashMap<u64, Slot>,
    next_id: u64,
    /// Whether the timer thread runs (started by the first deadline).
    running: bool,
}

impl State {
    /// Drops queue entries whose timer is gone or was armed again since.
    fn drop_stale_head(&mut self) {
        while let Some(&Reverse((_, id, generation))) = self.queue.peek() {
            if self
                .slots
                .get(&id)
                .is_some_and(|s| s.generation == generation && s.deadline.is_some())
            {
                return;
            }
            self.queue.pop();
        }
    }

    /// Rebuilds the queue from the pending deadlines once stale entries
    /// make up most of it.
    fn compact(&mut self) {
        let live = self.slots.values().filter(|s| s.deadline.is_some()).count();
        if self.queue.len() <= 2 * live + 64 {
            return;
        }
        self.queue = self
            .slots
            .iter()
            .filter_map(|(&id, s)| s.deadline.map(|d| Reverse((d, id, s.generation))))
            .collect();
    }
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // An action never runs under the lock, so a poisoned lock only
        // means a panic in this module's own bookkeeping: keep going.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A set of timers served by one thread.
#[derive(Clone)]
pub struct Timers {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Timers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.shared.lock();
        f.debug_struct("Timers")
            .field("timers", &s.slots.len())
            .field("queued", &s.queue.len())
            .finish()
    }
}

impl Default for Timers {
    fn default() -> Self {
        Self::new()
    }
}

impl Timers {
    /// A new set; its thread starts with the first deadline.
    #[must_use]
    pub fn new() -> Self {
        Timers {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                wake: Condvar::new(),
            }),
        }
    }

    /// The process's set.
    pub fn global() -> &'static Timers {
        static GLOBAL: LazyLock<Timers> = LazyLock::new(Timers::new);
        &GLOBAL
    }

    /// A timer that runs `action` when a deadline it was armed with
    /// passes; not armed yet.
    pub fn timer(&self, action: impl Fn(&Expired<'_>) + Send + Sync + 'static) -> Timer {
        let mut s = self.shared.lock();
        let id = s.next_id;
        s.next_id += 1;
        s.slots.insert(
            id,
            Slot {
                action: Arc::new(action),
                deadline: None,
                generation: 0,
            },
        );
        Timer {
            shared: Arc::clone(&self.shared),
            id,
        }
    }

    /// The timers that exist (armed or not), for tests and status pages.
    #[must_use]
    pub fn len(&self) -> usize {
        self.shared.lock().slots.len()
    }

    /// Whether no timer exists.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A timer of a [`Timers`] set (see the module docs).
pub struct Timer {
    shared: Arc<Shared>,
    id: u64,
}

impl std::fmt::Debug for Timer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Timer")
            .field("id", &self.id)
            .field("deadline", &self.deadline())
            .finish_non_exhaustive()
    }
}

impl Timer {
    /// Runs the action `after` from now (replacing a pending deadline).
    /// A deadline too far ahead to represent never passes.
    pub fn arm(&self, after: Duration) {
        match Instant::now().checked_add(after) {
            Some(at) => self.arm_at(at),
            None => self.disarm(),
        }
    }

    /// Runs the action at `deadline` (replacing a pending deadline); a past
    /// deadline runs it as soon as the timer thread gets to it.
    pub fn arm_at(&self, deadline: Instant) {
        arm_at(&self.shared, self.id, deadline);
    }

    /// Cancels the pending deadline, if any.
    pub fn disarm(&self) {
        let mut s = self.shared.lock();
        if let Some(slot) = s.slots.get_mut(&self.id) {
            slot.deadline = None;
            slot.generation += 1;
        }
    }

    /// The pending deadline.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.shared
            .lock()
            .slots
            .get(&self.id)
            .and_then(|s| s.deadline)
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        self.shared.lock().slots.remove(&self.id);
    }
}

/// The timer whose action runs, as that action sees it: it can be armed
/// again (a timer dropped meanwhile stays gone).
#[derive(Debug)]
pub struct Expired<'a> {
    shared: &'a Arc<Shared>,
    id: u64,
}

impl Expired<'_> {
    /// Runs the action again `after` from now.
    pub fn arm(&self, after: Duration) {
        if let Some(at) = Instant::now().checked_add(after) {
            arm_at(self.shared, self.id, at);
        }
    }

    /// Runs the action again at `deadline`.
    pub fn arm_at(&self, deadline: Instant) {
        arm_at(self.shared, self.id, deadline);
    }
}

/// Arms timer `id` of `shared` for `deadline`.
fn arm_at(shared: &Arc<Shared>, id: u64, deadline: Instant) {
    let mut s = shared.lock();
    let Some(slot) = s.slots.get_mut(&id) else {
        return;
    };
    slot.generation += 1;
    slot.deadline = Some(deadline);
    let generation = slot.generation;
    let earliest = s
        .queue
        .peek()
        .is_none_or(|&Reverse((d, _, _))| deadline < d);
    s.queue.push(Reverse((deadline, id, generation)));
    s.compact();
    if !s.running {
        s.running = true;
        drop(s);
        spawn(shared);
    } else if earliest {
        drop(s);
        shared.wake.notify_one();
    }
}

/// Starts the thread that serves `shared`'s deadlines. It ends with the
/// last [`Timers`] handle and timer, once nothing is pending.
fn spawn(shared: &Arc<Shared>) {
    let started = std::thread::Builder::new().name("php-timer".into()).spawn({
        let shared = Arc::clone(shared);
        move || serve(&shared)
    });
    if started.is_err() {
        // No thread: deadlines cannot pass. Let the next arm try again.
        shared.lock().running = false;
    }
}

fn serve(shared: &Arc<Shared>) {
    let mut s = shared.lock();
    loop {
        s.drop_stale_head();
        let Some(&Reverse((deadline, id, _))) = s.queue.peek() else {
            if Arc::strong_count(shared) == 1 {
                s.running = false;
                return;
            }
            // Nothing pending: wait for an arm (bounded, to notice the
            // last handle going away).
            s = shared
                .wake
                .wait_timeout(s, Duration::from_secs(1))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            continue;
        };
        let now = Instant::now();
        if deadline > now {
            s = shared
                .wake
                .wait_timeout(s, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            continue;
        }
        s.queue.pop();
        let Some(slot) = s.slots.get_mut(&id) else {
            continue;
        };
        slot.deadline = None;
        let action = Arc::clone(&slot.action);
        drop(s);
        action(&Expired { shared, id });
        s = shared.lock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    fn counting(timers: &Timers) -> (Timer, Arc<AtomicUsize>) {
        let n = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&n);
        let t = timers.timer(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        (t, n)
    }

    fn settle() {
        std::thread::sleep(Duration::from_millis(150));
    }

    #[test]
    fn deadlines_pass_in_order() {
        let timers = Timers::new();
        let (tx, rx) = mpsc::channel();
        let mut held = Vec::new();
        for (i, ms) in [(0, 60u64), (1, 20), (2, 40)] {
            let tx = tx.clone();
            let t = timers.timer(move |_| tx.send(i).unwrap_or(()));
            t.arm(Duration::from_millis(ms));
            held.push(t);
        }
        let got: Vec<i32> = (0..3)
            .map(|_| rx.recv_timeout(Duration::from_secs(5)).expect("expiry"))
            .collect();
        assert_eq!(got, [1, 2, 0]);
        assert!(held.iter().all(|t| t.deadline().is_none()));
    }

    #[test]
    fn rearming_replaces_and_disarming_cancels() {
        let timers = Timers::new();
        let (t, n) = counting(&timers);
        t.arm(Duration::from_millis(30));
        t.arm(Duration::from_millis(60));
        settle();
        assert_eq!(n.load(Ordering::SeqCst), 1, "one expiry for two arms");
        t.arm(Duration::from_millis(30));
        t.disarm();
        settle();
        assert_eq!(n.load(Ordering::SeqCst), 1, "disarmed");
        assert_eq!(t.deadline(), None);
    }

    #[test]
    fn a_keep_alive_pushed_back_often_fires_once() {
        let timers = Timers::new();
        let (t, n) = counting(&timers);
        for _ in 0..10_000 {
            t.arm(Duration::from_millis(40));
        }
        assert!(
            timers.shared.lock().queue.len() < 200,
            "stale entries are compacted"
        );
        settle();
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn dropping_a_timer_cancels_it() {
        let timers = Timers::new();
        let (t, n) = counting(&timers);
        t.arm(Duration::from_millis(30));
        drop(t);
        settle();
        assert_eq!(n.load(Ordering::SeqCst), 0);
        assert!(timers.is_empty());
    }

    #[test]
    fn an_earlier_deadline_wakes_the_thread() {
        let timers = Timers::new();
        let (late, _) = counting(&timers);
        late.arm(Duration::from_secs(60));
        let (soon, n) = counting(&timers);
        let start = Instant::now();
        soon.arm(Duration::from_millis(20));
        while n.load(Ordering::SeqCst) == 0 {
            assert!(start.elapsed() < Duration::from_secs(5), "never woke");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn an_action_may_rearm_its_timer() {
        // `hard_timeout`: each expiry arms the next.
        let timers = Timers::new();
        let n = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&n);
        let t = timers.timer(move |me| {
            if c.fetch_add(1, Ordering::SeqCst) < 2 {
                me.arm(Duration::from_millis(10));
            }
        });
        t.arm(Duration::from_millis(10));
        settle();
        assert_eq!(n.load(Ordering::SeqCst), 3);
        assert_eq!(t.deadline(), None);
    }

    #[test]
    fn many_threads_arm_their_own_timers() {
        let timers = Timers::new();
        let fired = Arc::new(AtomicUsize::new(0));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let timers = timers.clone();
                let fired = Arc::clone(&fired);
                std::thread::spawn(move || {
                    let f = Arc::clone(&fired);
                    let t = timers.timer(move |_| {
                        f.fetch_add(1, Ordering::SeqCst);
                    });
                    for _ in 0..1000 {
                        t.arm(Duration::from_millis(30));
                    }
                    std::thread::sleep(Duration::from_millis(150));
                    t
                })
            })
            .collect();
        let held: Vec<Timer> = workers
            .into_iter()
            .map(|w| w.join().expect("worker"))
            .collect();
        assert_eq!(fired.load(Ordering::SeqCst), 8);
        drop(held);
    }
}
