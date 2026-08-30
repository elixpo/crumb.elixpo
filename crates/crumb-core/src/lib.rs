//! Provider-neutral domain types shared by crumb components.

/// A command handled directly by crumb rather than the native shell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BuiltInCommand {
    Auth(AuthAction),
    Exit,
    History(HistoryAction),
    Platform,
    Shell,
    Version,
}

/// Secure authentication action handled by crumb.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthAction {
    Login,
    Status,
    Logout,
}

/// Query performed by the local history built-in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HistoryAction {
    Recent,
    Search(String),
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
