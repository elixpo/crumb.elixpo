use std::io;

fn main() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();

    crumb_cli::run(stdin.lock(), stdout.lock())
}
