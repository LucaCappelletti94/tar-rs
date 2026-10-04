#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::io::{self, BufRead, BufReader, Read};

use tar::{OwnedArchive, OwnedEntry, OwnedStep};

const BUFFER_SIZE: usize = 8;

enum State<R: Read> {
    Between(OwnedArchive<R>),
    Reading(BufReader<OwnedEntry<R>>),
    End(R),
}

pub struct Records<R: Read> {
    state: Option<State<R>>,
}

impl<R: Read> Records<R> {
    pub fn new(reader: R) -> Self {
        Self {
            state: Some(State::Between(OwnedArchive::new(reader))),
        }
    }

    pub fn into_inner(self) -> R {
        match self.state {
            Some(State::End(reader)) => reader,
            _ => panic!("record stream has not ended cleanly"),
        }
    }
}

impl<R: Read> Iterator for Records<R> {
    type Item = io::Result<String>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.state.take()? {
                State::Between(archive) => match archive.advance() {
                    Ok(OwnedStep::Entry(entry)) => {
                        let selected = match entry.path() {
                            Ok(path) => path.extension() == Some(OsStr::new("lines")),
                            Err(error) => return Some(Err(error)),
                        };
                        if selected {
                            self.state =
                                Some(State::Reading(BufReader::with_capacity(BUFFER_SIZE, entry)));
                        } else {
                            match entry.finish() {
                                Ok(archive) => self.state = Some(State::Between(archive)),
                                Err(error) => return Some(Err(error)),
                            }
                        }
                    }
                    Ok(OwnedStep::End(archive)) => {
                        self.state = Some(State::End(archive.into_inner()));
                    }
                    Err(error) => return Some(Err(error)),
                },
                State::Reading(mut body) => {
                    let mut line = String::new();
                    match body.read_line(&mut line) {
                        Ok(0) => match body.into_inner().finish() {
                            Ok(archive) => self.state = Some(State::Between(archive)),
                            Err(error) => return Some(Err(error)),
                        },
                        Ok(_) => {
                            self.state = Some(State::Reading(body));
                            return Some(Ok(line));
                        }
                        Err(error) => return Some(Err(error)),
                    }
                }
                State::End(reader) => {
                    self.state = Some(State::End(reader));
                    return None;
                }
            }
        }
    }
}

#[cfg(not(test))]
fn main() -> io::Result<()> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, body) in [
        ("z.lines", "z1\nz2\n"),
        ("ignored.bin", "skip this member"),
        ("a.lines", "a1\na2"),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(u64::try_from(body.len()).unwrap());
        builder.append_data(&mut header, path, body.as_bytes())?;
    }
    let bytes = builder.into_inner()?;
    let mut records = Records::new(&bytes[..]);
    for record in records.by_ref() {
        println!("{}", record?.trim_end_matches('\n'));
    }
    let mut reader = records.into_inner();
    let remaining = io::copy(&mut reader, &mut io::sink())?;
    assert_eq!(remaining, 512);
    println!("reader recovered, {remaining} trailing bytes drained");
    Ok(())
}
