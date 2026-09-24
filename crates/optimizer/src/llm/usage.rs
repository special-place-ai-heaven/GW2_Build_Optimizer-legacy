//! Per-run LLM accounting: tokens, requests, time spent waiting on the model,
//! and a live event hook so the overlay can show each request as it happens.
//!
//! Thread-local, the same shape as [`super::cancel::CancelScope`] and
//! [`super::live::LiveScope`]: `LlmClient` returns `String` and a tool loop
//! makes many requests per call, so the transports record each response here
//! instead of threading usage back through every signature. A run installs a
//! [`UsageScope`] (and optionally an [`ObserverScope`]) on its worker thread
//! and reads the total when it ends. With neither installed every function
//! below is a no-op.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use gw2_core::generations::TokenUsage;

/// Everything one run spent on the model.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RunUsage {
    pub tokens: TokenUsage,
    /// Wall time inside chat requests: pacing sleeps, retries and the stream.
    pub wait: Duration,
    /// Sum of the cost the provider itself reported (OpenRouter `usage.cost`),
    /// `None` when no response carried one.
    pub reported_cost_usd: Option<f64>,
    /// Tokens and requests of the responses that carried no `usage.cost`.
    /// When some did, this is the part the reported sum leaves out.
    pub uncosted: TokenUsage,
    /// Chat requests started (one per transport call, retries inside it included).
    pub calls: u32,
}

impl RunUsage {
    fn add(&mut self, other: &RunUsage) {
        self.tokens.add(&other.tokens);
        self.uncosted.add(&other.uncosted);
        self.wait += other.wait;
        self.calls += other.calls;
        self.reported_cost_usd = match (self.reported_cost_usd, other.reported_cost_usd) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        };
    }
}

/// Usage one response reported. Every field is optional on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ResponseUsage {
    pub prompt: Option<u64>,
    pub completion: Option<u64>,
    pub total: Option<u64>,
    pub cost_usd: Option<f64>,
}

/// What the transports report as it happens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LlmEvent {
    /// Chat request `n` of this run (1-based) is starting.
    RequestStarted { n: u32 },
    /// Request `n` ended. `tokens` is what this request alone spent;
    /// `answered` is false when no body was read (refused, cancelled, dropped).
    RequestFinished {
        n: u32,
        tokens: TokenUsage,
        took: Duration,
        answered: bool,
    },
    /// The transport is sleeping before its next attempt.
    Waiting { wait: Duration, reason: WaitReason },
    /// That sleep ended (elapsed or cancelled).
    WaitOver,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitReason {
    /// Pacing to the provider's per-minute quota.
    Quota,
    /// Backing off after a failed attempt.
    Retry,
}

type Observer = Box<dyn Fn(&LlmEvent)>;

thread_local! {
    static RUN: RefCell<Option<RunUsage>> = const { RefCell::new(None) };
    static OBSERVER: RefCell<Option<Observer>> = RefCell::new(None);
}

/// Installs a zeroed accumulator for the current thread. On drop the previous
/// one is restored with this scope's total folded into it, so nested scopes
/// compose.
pub struct UsageScope {
    previous: Option<RunUsage>,
}

impl UsageScope {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let previous = RUN.with(|slot| slot.borrow_mut().replace(RunUsage::default()));
        Self { previous }
    }

    /// What this scope has accumulated so far.
    pub fn total(&self) -> RunUsage {
        current()
    }
}

impl Drop for UsageScope {
    fn drop(&mut self) {
        let mine = RUN.with(|slot| slot.borrow_mut().take());
        let restored = self.previous.take().map(|mut outer| {
            if let Some(mine) = mine {
                outer.add(&mine);
            }
            outer
        });
        RUN.with(|slot| *slot.borrow_mut() = restored);
    }
}

/// Installs an event observer for the current thread; restores the previous
/// one on drop. The observer must not install scopes of its own.
pub struct ObserverScope {
    previous: Option<Observer>,
}

impl ObserverScope {
    pub fn new(observer: impl Fn(&LlmEvent) + 'static) -> Self {
        let previous = OBSERVER.with(|slot| slot.borrow_mut().replace(Box::new(observer)));
        Self { previous }
    }
}

impl Drop for ObserverScope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        OBSERVER.with(|slot| *slot.borrow_mut() = previous);
    }
}

fn current() -> RunUsage {
    RUN.with(|slot| slot.borrow().unwrap_or_default())
}

fn with_run(f: impl FnOnce(&mut RunUsage)) {
    RUN.with(|slot| {
        if let Some(run) = slot.borrow_mut().as_mut() {
            f(run);
        }
    });
}

fn emit(event: LlmEvent) {
    OBSERVER.with(|slot| {
        if let Some(observer) = slot.borrow().as_ref() {
            observer(&event);
        }
    });
}

/// One response body was read. `usage` is `None` when the provider sent none;
/// the request still counts.
pub(crate) fn record(usage: Option<ResponseUsage>) {
    with_run(|run| {
        let u = usage.unwrap_or_default();
        let prompt = u.prompt.unwrap_or(0);
        let completion = u.completion.unwrap_or(0);
        let tokens = TokenUsage {
            prompt,
            completion,
            total: u.total.unwrap_or(prompt + completion),
            requests: 1,
        };
        run.add(&RunUsage {
            tokens,
            reported_cost_usd: u.cost_usd,
            uncosted: if u.cost_usd.is_some() {
                TokenUsage::default()
            } else {
                tokens
            },
            ..RunUsage::default()
        });
    });
}

/// One request that read a body carrying `usage`, counted exactly as a
/// transport counts it (start, usage, finish). For other crates' tests,
/// which cannot reach the transports.
#[doc(hidden)]
pub fn simulate_request(usage: Option<ResponseUsage>) {
    let _timer = WaitTimer::start();
    record(usage);
}

/// Times one chat request, from before pacing to after the last retry. The
/// elapsed time is added to the run on drop, whichever way the request ends,
/// and the observer sees the request start and finish.
pub(crate) struct WaitTimer {
    started: Instant,
    before: RunUsage,
    n: u32,
}

impl WaitTimer {
    pub(crate) fn start() -> Self {
        with_run(|run| run.calls += 1);
        let before = current();
        let n = before.calls;
        emit(LlmEvent::RequestStarted { n });
        Self {
            started: Instant::now(),
            before,
            n,
        }
    }
}

impl Drop for WaitTimer {
    fn drop(&mut self) {
        let took = self.started.elapsed();
        with_run(|run| run.wait += took);
        let after = current();
        let tokens = TokenUsage {
            prompt: after
                .tokens
                .prompt
                .saturating_sub(self.before.tokens.prompt),
            completion: after
                .tokens
                .completion
                .saturating_sub(self.before.tokens.completion),
            total: after.tokens.total.saturating_sub(self.before.tokens.total),
            requests: after
                .tokens
                .requests
                .saturating_sub(self.before.tokens.requests),
        };
        emit(LlmEvent::RequestFinished {
            n: self.n,
            tokens,
            took,
            answered: tokens.requests > 0,
        });
    }
}

/// Announces a transport sleep to the observer; announces its end on drop.
pub(crate) struct Waiting;

impl Waiting {
    pub(crate) fn new(wait: Duration, reason: WaitReason) -> Self {
        emit(LlmEvent::Waiting { wait, reason });
        Self
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        emit(LlmEvent::WaitOver);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn resp(prompt: u64, completion: u64) -> Option<ResponseUsage> {
        Some(ResponseUsage {
            prompt: Some(prompt),
            completion: Some(completion),
            total: None,
            cost_usd: None,
        })
    }

    #[test]
    fn three_calls_sum_into_the_run() {
        let scope = UsageScope::new();
        record(resp(1_000, 200));
        record(resp(2_000, 300));
        // A provider that sent no usage still costs a request.
        record(None);
        let total = scope.total();
        assert_eq!(
            total.tokens,
            TokenUsage {
                prompt: 3_000,
                completion: 500,
                total: 3_500,
                requests: 3,
            }
        );
        assert_eq!(total.reported_cost_usd, None);
    }

    #[test]
    fn no_scope_is_a_no_op_and_nested_scopes_fold_outward() {
        record(resp(9, 9)); // no scope: dropped
        let outer = UsageScope::new();
        record(resp(10, 1));
        {
            let inner = UsageScope::new();
            record(Some(ResponseUsage {
                cost_usd: Some(0.25),
                ..resp(5, 5).expect("usage")
            }));
            {
                let _t = WaitTimer::start();
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(inner.total().tokens.requests, 1);
            assert!(inner.total().wait >= Duration::from_millis(5));
        }
        let total = outer.total();
        assert_eq!(total.tokens.requests, 2, "inner folded into outer");
        assert_eq!(total.tokens.total, 21);
        assert_eq!(total.reported_cost_usd, Some(0.25));
        assert_eq!(
            total.uncosted,
            TokenUsage {
                prompt: 10,
                completion: 1,
                total: 11,
                requests: 1,
            },
            "the response without a cost is counted apart"
        );
        assert!(total.wait >= Duration::from_millis(5));
    }

    #[test]
    fn scope_does_not_leak_to_another_thread() {
        let scope = UsageScope::new();
        std::thread::spawn(|| record(resp(1, 1)))
            .join()
            .expect("joined");
        assert_eq!(scope.total().tokens.requests, 0);
    }

    #[test]
    fn observer_sees_each_request_with_its_own_tokens() {
        let seen: Rc<RefCell<Vec<LlmEvent>>> = Rc::default();
        let sink = seen.clone();
        let _usage = UsageScope::new();
        let _obs = ObserverScope::new(move |e| sink.borrow_mut().push(*e));
        {
            let _t = WaitTimer::start();
            let _w = Waiting::new(Duration::from_secs(42), WaitReason::Quota);
            record(resp(100, 10));
        }
        {
            let _t = WaitTimer::start();
            // Refused: nothing read.
        }
        let seen = seen.borrow();
        assert!(matches!(seen[0], LlmEvent::RequestStarted { n: 1 }));
        assert!(matches!(
            seen[1],
            LlmEvent::Waiting {
                reason: WaitReason::Quota,
                ..
            }
        ));
        assert!(matches!(seen[2], LlmEvent::WaitOver));
        match seen[3] {
            LlmEvent::RequestFinished {
                n,
                tokens,
                answered,
                ..
            } => {
                assert_eq!(n, 1);
                assert_eq!(tokens.total, 110);
                assert!(answered);
            }
            other => panic!("expected finish, got {other:?}"),
        }
        assert!(matches!(seen[4], LlmEvent::RequestStarted { n: 2 }));
        assert!(matches!(
            seen[5],
            LlmEvent::RequestFinished {
                n: 2,
                answered: false,
                ..
            }
        ));
    }
}
