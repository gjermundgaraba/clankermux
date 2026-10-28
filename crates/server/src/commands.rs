use std::collections::VecDeque;

use ghostty_vt::{PromptKind, SemanticPrompt};
use proto::Command;

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

    pub fn current(&self) -> Option<Command> {
        self.list.iter().rev().find(|c| c.output.is_some()).copied()
    }

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
        match (mark, open) {
            (SemanticPrompt::PromptStart(PromptKind::Primary), None) => {
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
                );
            }
            (SemanticPrompt::OutputStart, Some(command)) => command.output = Some(offset),
            (SemanticPrompt::OutputStart, None) => self.push(
                Command {
                    start: offset,
                    output: Some(offset),
                    end: None,
                    exit: None,
                },
                max,
            ),
            (SemanticPrompt::CommandEnd, _) => {
                match self.list.back_mut().filter(|c| c.end.is_none()) {
                    Some(command) => {
                        command.end = Some(offset);
                        command.exit = exit;
                    }
                    None => return false,
                }
            }
            (SemanticPrompt::PromptStart(_) | SemanticPrompt::InputStart, _) => return false,
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
    use SemanticPrompt::*;

    const PROMPT: SemanticPrompt = PromptStart(PromptKind::Primary);

    fn command(start: u64, output: Option<u64>, end: Option<u64>, exit: Option<i32>) -> Command {
        Command {
            start,
            output,
            end,
            exit,
        }
    }

    #[test]
    fn marks_make_ranges() {
        let mut c = Commands::default();
        assert!(!c.apply(CommandEnd, Some(0), 1, 2), "nothing to end");
        assert!(c.apply(PROMPT, None, 10, 2));
        assert!(!c.apply(InputStart, None, 12, 2));
        assert_eq!(c.all(), [command(10, None, None, None)]);
        assert_eq!(c.current(), None, "no output yet");
        assert!(c.apply(OutputStart, None, 20, 2));
        assert_eq!(c.current(), Some(command(10, Some(20), None, None)));
        assert!(c.apply(CommandEnd, Some(3), 30, 2));
        assert!(!c.apply(CommandEnd, Some(4), 31, 2), "ended already");
        let ran = command(10, Some(20), Some(30), Some(3));
        c.apply(PROMPT, None, 40, 2);
        assert_eq!(c.current(), Some(ran));
        c.apply(CommandEnd, None, 41, 2);
        c.apply(OutputStart, None, 50, 2);
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
        c.apply(PROMPT, None, 10, 4);
        assert!(!c.apply(PROMPT, None, 15, 4));
        for prompt in [
            PromptKind::Right,
            PromptKind::Continuation,
            PromptKind::Secondary,
        ] {
            assert!(!c.apply(PromptStart(prompt), None, 18, 4));
        }
        assert_eq!(c.all(), [command(10, None, None, None)]);
        assert!(c.apply(CommandEnd, Some(0), 20, 4));
        assert!(c.apply(PROMPT, None, 21, 4));
        assert_eq!(c.all(), [command(21, None, None, None)]);
    }
}
