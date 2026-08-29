//! Startup lifecycle for the `crumb` executable.

use std::io::{self, BufRead, Write};

use crumb_platform::Platform;

/// Starts crumb with injectable input and output for testing.
pub fn run<R: BufRead, W: Write>(reader: R, writer: W) -> io::Result<()> {
    crumb_repl::run(reader, writer, Platform::current(), env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    #[test]
    fn application_exits_cleanly() {
        let mut output = Vec::new();

        super::run(Cursor::new(":exit\n"), &mut output).expect("application should exit");

        assert!(String::from_utf8(output)
            .expect("output should be UTF-8")
            .starts_with("crumb:"));
    }
}
