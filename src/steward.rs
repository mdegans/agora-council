use rustyline::Editor;
use rustyline::history::DefaultHistory;

use crate::models::{
    CouncilVote, DecisionCategory, SeatVotes, Statement, StewardAction, StewardStanding,
    StewardVote,
};

/// Where the Steward's answers come from: the terminal in a sitting, a
/// script in tests.
pub trait StewardInput {
    fn readline(&mut self, prompt: &str) -> anyhow::Result<String>;

    /// A line at the Steward-stop reason prompt ([`crate::stop::ask_reason`]):
    /// the read with nothing in the way, so a second Ctrl-C comes back as
    /// itself and is asked again rather than starting another stop.
    fn readline_reason(&mut self, prompt: &str) -> anyhow::Result<String> {
        self.readline(prompt)
    }

    /// The sitting's stop signal, once the sitting is confirmed: what the
    /// steps that wait on a model race against ([`crate::stop::race`]).
    /// `None` for a script, and before the confirmation.
    fn stop_signal(&self) -> Option<crate::stop::StopSignal> {
        None
    }
}

impl StewardInput for Editor<(), DefaultHistory> {
    fn readline(&mut self, prompt: &str) -> anyhow::Result<String> {
        Ok(Editor::readline(self, prompt)?)
    }
}

/// The Steward's terminal in a sitting. Before [`arm`](Self::arm) it is
/// the plain editor, and Ctrl-C at a prompt exits as it always did. Armed
/// (from the confirmation on), Ctrl-C at any prompt is a Steward stop
/// ([`crate::stop`]), and so is any prompt reached after a Ctrl-C the
/// listener took between steps.
pub struct Terminal {
    editor: Editor<(), DefaultHistory>,
    stop: Option<crate::stop::StopSignal>,
}

impl Terminal {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            editor: Editor::new()?,
            stop: None,
        })
    }

    /// The editor itself, for the prompts before the sitting is confirmed.
    pub fn editor(&mut self) -> &mut Editor<(), DefaultHistory> {
        &mut self.editor
    }

    /// From here on, Ctrl-C is a Steward stop.
    pub fn arm(&mut self, stop: crate::stop::StopSignal) {
        self.stop = Some(stop);
    }
}

impl StewardInput for Terminal {
    fn readline(&mut self, prompt: &str) -> anyhow::Result<String> {
        use crate::stop::{StewardStopRequested, StopDuring};
        let Some(stop) = &self.stop else {
            return Ok(self.editor.readline(prompt)?);
        };
        // Ctrl-C during a step that is not raced (a database write) set
        // the flag; it takes effect here, before anything else is asked.
        if stop.is_requested() {
            return Err(StewardStopRequested {
                during: StopDuring::BetweenSteps,
            }
            .into());
        }
        match self.editor.readline(prompt) {
            Ok(line) => Ok(line),
            Err(rustyline::error::ReadlineError::Interrupted) => {
                stop.trigger();
                Err(StewardStopRequested {
                    during: StopDuring::StewardPrompt,
                }
                .into())
            }
            Err(e) => Err(e.into()),
        }
    }

    fn readline_reason(&mut self, prompt: &str) -> anyhow::Result<String> {
        Ok(self.editor.readline(prompt)?)
    }

    fn stop_signal(&self) -> Option<crate::stop::StopSignal> {
        self.stop.clone()
    }
}

// There was a `prompt_refusal` here: on a final vote or ranking that drew
// a refusal, the Steward chose retry once / table / abort. Removed with
// the refusal-abort rule (Steward, 2026-09-29; supersedes GOV-2026-0011's
// abstain condition pending the Council's approval of the amendment): any
// refusal aborts the sitting, nothing is retried, and no retry is offered.

/// Where the item stands when the Steward is asked what to do next, so
/// the menu can say what the next step actually is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextStep {
    /// Another deliberation round follows unless the Steward calls the
    /// vote.
    Round(i32),
    /// Every seat set `ready_to_vote` in the round just finished: the final
    /// vote follows whatever the Steward chooses (short of tabling). Notes
    /// typed here are read to the seats with the vote call (#450 — they
    /// used to be silently dropped at this prompt).
    FinalVote,
}

/// What a recused Steward is told when asking for what the item's
/// recusal forbids.
pub const RECUSED_REFUSAL: &str = "  Refused: you are recused from this item (Constitution Art. IV § 3, \
     GOV-2026-0009): no deliberation, no vote and no veto. No notes, no \
     tabling, and no say in when the vote is called.";

/// Prompt the Steward for their action between rounds.
///
/// A recused Steward is never asked (Constitution Art. IV § 3): no notes,
/// no tabling, and no say in when deliberation ends, since choosing when
/// to cut it off is influence (Steward, 2026-10-04, replacing the
/// 2026-09-28 ruling on agora#468 that let a recused Steward continue or
/// call the vote). The deliberation loop advances a recused item on its
/// own; asking here for a recused Steward is refused as an error, so no
/// caller can reopen the menu.
pub fn prompt_action(
    input: &mut impl StewardInput,
    next: NextStep,
    standing: StewardStanding,
) -> anyhow::Result<StewardAction> {
    anyhow::ensure!(
        standing == StewardStanding::Seated,
        "{}",
        RECUSED_REFUSAL.trim()
    );
    println!();
    match next {
        NextStep::Round(round_number) => {
            println!("  [Steward] Round {round_number} — what would you like to do?");
            println!("    (n) Add notes for this round");
            println!("    (v) Call for final vote");
            println!("    (s) Skip/defer this item");
            println!("    (c) Continue without notes");
        }
        NextStep::FinalVote => {
            println!("  [Steward] All seats are ready to vote — the final vote is next.");
            println!("    (n) Add notes, read to the seats with the vote call");
            println!("    (v) Call the final vote (no notes)");
            println!("    (s) Skip/defer this item");
        }
    }

    loop {
        let answer = input.readline("  > ")?;
        let trimmed = answer.trim().to_lowercase();

        match trimmed.as_str() {
            "n" | "notes" => {
                let notes = prompt_multiline(input, "  Enter your notes (blank line to finish):")?;
                if notes.is_empty() {
                    return Ok(match next {
                        NextStep::Round(_) => {
                            println!("  No notes entered. Continuing without notes.");
                            StewardAction::Continue
                        }
                        NextStep::FinalVote => {
                            println!("  No notes entered. Calling the vote.");
                            StewardAction::CallVote
                        }
                    });
                }
                if next == NextStep::FinalVote {
                    println!("  Notes will be read to the seats with the vote call.");
                }
                return Ok(StewardAction::AddNotes(notes));
            }
            "v" | "vote" => return Ok(StewardAction::CallVote),
            "s" | "skip" => return Ok(StewardAction::Skip),
            // At the all-ready prompt there is no round to continue into:
            // "continue" means the vote, and the menu says so.
            "c" | "continue" | "" => {
                return Ok(match next {
                    NextStep::Round(_) => StewardAction::Continue,
                    NextStep::FinalVote => StewardAction::CallVote,
                });
            }
            _ => match next {
                NextStep::Round(_) => println!("  Unrecognized. Enter n/v/s/c."),
                NextStep::FinalVote => println!("  Unrecognized. Enter n/v/s."),
            },
        }
    }
}

/// Prompt the Steward for their vote on the final decision.
///
/// A recused Steward is not asked: the vote is [`StewardVote::Recused`],
/// with no veto and no rationale, and nothing is read from the input
/// (Constitution Art. IV § 3). An abstention and a veto are each asked for
/// their rationale, and a blank one is refused (Art. IV § 3 and § 5): the
/// type holds it, so neither can be recorded without one. This is the only
/// place the deliberation loop gets the Steward's vote from.
pub fn prompt_vote(
    editor: &mut impl StewardInput,
    category: &DecisionCategory,
    seats: &SeatVotes,
    standing: StewardStanding,
) -> anyhow::Result<StewardVote> {
    println!();
    println!("  [Final Votes]");
    for (seat, vote) in seats.iter() {
        println!("    {seat}: {}", CouncilVote::from(vote.choice));
    }
    if standing == StewardStanding::Recused {
        println!(
            "    Steward: Recused (Art. IV § 3) — no vote and no veto. Threshold: {}",
            category
                .recused_threshold()
                .unwrap_or_else(|| category.threshold())
        );
        return Ok(StewardVote::Recused);
    }

    println!(
        "    Agent tally: {}-{}",
        seats.yes_count(),
        seats.no_count()
    );
    println!(
        "    Category: {category} (threshold: {})",
        category.threshold()
    );
    println!();
    println!("  [Steward] Cast your vote:");
    println!("    (y) Yes / Concur");
    println!("    (n) No");
    println!("    (a) Abstain");
    println!("    (x) Veto (requires rationale)");

    loop {
        let input = editor.readline("  > ")?;
        let trimmed = input.trim().to_lowercase();

        match trimmed.as_str() {
            "y" | "yes" | "concur" => {
                // For Policy items, Steward concurs rather than voting yes
                if *category == DecisionCategory::Policy {
                    return Ok(StewardVote::Concur);
                }
                return Ok(StewardVote::Yes);
            }
            "n" | "no" => return Ok(StewardVote::No),
            "a" | "abstain" => {
                let rationale = prompt_multiline(
                    editor,
                    "  Abstention rationale, one sentence for the Governance Log \
                     (Art. IV § 3; blank line to finish):",
                )?;
                let Ok(rationale) = Statement::try_from(rationale) else {
                    println!("  An abstention requires a rationale (Art. IV § 3). Try again.");
                    continue;
                };
                println!("  Abstention rationale recorded.");
                return Ok(StewardVote::Abstain { rationale });
            }
            "x" | "veto" => {
                let rationale =
                    prompt_multiline(editor, "  Enter veto rationale (blank line to finish):")?;
                let Ok(rationale) = Statement::try_from(rationale) else {
                    println!("  Veto requires a rationale. Try again.");
                    continue;
                };
                println!("  Veto rationale recorded.");
                return Ok(StewardVote::Veto { rationale });
            }
            _ => {
                println!("  Unrecognized. Enter y/n/a/x.");
            }
        }
    }
}

/// A yes/no answer: `y`, `yes`, `n` or `no`, or empty for `default`.
/// `None` for anything else, which the caller asks again. On 2026-10-18
/// the Steward typed a decision's tags one prompt early, at the landmark
/// question, and "anything not starting with y" took them as "no".
fn yes_no(answer: &str, default: bool) -> Option<bool> {
    match answer.trim().to_lowercase().as_str() {
        "" => Some(default),
        "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

/// Ask the Steward if this should be a landmark decision.
pub fn prompt_landmark(editor: &mut impl StewardInput) -> anyhow::Result<bool> {
    loop {
        let input = editor.readline("  Mark as landmark decision? (y/N) > ")?;
        match yes_no(&input, false) {
            Some(landmark) => return Ok(landmark),
            None => println!("  Unrecognized. Enter y or n."),
        }
    }
}

/// Ask the Steward for tags.
pub fn prompt_tags(editor: &mut impl StewardInput) -> anyhow::Result<Vec<String>> {
    let input = editor.readline("  Tags (comma-separated, or blank): > ")?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    Ok(trimmed
        .split(',')
        .map(|t| t.trim().to_lowercase().replace(' ', "-"))
        .filter(|t| !t.is_empty())
        .collect())
}

/// Prompt for multi-line text input. Ends on a blank line.
fn prompt_multiline(editor: &mut impl StewardInput, prompt_msg: &str) -> anyhow::Result<String> {
    read_multiline(editor, prompt_msg)
}

/// Read multi-line text from any Steward input. Ends on a blank line.
pub fn read_multiline(input: &mut impl StewardInput, prompt_msg: &str) -> anyhow::Result<String> {
    println!("{prompt_msg}");
    let mut lines = Vec::new();

    loop {
        let line = input.readline("  | ")?;
        if line.trim().is_empty() {
            break;
        }
        lines.push(line);
    }

    Ok(lines.join("\n"))
}

/// Prompt the Steward to confirm meeting start.
pub fn confirm_meeting(
    editor: &mut Editor<(), DefaultHistory>,
    item_count: usize,
) -> anyhow::Result<bool> {
    println!();
    println!("  {item_count} agenda item(s) ready for deliberation.");
    // Nothing has been sent to a seat yet: Ctrl-C here cancels, like "n",
    // and the meeting is closed rather than left `active`.
    loop {
        let input = match editor.readline("  Start meeting? (Y/n) > ") {
            Err(rustyline::error::ReadlineError::Interrupted) => return Ok(false),
            other => other?,
        };
        match yes_no(&input, true) {
            Some(start) => return Ok(start),
            None => println!("  Unrecognized. Enter y or n."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers typed in order, as the Steward would.
    struct Typed(std::collections::VecDeque<&'static str>);

    impl StewardInput for Typed {
        fn readline(&mut self, _prompt: &str) -> anyhow::Result<String> {
            self.0
                .pop_front()
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("no more input"))
        }
    }

    #[test]
    fn tags_typed_at_the_landmark_prompt_are_asked_again() {
        let mut input = Typed(["justice,transparency,data", "y"].into());
        assert!(prompt_landmark(&mut input).unwrap());
        assert!(input.0.is_empty());
    }

    #[test]
    fn yes_no_takes_only_yes_no_or_the_default() {
        for (answer, default, expected) in [
            ("", false, Some(false)),
            ("", true, Some(true)),
            (" Y ", false, Some(true)),
            ("yes", false, Some(true)),
            ("N", true, Some(false)),
            ("no", true, Some(false)),
            ("yeah", false, None),
            ("justice", false, None),
        ] {
            assert_eq!(yes_no(answer, default), expected, "{answer:?}");
        }
    }
}
