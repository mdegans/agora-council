//! A Steward stop: Ctrl-C during a sitting.
//!
//! **The rule** (the Steward, 2026-10-17). On 2026-10-17 the Steward
//! stopped a sitting with Ctrl-C: the program died, no reason was on
//! record, and the meeting row stayed `active`. From the moment the Steward
//! confirms the sitting, Ctrl-C no longer kills the program. It is a
//! **Steward stop**:
//!
//! 1. the model call in flight, if any, is cancelled (its future is
//!    dropped; nothing from it is recorded);
//! 2. the Steward is asked for a reason, and nothing else is accepted: an
//!    empty line, a second Ctrl-C and Ctrl-D each ask again
//!    ([`ask_reason`]);
//! 3. the moment is sealed like a refusal's ([`crate::abort`]): every live
//!    conversation, the Clerk's briefing, a manifest, and the
//!    pending marker that blocks the next sitting until the Council has
//!    heard the stop (`resolve-refusal … heard:<GOV-or-REC id>`);
//! 4. the Steward's record is drafted and appended to the Governance Log
//!    automatically (`meeting::run`), and the meeting row is closed as an
//!    aborted sitting's is.
//!
//! Before the confirmation, Ctrl-C exits as it always did: nothing has been
//! sent to a seat. Answering "n" at "Start meeting?" is a cancel, not a
//! stop.
//!
//! **How a call is cancelled.** One listener ([`StopSignal::listen`])
//! takes SIGINT for the rest of the process once the sitting is confirmed,
//! and sets a flag. The steps that wait on a model (an item's rounds, the
//! Lawyer's decision summary and the seats' review of it) are raced
//! against it ([`race`]), so the stop drops the call where it stands. A
//! Ctrl-C at a Steward prompt never reaches the listener: the terminal is
//! in raw mode, and rustyline returns `Interrupted`, which the sitting's
//! [`crate::steward::Terminal`] turns into the same stop. A Ctrl-C during
//! a step that is not raced (a database write: a transaction is never cut
//! short) sets the flag, and the stop takes effect at the next prompt or
//! raced call.
//!
//! No enumerated power of the Steward's covers a stop, and it is not an
//! emergency action. It is recorded so that the Council can review it,
//! pending the Council's review of the rule itself.

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use agora_common::ids::CouncilMeetingId;

use crate::steward::StewardInput;

/// Where the program was when the Steward stopped it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopDuring {
    /// A model call was in flight; it was cancelled.
    ModelCall,
    /// The program was waiting on the Steward at a prompt.
    StewardPrompt,
    /// Between steps (a database write, or local work, which a stop never
    /// cuts short); the stop took effect at the next step.
    BetweenSteps,
}

impl fmt::Display for StopDuring {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            StopDuring::ModelCall => "while a model call was in flight (it was cancelled)",
            StopDuring::StewardPrompt => "at a Steward prompt",
            StopDuring::BetweenSteps => {
                "between steps (a database write or local work, which a stop does not cut \
                 short); it took effect at the next step"
            }
        })
    }
}

/// The Steward pressed Ctrl-C during the sitting. Returned (as an
/// `anyhow::Error`) from wherever it surfaced, so the code that holds the
/// live conversations can ask for the reason and seal them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StewardStopRequested {
    pub during: StopDuring,
}

impl fmt::Display for StewardStopRequested {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the Steward stopped the sitting, {}", self.during)
    }
}

impl std::error::Error for StewardStopRequested {}

impl StewardStopRequested {
    /// The request anywhere in `error`'s chain.
    pub fn find(error: &anyhow::Error) -> Option<Self> {
        error
            .chain()
            .find_map(|e| e.downcast_ref::<StewardStopRequested>())
            .copied()
    }
}

/// A Steward stop's facts, as sealed and recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopFacts {
    /// The Steward's reason, verbatim. `None` only when no reason could be
    /// read at all: the terminal's input closed or failed (see
    /// [`ask_reason`]); the record says so.
    pub reason: Option<String>,
    /// Where the program was.
    pub during: StopDuring,
    /// When Ctrl-C was taken (not when the reason was typed).
    pub stopped_at: DateTime<Utc>,
    /// When the reason was given.
    pub reason_at: DateTime<Utc>,
    /// The sitting, when the program knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meeting: Option<CouncilMeetingId>,
    /// The Steward's standing on the item stopped on: recused (`true`) or
    /// seated (`false`); `None` between items, or before the recusal
    /// determination was complete. Set by whoever took the stop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steward_recused: Option<bool>,
}

/// The flag Ctrl-C sets, shared by every raced step of the sitting.
#[derive(Clone, Default)]
pub struct StopSignal(Arc<Inner>);

#[derive(Default)]
struct Inner {
    requested: AtomicBool,
    notify: Notify,
    meeting: Option<CouncilMeetingId>,
}

impl StopSignal {
    /// A signal nothing triggers but [`trigger`](Self::trigger).
    pub fn new(meeting: Option<CouncilMeetingId>) -> Self {
        Self(Arc::new(Inner {
            meeting,
            ..Inner::default()
        }))
    }

    /// Take SIGINT for the rest of the process and turn each one into
    /// [`trigger`](Self::trigger). The handler is registered before this
    /// returns, so there is no window in which Ctrl-C still kills the
    /// program. Once registered it is never removed: a stopped sitting
    /// ends with the program, after its seal.
    pub fn listen(meeting: CouncilMeetingId) -> anyhow::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        let stop = Self::new(Some(meeting));
        let mut interrupts = signal(SignalKind::interrupt())?;
        let flag = stop.clone();
        tokio::spawn(async move {
            while interrupts.recv().await.is_some() {
                if !flag.is_requested() {
                    tracing::warn!(
                        event_type = "council_steward_stop_signal",
                        meeting = ?flag.meeting(),
                        "Ctrl-C during the sitting: a Steward stop"
                    );
                }
                flag.trigger();
            }
        });
        Ok(stop)
    }

    /// Request the stop.
    pub fn trigger(&self) {
        self.0.requested.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    pub fn is_requested(&self) -> bool {
        self.0.requested.load(Ordering::SeqCst)
    }

    /// The sitting this signal belongs to.
    pub fn meeting(&self) -> Option<CouncilMeetingId> {
        self.0.meeting
    }

    /// Resolves once the stop is requested (at once if it already is).
    pub async fn requested(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            // Registered before the flag is read, so a trigger between the
            // read and the await is not missed.
            notified.as_mut().enable();
            if self.is_requested() {
                return;
            }
            notified.await;
        }
    }
}

/// Run `step`, unless the Steward stops the sitting first: then `step` is
/// dropped where it stands (the call in flight is cancelled) and the error
/// is [`StewardStopRequested`] (a model call in flight). With no signal,
/// just `step`.
pub async fn race<T>(
    stop: Option<&StopSignal>,
    step: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let Some(stop) = stop else {
        return step.await;
    };
    tokio::select! {
        // A stop already requested wins before the step is polled at all.
        biased;
        () = stop.requested() => Err(StewardStopRequested {
            during: StopDuring::ModelCall,
        }
        .into()),
        result = step => result,
    }
}

/// How many times in a row the reason prompt may read end-of-file before
/// the terminal is taken to be gone. Ctrl-D asks again; a closed stdin
/// would ask forever and nothing would be sealed.
pub const EOF_LIMIT: u32 = 10;

/// Ask the Steward why he stopped the sitting, until he says. An empty
/// line is refused and asked again, and so are a second Ctrl-C
/// (`Interrupted`) and Ctrl-D (end of file). Only an input that is gone
/// (end of file [`EOF_LIMIT`] times in a row, or any other read error)
/// ends the asking without a reason: the stop is sealed all the same, and
/// its record says no reason could be read.
pub fn ask_reason(input: &mut impl StewardInput) -> Option<String> {
    println!();
    println!("  [Steward] You stopped the sitting (Ctrl-C). The call in flight, if any, was");
    println!("  cancelled and nothing from it is recorded. The stop is recorded in the");
    println!("  Governance Log with your reason, verbatim, and the next sitting is blocked");
    println!("  until the Council has heard it.");
    let mut eofs = 0;
    loop {
        match input.readline_reason("  Reason for the stop (required) > ") {
            Ok(line) => {
                eofs = 0;
                let reason = line.trim();
                if reason.is_empty() {
                    println!("  A reason is required. Say why the sitting was stopped.");
                    continue;
                }
                return Some(reason.to_string());
            }
            Err(e) => match ReasonReadError::classify(&e) {
                ReasonReadError::Interrupted => {
                    println!("  The sitting is already stopped; a reason is required.");
                }
                ReasonReadError::Eof => {
                    eofs += 1;
                    if eofs >= EOF_LIMIT {
                        tracing::error!(
                            event_type = "council_steward_stop_no_reason",
                            eofs,
                            "the reason prompt read end-of-file repeatedly; sealing the stop \
                             without a reason"
                        );
                        println!(
                            "  !! The terminal's input is closed. The stop is sealed and recorded \
                             without a reason."
                        );
                        return None;
                    }
                    println!("  A reason is required (Ctrl-D does not skip it).");
                }
                ReasonReadError::Other => {
                    tracing::error!(
                        event_type = "council_steward_stop_no_reason",
                        error = %format!("{e:#}"),
                        "the reason prompt could not read the terminal; sealing the stop \
                         without a reason"
                    );
                    println!(
                        "  !! The terminal could not be read ({e:#}). The stop is sealed and \
                         recorded without a reason."
                    );
                    return None;
                }
            },
        }
    }
}

/// What a failed read at the reason prompt was.
enum ReasonReadError {
    /// Ctrl-C again.
    Interrupted,
    /// Ctrl-D, or a closed input.
    Eof,
    /// Anything else: the terminal cannot be read.
    Other,
}

impl ReasonReadError {
    fn classify(error: &anyhow::Error) -> Self {
        if StewardStopRequested::find(error).is_some() {
            return Self::Interrupted;
        }
        match error.downcast_ref::<rustyline::error::ReadlineError>() {
            Some(rustyline::error::ReadlineError::Interrupted) => Self::Interrupted,
            Some(rustyline::error::ReadlineError::Eof) => Self::Eof,
            _ => Self::Other,
        }
    }
}

/// Take a stop: ask the reason and return its facts. `stop` names the
/// sitting, when the input carries the sitting's signal.
pub fn take(
    input: &mut impl StewardInput,
    requested: StewardStopRequested,
    stopped_at: DateTime<Utc>,
) -> StopFacts {
    let meeting = input.stop_signal().and_then(|s| s.meeting());
    let reason = ask_reason(input);
    StopFacts {
        reason,
        during: requested.during,
        stopped_at,
        reason_at: Utc::now(),
        meeting,
        steward_recused: None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    /// A Steward at the reason prompt: each entry is a line or a failed
    /// read.
    enum Line {
        Text(&'static str),
        Interrupted,
        Eof,
        Broken,
    }

    struct Script(VecDeque<Line>, usize);

    impl StewardInput for Script {
        fn readline(&mut self, _prompt: &str) -> anyhow::Result<String> {
            self.1 += 1;
            match self.0.pop_front().expect("the script ran out") {
                Line::Text(s) => Ok(s.to_string()),
                Line::Interrupted => Err(rustyline::error::ReadlineError::Interrupted.into()),
                Line::Eof => Err(rustyline::error::ReadlineError::Eof.into()),
                Line::Broken => Err(anyhow::anyhow!("the terminal is gone")),
            }
        }
    }

    #[test]
    fn the_reason_is_asked_until_one_is_given() {
        let mut input = Script(
            VecDeque::from([
                Line::Text(""),
                Line::Text("   "),
                Line::Interrupted,
                Line::Eof,
                Line::Text("  The Engineer's seat was misconfigured.  "),
            ]),
            0,
        );
        assert_eq!(
            ask_reason(&mut input).as_deref(),
            Some("The Engineer's seat was misconfigured.")
        );
        assert_eq!(input.1, 5, "asked five times");
        assert!(input.0.is_empty());
    }

    #[test]
    fn a_stop_request_at_the_reason_prompt_asks_again() {
        // A script that answers with the stop error itself (what the
        // sitting's terminal returns for Ctrl-C at an ordinary prompt).
        struct Again(u32);
        impl StewardInput for Again {
            fn readline(&mut self, _prompt: &str) -> anyhow::Result<String> {
                self.0 += 1;
                if self.0 < 3 {
                    Err(StewardStopRequested {
                        during: StopDuring::StewardPrompt,
                    }
                    .into())
                } else {
                    Ok("reason".to_string())
                }
            }
        }
        let mut input = Again(0);
        assert_eq!(ask_reason(&mut input).as_deref(), Some("reason"));
        assert_eq!(input.0, 3);
    }

    #[test]
    fn a_closed_input_ends_the_asking_without_a_reason() {
        let mut input = Script((0..EOF_LIMIT).map(|_| Line::Eof).collect(), 0);
        assert_eq!(ask_reason(&mut input), None);
        assert_eq!(input.1, EOF_LIMIT as usize);

        // A typed line resets the count: an empty one in between does not
        // let a later run of Ctrl-D end the asking early.
        let mut lines: VecDeque<Line> = (0..EOF_LIMIT - 1).map(|_| Line::Eof).collect();
        lines.push_back(Line::Text(""));
        lines.extend((0..EOF_LIMIT - 1).map(|_| Line::Eof));
        lines.push_back(Line::Text("given"));
        let mut input = Script(lines, 0);
        assert_eq!(ask_reason(&mut input).as_deref(), Some("given"));

        let mut input = Script(VecDeque::from([Line::Broken]), 0);
        assert_eq!(ask_reason(&mut input), None);
    }

    #[tokio::test]
    async fn race_cancels_the_step_when_the_stop_fires() {
        let stop = StopSignal::new(None);
        let trigger = stop.clone();
        let step = async move {
            trigger.trigger();
            std::future::pending::<()>().await;
            Ok(())
        };
        let err = race(Some(&stop), step).await.expect_err("stopped");
        assert_eq!(
            StewardStopRequested::find(&err),
            Some(StewardStopRequested {
                during: StopDuring::ModelCall
            })
        );

        // Already requested: the step is never polled.
        let polled = std::sync::atomic::AtomicBool::new(false);
        let err = race(Some(&stop), async {
            polled.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert!(err.is_err());
        assert!(!polled.load(Ordering::SeqCst));

        // No signal: the step runs as it is.
        assert_eq!(race(None, async { Ok(7) }).await.unwrap(), 7);
    }
}
