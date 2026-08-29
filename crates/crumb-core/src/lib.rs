//! Provider-neutral domain types shared by crumb components.

/// A command handled directly by crumb rather than the native shell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuiltInCommand {
    Exit,
    Platform,
    Version,
}

/// A classified line of terminal input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputEvent {
    BuiltIn(BuiltInCommand),
    NativeInput(String),
}

#[cfg(test)]
mod tests {
    use super::{BuiltInCommand, InputEvent};

    #[test]
    fn native_input_preserves_the_command() {
        let event = InputEvent::NativeInput("git status".to_owned());

        assert_eq!(event, InputEvent::NativeInput("git status".to_owned()));
    }

    #[test]
    fn built_in_is_typed() {
        let event = InputEvent::BuiltIn(BuiltInCommand::Platform);

        assert_eq!(event, InputEvent::BuiltIn(BuiltInCommand::Platform));
    }
}
