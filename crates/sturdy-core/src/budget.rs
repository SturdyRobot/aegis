//! Hard execution budgets.
//!
//! Every dimension (tokens, steps, wall-clock) is a *ceiling*, not a target.
//! The tracker is `Clone` + `Send` + `Sync` (atomics behind an `Arc`) so it can
//! be shared across Tokio tasks and charged concurrently without a mutex.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::{BudgetKind, HarnessError, Result};

/// Static budget configuration for a run.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub max_tokens: u64,
    pub max_steps: u64,
    pub wall_clock: Duration,
}

impl Budget {
    /// A sane interactive default: 100k tokens, 30 steps, 5 minutes.
    pub fn standard() -> Self {
        Budget {
            max_tokens: 100_000,
            max_steps: 30,
            wall_clock: Duration::from_secs(300),
        }
    }

    /// Turn this configuration into a live, shareable tracker.
    pub fn tracker(self) -> BudgetTracker {
        BudgetTracker::new(self)
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::standard()
    }
}

#[derive(Debug)]
struct Inner {
    budget: Budget,
    tokens: AtomicU64,
    steps: AtomicU64,
    deadline: Instant,
}

/// Live, thread-safe budget accounting for a single run.
#[derive(Debug, Clone)]
pub struct BudgetTracker {
    inner: Arc<Inner>,
}

impl BudgetTracker {
    pub fn new(budget: Budget) -> Self {
        BudgetTracker {
            inner: Arc::new(Inner {
                budget,
                tokens: AtomicU64::new(0),
                steps: AtomicU64::new(0),
                deadline: Instant::now() + budget.wall_clock,
            }),
        }
    }

    /// Charge `n` tokens. Returns an error the moment the ceiling is crossed,
    /// so a single over-large charge is still caught.
    pub fn charge_tokens(&self, n: u64) -> Result<()> {
        let used = self.inner.tokens.fetch_add(n, Ordering::SeqCst) + n;
        if used > self.inner.budget.max_tokens {
            return Err(HarnessError::BudgetExceeded {
                kind: BudgetKind::Tokens,
                used,
                limit: self.inner.budget.max_tokens,
            });
        }
        Ok(())
    }

    /// Charge one ReAct step.
    pub fn charge_step(&self) -> Result<()> {
        let used = self.inner.steps.fetch_add(1, Ordering::SeqCst) + 1;
        if used > self.inner.budget.max_steps {
            return Err(HarnessError::BudgetExceeded {
                kind: BudgetKind::Steps,
                used,
                limit: self.inner.budget.max_steps,
            });
        }
        Ok(())
    }

    /// Fail if the wall-clock deadline has passed.
    pub fn check_deadline(&self) -> Result<()> {
        if Instant::now() >= self.inner.deadline {
            return Err(HarnessError::BudgetExceeded {
                kind: BudgetKind::WallClock,
                used: self.inner.budget.wall_clock.as_millis() as u64,
                limit: self.inner.budget.wall_clock.as_millis() as u64,
            });
        }
        Ok(())
    }

    /// Time left before the deadline (zero once past it).
    pub fn time_remaining(&self) -> Duration {
        self.inner
            .deadline
            .saturating_duration_since(Instant::now())
    }

    /// Run `fut` but abort it if it would outlast the remaining wall-clock.
    ///
    /// This is how the engine bounds a single potentially-slow reasoner or tool
    /// call: the whole run can never exceed its deadline, even mid-await.
    pub async fn run_within<T>(
        &self,
        fut: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        let remaining = self.time_remaining();
        if remaining.is_zero() {
            // Deadline already passed — don't even start the future.
            return Err(HarnessError::BudgetExceeded {
                kind: BudgetKind::WallClock,
                used: self.inner.budget.wall_clock.as_millis() as u64,
                limit: self.inner.budget.wall_clock.as_millis() as u64,
            });
        }
        match tokio::time::timeout(remaining, fut).await {
            Ok(inner) => inner,
            Err(_) => Err(HarnessError::BudgetExceeded {
                kind: BudgetKind::WallClock,
                used: self.inner.budget.wall_clock.as_millis() as u64,
                limit: self.inner.budget.wall_clock.as_millis() as u64,
            }),
        }
    }

    pub fn tokens_used(&self) -> u64 {
        self.inner.tokens.load(Ordering::SeqCst)
    }

    pub fn steps_used(&self) -> u64 {
        self.inner.steps.load(Ordering::SeqCst)
    }

    pub fn budget(&self) -> Budget {
        self.inner.budget
    }
}
