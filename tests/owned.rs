#![forbid(unsafe_code)]

use std::cell::Cell;
use std::io::{self, Cursor, ErrorKind, Read};
use std::sync::mpsc;
use std::time::Duration;

use tar::{Builder, EntryType, Header, OwnedArchive, OwnedEntry, OwnedStep};

fn entry<R: Read>(archive: OwnedArchive<R>) -> OwnedEntry<R> {
    match archive.advance().unwrap() {
        OwnedStep::Entry(entry) => entry,
        OwnedStep::End(_) => panic!("expected a member"),
    }
}

fn end<R: Read>(archive: OwnedArchive<R>) -> OwnedArchive<R> {
    match archive.advance().unwrap() {
        OwnedStep::End(archive) => archive,
        OwnedStep::Entry(_) => panic!("expected archive end"),
    }
}

fn append(builder: &mut Builder<Vec<u8>>, name: &str, body: &[u8]) {
    let mut header = Header::new_gnu();
    header.set_size(u64::try_from(body.len()).unwrap());
    builder.append_data(&mut header, name, body).unwrap();
}

fn two_members() -> Vec<u8> {
    let mut builder = Builder::new(Vec::new());
    append(&mut builder, "z", b"0123456789");
    append(&mut builder, "a", b"abcdefg");
    builder.into_inner().unwrap()
}

fn read_into(reader: &mut impl Read, buffer: &mut [u8], body: &mut Vec<u8>) {
    loop {
        let n = reader.read(buffer).unwrap();
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buffer[..n]);
    }
}

#[test]
fn nonseekable_members_are_bounded_and_finish_skips_remainder_and_padding() {
    let bytes = two_members();
    let mut buffer = [0; 3];
    for partial in [false, true] {
        let mut first = entry(OwnedArchive::new(&bytes[..]));
        assert_eq!(first.path_bytes().as_ref(), b"z");
        assert_eq!(first.size(), 10);
        assert_eq!(first.read(&mut buffer).unwrap(), 3);
        assert_eq!(buffer, *b"012");
        assert_eq!(first.read(&mut []).unwrap(), 0);
        if !partial {
            let mut body = buffer.to_vec();
            read_into(&mut first, &mut buffer, &mut body);
            assert_eq!(body, b"0123456789");
            assert_eq!(first.read(&mut buffer).unwrap(), 0);
        }
        let mut second = entry(first.finish().unwrap());
        assert_eq!(second.path_bytes().as_ref(), b"a");
        let mut body = Vec::new();
        read_into(&mut second, &mut buffer, &mut body);
        assert_eq!(body, b"abcdefg");
        let mut tail = end(second.finish().unwrap()).into_inner();
        assert_eq!(tail.read_to_end(&mut Vec::new()).unwrap(), 512);
    }
}

#[test]
fn gnu_names_links_and_raw_iteration() {
    let name = "long/".repeat(30);
    let target = "target/".repeat(30);
    let mut builder = Builder::new(Vec::new());
    append(&mut builder, &name, b"body");
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Symlink);
    header.set_size(0);
    builder.append_link(&mut header, "link", &target).unwrap();
    let bytes = builder.into_inner().unwrap();
    let mut member = entry(OwnedArchive::new(&bytes[..]));
    assert_eq!(member.path().unwrap().to_str(), Some(name.as_str()));
    let mut body = Vec::new();
    member.read_to_end(&mut body).unwrap();
    assert_eq!(body, b"body");
    let link = entry(member.finish().unwrap());
    assert_eq!(link.header().entry_type(), EntryType::Symlink);
    assert_eq!(
        link.link_name().unwrap().unwrap().to_str(),
        Some(target.as_str())
    );
    end(link.finish().unwrap());
    let mut raw = OwnedArchive::new(&bytes[..]).raw(true);
    for (kind, expected) in [
        (EntryType::GNULongName, name.as_bytes()),
        (EntryType::Regular, &b"body"[..]),
        (EntryType::GNULongLink, target.as_bytes()),
        (EntryType::Symlink, &b""[..]),
    ] {
        let mut member = entry(raw);
        assert_eq!(member.header().entry_type(), kind);
        body.clear();
        member.read_to_end(&mut body).unwrap();
        if kind.is_gnu_longname() || kind.is_gnu_longlink() {
            assert_eq!(body.pop(), Some(0));
        }
        assert_eq!(body, expected);
        raw = member.finish().unwrap();
    }
    end(raw);
}

#[test]
fn pax_metadata_and_size_do_not_leak_into_the_next_member() {
    let mut builder = Builder::new(Vec::new());
    builder
        .append_pax_extensions([("size", &b"8"[..]), ("path", &b"paxed"[..])])
        .unwrap();
    let mut header = Header::new_gnu();
    header.set_path("raw").unwrap();
    header.set_size(4);
    header.set_cksum();
    builder.append(&header, &b"01234567"[..]).unwrap();
    append(&mut builder, "next", b"tail");
    let bytes = builder.into_inner().unwrap();
    let mut first = entry(OwnedArchive::new(&bytes[..]));
    assert_eq!(first.header().size().unwrap(), 4);
    assert_eq!(first.size(), 8);
    assert_eq!(first.path_bytes().as_ref(), b"paxed");
    let attrs = first.pax_extensions().unwrap().unwrap();
    let pairs = attrs
        .map(|attr| {
            let attr = attr.unwrap();
            (attr.key_bytes().to_vec(), attr.value_bytes().to_vec())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        pairs,
        [
            (b"size".to_vec(), b"8".to_vec()),
            (b"path".to_vec(), b"paxed".to_vec())
        ]
    );
    let mut body = Vec::new();
    first.read_to_end(&mut body).unwrap();
    assert_eq!(body, b"01234567");
    let mut second = entry(first.finish().unwrap());
    assert_eq!(second.size(), 4);
    assert_eq!(second.path_bytes().as_ref(), b"next");
    body.clear();
    second.read_to_end(&mut body).unwrap();
    assert_eq!(body, b"tail");
    end(second.finish().unwrap());
}

#[test]
fn ignore_zeros_preserves_concatenated_archive_order() {
    let mut bytes = two_members();
    bytes.extend(two_members());
    let mut archive = OwnedArchive::new(&bytes[..]);
    archive.set_ignore_zeros(true);
    for (name, expected) in [("z", "0123456789"), ("a", "abcdefg")]
        .iter()
        .cycle()
        .take(4)
    {
        let mut member = entry(archive);
        assert_eq!(member.path_bytes().as_ref(), name.as_bytes());
        let mut body = String::new();
        member.read_to_string(&mut body).unwrap();
        assert_eq!(&body, expected);
        archive = member.finish().unwrap();
    }
    end(archive);
}

#[test]
fn valid_fixtures_match_borrowed_metadata_and_body_bytes() {
    macro_rules! fixture {
        ($name:literal) => {
            (
                $name,
                &include_bytes!(concat!("archives/", $name))[..],
                true,
            )
        };
    }
    for (name, bytes, read_body) in [
        fixture!("7z_long_path.tar"),
        fixture!("biguid_gnu.tar"),
        fixture!("biguid_pax.tar"),
        fixture!("directory.tar"),
        fixture!("duplicate_dirs.tar"),
        fixture!("file_times.tar"),
        fixture!("link.tar"),
        fixture!("pax-overrides-extension-header.tar"),
        fixture!("pax.tar"),
        fixture!("pax2.tar"),
        fixture!("pax_size.tar"),
        fixture!("reading_files.tar"),
        fixture!("simple.tar"),
        fixture!("spaces.tar"),
        fixture!("sparse-1.tar"),
        fixture!("sparse.tar"),
        fixture!("xattrs.tar"),
        (
            "sparse-large.tar",
            &include_bytes!("archives/sparse-large.tar")[..],
            false,
        ),
    ] {
        let mut borrowed = tar::Archive::new(bytes);
        let mut archive = OwnedArchive::new(bytes);
        for member in borrowed.entries().unwrap() {
            let mut member = member.unwrap();
            let mut owning = entry(archive);
            assert_eq!(owning.path_bytes(), member.path_bytes(), "{name}");
            assert_eq!(owning.link_name_bytes(), member.link_name_bytes(), "{name}");
            assert_eq!(
                owning.header().as_bytes(),
                member.header().as_bytes(),
                "{name}"
            );
            assert_eq!(owning.size(), member.size(), "{name}");
            assert_eq!(
                owning.raw_header_position(),
                member.raw_header_position(),
                "{name}"
            );
            assert_eq!(
                owning.raw_file_position(),
                member.raw_file_position(),
                "{name}"
            );
            if read_body {
                let mut expected = Vec::new();
                let mut actual = Vec::new();
                member.read_to_end(&mut expected).unwrap();
                owning.read_to_end(&mut actual).unwrap();
                assert_eq!(actual, expected, "{name}");
            }
            archive = owning.finish().unwrap();
        }
        end(archive);
    }
}

struct SendOnlyReader {
    bytes: Vec<u8>,
    position: Cell<usize>,
}

impl Read for SendOnlyReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let position = self.position.get();
        let n = (&self.bytes[position..]).read(buffer)?;
        self.position.set(position + n);
        Ok(n)
    }
}

#[test]
fn send_only_member_and_session_transfer_between_threads() {
    fn assert_send<T: Send>() {}
    fn assert_owned_send<R: Read + Send>() {
        assert_send::<OwnedArchive<R>>();
        assert_send::<OwnedEntry<R>>();
        assert_send::<OwnedStep<R>>();
    }
    assert_owned_send::<SendOnlyReader>();
    let first = entry(OwnedArchive::new(SendOnlyReader {
        bytes: two_members(),
        position: Cell::new(0),
    }));
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut first = first;
        let mut body = String::new();
        first.read_to_string(&mut body).unwrap();
        sender
            .send(("owned-member-transfer", body, first.finish().unwrap()))
            .unwrap();
    });
    let (id, body, archive) = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(id, "owned-member-transfer");
    assert_eq!(body, "0123456789");
    let mut second = entry(archive);
    let mut body = String::new();
    second.read_to_string(&mut body).unwrap();
    assert_eq!(body, "abcdefg");
    end(second.finish().unwrap());
}

#[test]
fn abandonment_recovers_actual_position_and_end_is_idempotent() {
    let bytes = two_members();
    let mut first = entry(OwnedArchive::new(Cursor::new(bytes.clone())));
    first.read_exact(&mut [0; 3]).unwrap();
    let cursor = first.into_inner();
    assert_eq!(cursor.position(), 515);
    assert_eq!(&cursor.get_ref()[515..], &bytes[515..]);
    let first = entry(OwnedArchive::new(Cursor::new(bytes)));
    let archive = first.finish().unwrap();
    let position = archive.into_inner().position();
    assert_eq!(position, 1024);
    let archive = end(OwnedArchive::new(Cursor::new(vec![0; 1024])));
    let cursor = end(end(archive)).into_inner();
    assert_eq!(cursor.position(), 512);
}

struct Faults<'a, R> {
    reader: R,
    remaining: usize,
    interrupted: bool,
    calls: &'a Cell<usize>,
}

impl<R: Read> Read for Faults<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.calls.set(self.calls.get() + 1);
        if self.remaining == 0 {
            if self.interrupted {
                self.interrupted = false;
                self.remaining = usize::MAX;
                return Err(ErrorKind::Interrupted.into());
            }
            return Err(io::Error::new(
                ErrorKind::Other,
                io::Error::from(ErrorKind::PermissionDenied),
            ));
        }
        let size = self.remaining.min(buffer.len());
        let n = self.reader.read(&mut buffer[..size])?;
        self.remaining -= n;
        Ok(n)
    }
}

#[test]
fn body_errors_are_terminal_and_interrupted_reads_retry() {
    let bytes = two_members();
    for interrupted in [false, true] {
        let calls = Cell::new(0);
        let reader = Faults {
            reader: &bytes[..],
            remaining: 516,
            interrupted,
            calls: &calls,
        };
        let mut first = entry(OwnedArchive::new(reader));
        let mut body = Vec::new();
        let result = first.read_to_end(&mut body);
        if interrupted {
            result.unwrap();
            assert_eq!(body, b"0123456789");
            let mut second = entry(first.finish().unwrap());
            body.clear();
            second.read_to_end(&mut body).unwrap();
            assert_eq!(body, b"abcdefg");
            end(second.finish().unwrap());
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Other);
            let source = error
                .get_ref()
                .unwrap()
                .downcast_ref::<io::Error>()
                .unwrap();
            assert_eq!(source.kind(), ErrorKind::PermissionDenied);
            let before = calls.get();
            assert!(first.read(&mut [0; 1]).is_err());
            assert!(first.finish().is_err());
            assert_eq!(calls.get(), before);
        }
    }
}

#[test]
fn truncated_finish_and_bad_headers_are_fatal() {
    let bytes = two_members();
    let first = entry(OwnedArchive::new(&bytes[..516]));
    assert!(first.finish().is_err());
    let mut corrupt = bytes;
    corrupt[0] ^= 1;
    assert!(OwnedArchive::new(&corrupt[..]).advance().is_err());
}
