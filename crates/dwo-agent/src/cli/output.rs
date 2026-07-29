use std::fmt;
use std::io::{self, Write};

pub fn write(arguments: fmt::Arguments<'_>) -> io::Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    output.write_fmt(arguments)
}

pub fn line(arguments: fmt::Arguments<'_>) -> io::Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    output.write_fmt(arguments)?;
    output.write_all(b"\n")
}
