//! The only thread allowed to touch the probe (DESIGN §5.2.3).
//!
//! Each loop drains P0 completely, then gives P1..P4 a time slice in priority
//! order, returning to P0 as soon as new P0 work shows up.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use probe_api::{Histogram, HistogramSummary, Probe, ProbeError, Result};
use tokio::sync::oneshot;

/// Queue classes from the design. Lower runs first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    /// Run control, breakpoints, stepping.
    P0 = 0,
    /// HSS reads, halt detection.
    P1 = 1,
    /// RTT reads.
    P2 = 2,
    /// Live Watch and polled sampling.
    P3 = 3,
    /// Background work: SVD refresh, task lists, stack scans.
    P4 = 4,
}

const QUEUES: usize = 5;
/// How long P1..P4 may run before the loop looks at P0 and the channel again.
const TIME_SLICE: Duration = Duration::from_millis(2);
/// Longest idle wait, so a stopped sender is noticed promptly.
const MAX_IDLE: Duration = Duration::from_millis(50);

type Job = Box<dyn FnOnce(&mut dyn Probe) + Send>;
type Task = Box<dyn FnMut(&mut dyn Probe) + Send>;

enum Message {
    Job(Priority, Job),
    AddPeriodic {
        id: u64,
        priority: Priority,
        period: Duration,
        task: Task,
    },
    RemovePeriodic(u64),
    /// Re-evaluate the suspension state.
    Wake,
    Shutdown,
}

struct Periodic {
    id: u64,
    priority: Priority,
    period: Duration,
    next_due: Instant,
    task: Task,
}

#[derive(Clone, Debug, Default)]
pub struct QueueStats {
    /// Jobs and periodic runs executed.
    pub executed: u64,
    /// Periodic runs that started more than one period late.
    pub overruns: u64,
    pub max_depth: usize,
    pub run_time: HistogramSummary,
}

#[derive(Default)]
struct Shared {
    suspend_count: AtomicUsize,
    queues: Mutex<[QueueCounters; QUEUES]>,
}

#[derive(Default)]
struct QueueCounters {
    executed: u64,
    overruns: u64,
    max_depth: usize,
    run_time: Histogram,
}

#[derive(Clone)]
pub struct ProbeHandle {
    sender: Sender<Message>,
    shared: Arc<Shared>,
    next_id: Arc<AtomicU64>,
}

/// Owns the worker thread; dropping it stops the worker.
pub struct ProbeActor {
    handle: ProbeHandle,
    thread: Option<JoinHandle<Box<dyn Probe>>>,
}

impl ProbeActor {
    pub fn spawn(probe: Box<dyn Probe>) -> Self {
        let (sender, receiver) = mpsc::channel();
        let shared = Arc::new(Shared::default());
        let worker = Worker {
            probe,
            receiver,
            shared: shared.clone(),
            queues: Default::default(),
            periodic: Vec::new(),
            was_suspended: false,
        };
        let thread = thread::Builder::new()
            .name("probe-actor".to_owned())
            .spawn(move || worker.run())
            .expect("spawning the probe thread");
        Self {
            handle: ProbeHandle {
                sender,
                shared,
                next_id: Arc::new(AtomicU64::new(1)),
            },
            thread: Some(thread),
        }
    }

    pub fn handle(&self) -> ProbeHandle {
        self.handle.clone()
    }

    /// Stop the worker and take the probe back.
    pub fn shutdown(mut self) -> Result<Box<dyn Probe>> {
        let _ = self.handle.sender.send(Message::Shutdown);
        self.thread
            .take()
            .expect("the thread is only taken here or in drop")
            .join()
            .map_err(|_| ProbeError::ActorStopped)
    }
}

impl Drop for ProbeActor {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.handle.sender.send(Message::Shutdown);
            let _ = thread.join();
        }
    }
}

/// Keeps P2..P4 from running while it is alive; P0 and P1 are unaffected.
pub struct SuspendGuard {
    handle: ProbeHandle,
}

impl Drop for SuspendGuard {
    fn drop(&mut self) {
        self.handle
            .shared
            .suspend_count
            .fetch_sub(1, Ordering::SeqCst);
        let _ = self.handle.sender.send(Message::Wake);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PeriodicId(u64);

impl ProbeHandle {
    /// Queue a call; the result arrives on the returned channel.
    pub fn call<R, F>(&self, priority: Priority, f: F) -> Result<oneshot::Receiver<R>>
    where
        R: Send + 'static,
        F: FnOnce(&mut dyn Probe) -> R + Send + 'static,
    {
        let (reply, receiver) = oneshot::channel();
        let job: Job = Box::new(move |probe| {
            let _ = reply.send(f(probe));
        });
        self.sender
            .send(Message::Job(priority, job))
            .map_err(|_| ProbeError::ActorStopped)?;
        Ok(receiver)
    }

    /// Queue a call and wait for it. Must not be used from async code.
    pub fn call_blocking<R, F>(&self, priority: Priority, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut dyn Probe) -> R + Send + 'static,
    {
        self.call(priority, f)?
            .blocking_recv()
            .map_err(|_| ProbeError::ActorStopped)
    }

    pub fn periodic<F>(&self, priority: Priority, period: Duration, task: F) -> Result<PeriodicId>
    where
        F: FnMut(&mut dyn Probe) + Send + 'static,
    {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.sender
            .send(Message::AddPeriodic {
                id,
                priority,
                period,
                task: Box::new(task),
            })
            .map_err(|_| ProbeError::ActorStopped)?;
        Ok(PeriodicId(id))
    }

    pub fn cancel(&self, id: PeriodicId) -> Result<()> {
        self.sender
            .send(Message::RemovePeriodic(id.0))
            .map_err(|_| ProbeError::ActorStopped)
    }

    /// Hold P2..P4 back while stepping or halting so USB stays free.
    pub fn suspend_background(&self) -> SuspendGuard {
        self.shared.suspend_count.fetch_add(1, Ordering::SeqCst);
        SuspendGuard {
            handle: self.clone(),
        }
    }

    /// Per-queue statistics, indexed by [`Priority`].
    pub fn stats(&self) -> Vec<QueueStats> {
        let queues = self
            .shared
            .queues
            .lock()
            .expect("counters are never left inconsistent");
        queues
            .iter()
            .map(|q| QueueStats {
                executed: q.executed,
                overruns: q.overruns,
                max_depth: q.max_depth,
                run_time: q.run_time.summary(),
            })
            .collect()
    }
}

struct Worker {
    probe: Box<dyn Probe>,
    receiver: Receiver<Message>,
    shared: Arc<Shared>,
    queues: [VecDeque<Job>; QUEUES],
    periodic: Vec<Periodic>,
    was_suspended: bool,
}

/// Whether the loop should keep running.
#[derive(PartialEq)]
enum Flow {
    Continue,
    Stop,
}

impl Worker {
    fn run(mut self) -> Box<dyn Probe> {
        loop {
            if self.drain_channel() == Flow::Stop {
                break;
            }
            self.update_suspension();

            if let Some(job) = self.queues[Priority::P0 as usize].pop_front() {
                self.execute_job(Priority::P0, job);
                continue;
            }

            let (ran, flow) = self.run_background_slice();
            if flow == Flow::Stop {
                break;
            }
            if !ran && self.queues[Priority::P0 as usize].is_empty() {
                let timeout = self.idle_timeout();
                match self.receiver.recv_timeout(timeout) {
                    Ok(message) => {
                        if self.handle(message) == Flow::Stop {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        }
        self.probe
    }

    fn is_suspended(&self) -> bool {
        self.shared.suspend_count.load(Ordering::SeqCst) > 0
    }

    /// When suspension ends, restart background periods instead of treating
    /// the suspended time as missed deadlines.
    fn update_suspension(&mut self) {
        let suspended = self.is_suspended();
        if self.was_suspended && !suspended {
            let now = Instant::now();
            for task in &mut self.periodic {
                if task.priority >= Priority::P2 {
                    task.next_due = now;
                }
            }
        }
        self.was_suspended = suspended;
    }

    fn drain_channel(&mut self) -> Flow {
        loop {
            match self.receiver.try_recv() {
                Ok(message) => {
                    if self.handle(message) == Flow::Stop {
                        return Flow::Stop;
                    }
                }
                Err(TryRecvError::Empty) => return Flow::Continue,
                Err(TryRecvError::Disconnected) => return Flow::Stop,
            }
        }
    }

    fn handle(&mut self, message: Message) -> Flow {
        match message {
            Message::Job(priority, job) => {
                let queue = &mut self.queues[priority as usize];
                queue.push_back(job);
                let depth = queue.len();
                let mut counters = self.shared.queues.lock().expect("counters lock");
                let slot = &mut counters[priority as usize];
                slot.max_depth = slot.max_depth.max(depth);
            }
            Message::AddPeriodic {
                id,
                priority,
                period,
                task,
            } => self.periodic.push(Periodic {
                id,
                priority,
                period,
                next_due: Instant::now(),
                task,
            }),
            Message::RemovePeriodic(id) => self.periodic.retain(|task| task.id != id),
            Message::Wake => {}
            Message::Shutdown => return Flow::Stop,
        }
        Flow::Continue
    }

    /// Run P1..P4 until the slice is used up, nothing is due, or P0 has work.
    /// Returns whether anything ran.
    fn run_background_slice(&mut self) -> (bool, Flow) {
        let slice_end = Instant::now() + TIME_SLICE;
        let mut ran = false;
        loop {
            if !self.run_one_background() {
                return (ran, Flow::Continue);
            }
            ran = true;
            if self.drain_channel() == Flow::Stop {
                return (ran, Flow::Stop);
            }
            let p0_waiting = !self.queues[Priority::P0 as usize].is_empty();
            if p0_waiting || Instant::now() >= slice_end {
                return (ran, Flow::Continue);
            }
        }
    }

    /// Execute the most urgent background item, if any is ready.
    fn run_one_background(&mut self) -> bool {
        const BACKGROUND: [Priority; 4] = [Priority::P1, Priority::P2, Priority::P3, Priority::P4];
        let suspended = self.is_suspended();
        let now = Instant::now();
        for priority in BACKGROUND {
            if suspended && priority >= Priority::P2 {
                break;
            }
            let due = self
                .periodic
                .iter()
                .enumerate()
                .filter(|(_, task)| task.priority == priority && task.next_due <= now)
                .min_by_key(|(_, task)| task.next_due)
                .map(|(index, _)| index);
            if let Some(index) = due {
                self.execute_periodic(index, now);
                return true;
            }
            if let Some(job) = self.queues[priority as usize].pop_front() {
                self.execute_job(priority, job);
                return true;
            }
        }
        false
    }

    fn execute_job(&mut self, priority: Priority, job: Job) {
        let started = Instant::now();
        job(self.probe.as_mut());
        self.account(priority, started.elapsed(), false);
    }

    fn execute_periodic(&mut self, index: usize, now: Instant) {
        let (priority, period, lateness) = {
            let task = &self.periodic[index];
            (task.priority, task.period, now - task.next_due)
        };
        let started = Instant::now();
        (self.periodic[index].task)(self.probe.as_mut());
        let elapsed = started.elapsed();
        // Keep a fixed cadence, but never try to catch up on missed periods.
        let task = &mut self.periodic[index];
        task.next_due += period;
        if task.next_due < Instant::now() {
            task.next_due = Instant::now() + period;
        }
        self.account(priority, elapsed, lateness > period);
    }

    fn account(&self, priority: Priority, elapsed: Duration, overrun: bool) {
        let mut counters = self.shared.queues.lock().expect("counters lock");
        let slot = &mut counters[priority as usize];
        slot.executed += 1;
        slot.overruns += u64::from(overrun);
        slot.run_time.record(elapsed);
    }

    fn idle_timeout(&self) -> Duration {
        let suspended = self.is_suspended();
        let next_due = self
            .periodic
            .iter()
            .filter(|task| !suspended || task.priority < Priority::P2)
            .map(|task| task.next_due)
            .min();
        match next_due {
            Some(due) => due.saturating_duration_since(Instant::now()).min(MAX_IDLE),
            None => MAX_IDLE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use probe_api::ProbeExt;
    use probe_mock::MockProbe;
    use std::sync::atomic::AtomicBool;

    fn actor() -> ProbeActor {
        let mut mock = MockProbe::new();
        mock.add_region(0x2000_0000, vec![0; 64]);
        ProbeActor::spawn(Box::new(mock))
    }

    #[test]
    fn call_returns_the_result() {
        let actor = actor();
        let handle = actor.handle();
        handle
            .call_blocking(Priority::P3, |p| p.write_u32(0x2000_0000, 7))
            .unwrap()
            .unwrap();
        let value = handle
            .call_blocking(Priority::P0, |p| p.read_u32(0x2000_0000))
            .unwrap()
            .unwrap();
        assert_eq!(value, 7);
    }

    #[test]
    fn p0_runs_before_queued_background_work() {
        let actor = actor();
        let handle = actor.handle();
        let order = Arc::new(Mutex::new(Vec::new()));

        // Hold the worker inside a P0 job while the rest is queued, so that
        // both priorities are pending when it comes back.
        let (release, wait) = mpsc::channel::<()>();
        handle
            .call(Priority::P0, move |_| {
                wait.recv().unwrap();
            })
            .unwrap();
        for (priority, tag) in [
            (Priority::P4, "p4"),
            (Priority::P1, "p1"),
            (Priority::P0, "p0"),
        ] {
            let order = order.clone();
            handle
                .call(priority, move |_| order.lock().unwrap().push(tag))
                .unwrap();
        }
        release.send(()).unwrap();
        // A P4 job at the end of the queue runs once everything before is done.
        handle.call_blocking(Priority::P4, |_| ()).unwrap();
        assert_eq!(*order.lock().unwrap(), ["p0", "p1", "p4"]);
    }

    #[test]
    fn suspend_holds_back_p2_and_below_only() {
        let actor = actor();
        let handle = actor.handle();
        let guard = handle.suspend_background();
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let pending = handle
            .call(Priority::P3, move |_| flag.store(true, Ordering::SeqCst))
            .unwrap();

        // P0 and P1 keep flowing through while P3 stays parked.
        handle.call_blocking(Priority::P0, |_| ()).unwrap();
        handle.call_blocking(Priority::P1, |_| ()).unwrap();
        thread::sleep(Duration::from_millis(20));
        assert!(!done.load(Ordering::SeqCst));

        drop(guard);
        pending.blocking_recv().unwrap();
        assert!(done.load(Ordering::SeqCst));
    }

    #[test]
    fn periodic_tasks_run_until_cancelled() {
        let actor = actor();
        let handle = actor.handle();
        let count = Arc::new(AtomicU64::new(0));
        let counter = count.clone();
        let id = handle
            .periodic(Priority::P2, Duration::from_millis(1), move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
        while count.load(Ordering::SeqCst) < 5 {
            thread::sleep(Duration::from_millis(1));
        }
        handle.cancel(id).unwrap();
        // The cancel is processed once this round trip completes.
        handle.call_blocking(Priority::P0, |_| ()).unwrap();
        let settled = count.load(Ordering::SeqCst);
        thread::sleep(Duration::from_millis(10));
        assert_eq!(count.load(Ordering::SeqCst), settled);
    }

    #[test]
    fn suspended_time_is_not_counted_as_overruns() {
        let actor = actor();
        let handle = actor.handle();
        let count = Arc::new(AtomicU64::new(0));
        let counter = count.clone();
        handle
            .periodic(Priority::P3, Duration::from_millis(2), move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
        while count.load(Ordering::SeqCst) == 0 {
            thread::sleep(Duration::from_millis(1));
        }
        let guard = handle.suspend_background();
        handle.call_blocking(Priority::P0, |_| ()).unwrap();
        thread::sleep(Duration::from_millis(30));
        let before = count.load(Ordering::SeqCst);
        drop(guard);
        while count.load(Ordering::SeqCst) <= before {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(handle.stats()[Priority::P3 as usize].overruns, 0);
    }

    #[test]
    fn stats_count_executed_jobs() {
        let actor = actor();
        let handle = actor.handle();
        for _ in 0..3 {
            handle.call_blocking(Priority::P1, |_| ()).unwrap();
        }
        assert_eq!(handle.stats()[Priority::P1 as usize].executed, 3);
    }

    #[test]
    fn shutdown_returns_the_probe() {
        let actor = actor();
        actor
            .handle()
            .call_blocking(Priority::P0, |p| p.write_u32(0x2000_0004, 9))
            .unwrap()
            .unwrap();
        let mut probe = actor.shutdown().unwrap();
        assert_eq!(probe.read_u32(0x2000_0004).unwrap(), 9);
    }
}
