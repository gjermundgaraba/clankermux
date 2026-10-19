//! Command marks (OSC 133): each command as a range of stream offsets.

use std::collections::VecDeque;

use ghostty_vt::{PromptKind, SemanticPrompt, SemanticPromptKind};
use proto::Command;

/// The last commands of one session, oldest first.
#[derive(Default)]
pub struct Commands {
    list: VecDeque<Command>,
}

impl Commands {
    pub fn load(commands: Vec<Command>) -> Self {
        Commands {
            list: commands.into(),
        }
    }

    pub fn all(&self) -> Vec<Command> {
        self.list.iter().copied().collect()
    }

    /// The newest command whose output began: the one running, or the
    /// last that ran. `Session.command`, and what `last_command` reads.
    pub fn current(&self) -> Option<Command> {
        self.list.iter().rev().find(|c| c.output.is_some()).copied()
    }

    /// Applies a mark at `offset`. A primary prompt (`A`, `P;k=i`) starts
    /// a command, unless the newest is a prompt still waiting (a redraw:
    /// nothing changes) or replaces it when it ended without output (an
    /// empty line). `C` sets the output of a waiting prompt, or starts a
    /// command there. `D` ends the newest command unless it has ended.
    /// `B` and other prompts (right, continuation, secondary) change
    /// nothing. Beyond `max` commands the oldest goes. Returns whether
    /// anything changed.
    pub fn apply(
        &mut self,
        mark: SemanticPrompt,
        exit: Option<i32>,
        offset: u64,
        max: usize,
    ) -> bool {
        let open = self
            .list
            .back_mut()
            .filter(|c| c.end.is_none() && c.output.is_none());
        match (mark.kind, open) {
            (SemanticPromptKind::PromptStart, _) if mark.prompt != PromptKind::Primary => {
                return false;
            }
            (SemanticPromptKind::PromptStart, Some(_)) => return false,
            (SemanticPromptKind::PromptStart, None) => {
                if self.list.back().is_some_and(|c| c.output.is_none()) {
                    self.list.pop_back();
                }
                self.push(
                    Command {
                        start: offset,
                        output: None,
                        end: None,
                        exit: None,
                    },
                    max,
                )
            }
            (SemanticPromptKind::OutputStart, Some(command)) => command.output = Some(offset),
            (SemanticPromptKind::OutputStart, None) => self.push(
                Command {
                    start: offset,
                    output: Some(offset),
                    end: None,
                    exit: None,
                },
                max,
            ),
            (SemanticPromptKind::CommandEnd, _) => {
                match self.list.back_mut().filter(|c| c.end.is_none()) {
                    Some(command) => {
                        command.end = Some(offset);
                        command.exit = exit;
                    }
                    None => return false,
                }
            }
            (SemanticPromptKind::InputStart, _) => return false,
        }
        true
    }

    fn push(&mut self, command: Command, max: usize) {
        self.list.push_back(command);
        while self.list.len() > max {
            self.list.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SemanticPromptKind::*;

    fn command(start: u64, output: Option<u64>, end: Option<u64>, exit: Option<i32>) -> Command {
        Command {
            start,
            output,
            end,
            exit,
        }
    }

    fn mark(kind: SemanticPromptKind) -> SemanticPrompt {
        SemanticPrompt {
            kind,
            prompt: PromptKind::Primary,
        }
    }

    #[test]
    fn marks_make_ranges() {
        let mut c = Commands::default();
        assert!(!c.apply(mark(CommandEnd), Some(0), 1, 2), "nothing to end");
        assert!(c.apply(mark(PromptStart), None, 10, 2));
        assert!(!c.apply(mark(InputStart), None, 12, 2));
        assert_eq!(c.all(), [command(10, None, None, None)]);
        assert_eq!(c.current(), None, "no output yet");
        assert!(c.apply(mark(OutputStart), None, 20, 2));
        assert_eq!(c.current(), Some(command(10, Some(20), None, None)));
        assert!(c.apply(mark(CommandEnd), Some(3), 30, 2));
        assert!(!c.apply(mark(CommandEnd), Some(4), 31, 2), "ended already");
        let ran = command(10, Some(20), Some(30), Some(3));
        // At the next prompt the last command is still the one that ran.
        c.apply(mark(PromptStart), None, 40, 2);
        assert_eq!(c.current(), Some(ran));
        // Output without a prompt starts a command; the oldest goes.
        c.apply(mark(CommandEnd), None, 41, 2);
        c.apply(mark(OutputStart), None, 50, 2);
        assert_eq!(
            c.all(),
            [
                command(40, None, Some(41), None),
                command(50, Some(50), None, None)
            ]
        );
    }

    #[test]
    fn redraws_other_prompts_and_empty_lines_add_nothing() {
        let mut c = Commands::default();
        c.apply(mark(PromptStart), None, 10, 4);
        // A redraw, and the prompts of further lines.
        assert!(!c.apply(mark(PromptStart), None, 15, 4));
        for prompt in [
            PromptKind::Right,
            PromptKind::Continuation,
            PromptKind::Secondary,
        ] {
            let other = SemanticPrompt {
                prompt,
                ..mark(PromptStart)
            };
            assert!(!c.apply(other, None, 18, 4));
        }
        assert_eq!(c.all(), [command(10, None, None, None)]);
        // An empty line ends the prompt; the next one replaces it.
        assert!(c.apply(mark(CommandEnd), Some(0), 20, 4));
        assert!(c.apply(mark(PromptStart), None, 21, 4));
        assert_eq!(c.all(), [command(21, None, None, None)]);
    }
}
