use std::borrow::Cow;

use rustyline::config::{Configurer, EditMode};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::HistoryHinter;
use rustyline::{Completer, Editor, Helper, Hinter, Validator};

const PROMPT: &str = "debug> ";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Exit,
    Unknown(String),
}

#[derive(Completer, Helper, Hinter, Validator)]
struct ReplHelper {
    #[rustyline(Hinter)]
    hinter: HistoryHinter,
}

impl Highlighter for ReplHelper {
    fn highlight<'line>(&self, line: &'line str, _pos: usize) -> Cow<'line, str> {
        let command = line.split_whitespace().next().unwrap_or_default();
        let color = match command {
            "help" => "\x1b[36m",
            "exit" | "quit" => "\x1b[33m",
            "" => return Cow::Borrowed(line),
            _ => "\x1b[31m",
        };
        Cow::Owned(format!("{color}{line}\x1b[0m"))
    }

    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        Cow::Owned(format!("\x1b[1;35m{prompt}\x1b[0m"))
    }

    fn highlight_hint<'hint>(&self, hint: &'hint str) -> Cow<'hint, str> {
        Cow::Owned(format!("\x1b[2;90m{hint}\x1b[0m"))
    }
}

pub struct Repl {
    editor: Editor<ReplHelper, rustyline::history::DefaultHistory>,
}

impl Repl {
    pub fn new() -> rustyline::Result<Self> {
        let mut editor = Editor::new()?;
        editor.set_edit_mode(EditMode::Emacs);
        editor.set_auto_add_history(true);
        editor.set_helper(Some(ReplHelper {
            hinter: HistoryHinter {},
        }));
        Ok(Self { editor })
    }

    pub fn read_command(&mut self) -> rustyline::Result<Command> {
        match self.editor.readline(PROMPT) {
            Ok(line) => Ok(parse(&line)),
            Err(ReadlineError::Eof) => Ok(Command::Exit),
            Err(error) => Err(error),
        }
    }
}

fn parse(line: &str) -> Command {
    match line.trim() {
        "help" => Command::Help,
        "exit" | "quit" => Command::Exit,
        command => Command::Unknown(command.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert_eq!(parse(" help "), Command::Help);
        assert_eq!(parse("quit"), Command::Exit);
        assert_eq!(parse("wat"), Command::Unknown("wat".into()));
    }
}
