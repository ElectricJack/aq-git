use super::*;
pub const MAX_CHILD_WAITERS: usize = 64;
#[derive(Default)]
pub(super) struct Admission {
    next_ticket: u64,
    queue: std::collections::VecDeque<u64>,
}
struct WaitTicket {
    number: u64,
    queued_at: Instant,
    deadline: Instant,
}
impl Counters {
    pub(super) fn acquire_child(&self, limit: usize, deadline: Instant) -> Result<()> {
        if limit == 0 {
            return Err(Error::new(
                "invalid_configuration",
                "Git child limit must be positive",
            ));
        }
        let mut admission = lock_with_deadline(&self.admission)?;
        if Instant::now() >= deadline {
            return Err(Error::new(
                "deadline_exceeded",
                "Git child admission deadline expired",
            ));
        }
        if admission.queue.is_empty() && self.try_acquire_child(limit) {
            return Ok(());
        }
        if admission.queue.len() >= MAX_CHILD_WAITERS {
            return Err(Error::new("overloaded", "Git child wait queue is full"));
        }
        let ticket = admission.enqueue();
        self.queued.fetch_add(1, Ordering::Relaxed);
        let waiting = self.waiting.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak_waiters.fetch_max(waiting, Ordering::Relaxed);
        let queued_at = Instant::now();
        self.wait_for_ticket(
            admission,
            WaitTicket {
                number: ticket,
                queued_at,
                deadline,
            },
            limit,
        )
    }
    fn wait_for_ticket(
        &self,
        mut admission: std::sync::MutexGuard<'_, Admission>,
        WaitTicket {
            number: ticket,
            queued_at,
            deadline,
        }: WaitTicket,
        limit: usize,
    ) -> Result<()> {
        loop {
            if Instant::now() >= deadline {
                admission.queue.retain(|queued| *queued != ticket);
                self.waiting.fetch_sub(1, Ordering::Relaxed);
                self.queue_wait_micros.fetch_add(
                    queued_at.elapsed().as_micros().min(u64::MAX as u128) as u64,
                    Ordering::Relaxed,
                );
                self.queue_timeouts.fetch_add(1, Ordering::Relaxed);
                self.timeouts.fetch_add(1, Ordering::Relaxed);
                self.available.notify_all();
                return Err(Error::new(
                    "deadline_exceeded",
                    "Git child admission deadline expired",
                ));
            }
            if admission.queue.front() == Some(&ticket) && self.try_acquire_child(limit) {
                admission.queue.pop_front();
                self.waiting.fetch_sub(1, Ordering::Relaxed);
                self.queue_wait_micros.fetch_add(
                    queued_at.elapsed().as_micros().min(u64::MAX as u128) as u64,
                    Ordering::Relaxed,
                );
                self.available.notify_all();
                return Ok(());
            }
            let duration = deadline.saturating_duration_since(Instant::now());
            admission = match self.available.wait_timeout(admission, duration) {
                Ok((guard, _)) => guard,
                Err(poison) => {
                    let (mut guard, _) = poison.into_inner();
                    guard.queue.retain(|queued| *queued != ticket);
                    self.waiting.fetch_sub(1, Ordering::Relaxed);
                    self.available.notify_all();
                    return Err(Error::new(
                        "recovery_blocked",
                        "Git child admission lock is poisoned",
                    ));
                }
            };
        }
    }
    pub(crate) fn try_acquire_child(&self, limit: usize) -> bool {
        let mut active = self.active.load(Ordering::Relaxed);
        loop {
            if active >= limit {
                return false;
            }
            match self.active.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(value) => active = value,
            }
        }
    }
    pub(crate) fn release_child(&self) {
        let _admission = self
            .admission
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        self.active.fetch_sub(1, Ordering::Release);
        self.available.notify_all();
    }
}
impl Admission {
    fn enqueue(&mut self) -> u64 {
        let mut ticket = self.next_ticket;
        while self.queue.contains(&ticket) {
            ticket = ticket.wrapping_add(1);
        }
        self.next_ticket = ticket.wrapping_add(1);
        self.queue.push_back(ticket);
        ticket
    }
}
