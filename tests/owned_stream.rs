#![forbid(unsafe_code)]

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
#[path = "../examples/owned_lines.rs"]
mod owned_lines;
use owned_lines::Records;

fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, body) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(u64::try_from(body.len()).unwrap());
        builder.append_data(&mut header, name, *body).unwrap();
    }
    builder.into_inner().unwrap()
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn records_cross_members_and_threads_then_drain_all_gzip_members() {
    let tar = archive(&[
        ("z.lines", b"z1\nz2\nz3\n"),
        ("ignored.bin", b"not a record"),
        ("a.lines", b"a1\na2"),
    ]);
    let trailing = b"remaining gzip member";
    let mut input = gzip(&tar);
    input.extend(gzip(trailing));
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&input).unwrap();
    let stream = Records::new(MultiGzDecoder::new(file.reopen().unwrap()));
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stream = stream;
        let records = stream.by_ref().collect::<io::Result<Vec<_>>>().unwrap();
        assert!(stream.next().is_none());
        let mut decoder = stream.into_inner();
        let mut tail = Vec::new();
        decoder.read_to_end(&mut tail).unwrap();
        drop(decoder);
        sender
            .send(("gzip-reader-transfer", records, tail))
            .unwrap();
    });
    let (id, records, tail) = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(id, "gzip-reader-transfer");
    assert_eq!(records, ["z1\n", "z2\n", "z3\n", "a1\n", "a2"]);
    let mut expected = vec![0; 512];
    expected.extend_from_slice(trailing);
    assert_eq!(tail, expected);
}

#[test]
fn recovered_decoder_observes_later_gzip_footer_failure() {
    let tar = archive(&[("data.lines", b"first\nsecond\n")]);
    let mut input = gzip(&tar);
    let mut trailing = gzip(b"later member");
    let crc = trailing.len() - 8;
    trailing[crc] ^= 1;
    input.extend(trailing);
    let mut stream = Records::new(MultiGzDecoder::new(&input[..]));
    let records = stream.by_ref().collect::<io::Result<Vec<_>>>().unwrap();
    assert_eq!(records, ["first\n", "second\n"]);
    let mut decoder = stream.into_inner();
    let mut tail = Vec::new();
    assert!(decoder.read_to_end(&mut tail).is_err());
    assert!(tail.starts_with(&[0; 512]));
}

struct CountingReader<R> {
    reader: R,
    bytes: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let n = self.reader.read(buffer)?;
        self.bytes.fetch_add(n, Ordering::Relaxed);
        Ok(n)
    }
}

impl<R> Drop for CountingReader<R> {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn dropping_early_or_after_completion_releases_reader_without_extra_reads() {
    let body = "record\n".repeat(1024);
    let tar = archive(&[("data.lines", body.as_bytes())]);
    for early in [true, false] {
        let bytes = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let reader = CountingReader {
            reader: &tar[..],
            bytes: bytes.clone(),
            drops: drops.clone(),
        };
        let mut stream = Records::new(reader);
        assert_eq!(stream.next().unwrap().unwrap(), "record\n");
        if !early {
            let rest = stream.by_ref().collect::<io::Result<Vec<_>>>().unwrap();
            assert_eq!(rest.concat(), "record\n".repeat(1023));
        }
        let before = bytes.load(Ordering::Relaxed);
        if early {
            assert!(before < 512 + body.len());
        }
        drop(stream);
        assert_eq!(bytes.load(Ordering::Relaxed), before);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn early_drop_closes_owned_file_descriptor() {
    use std::os::unix::io::AsRawFd;

    let mut temporary = tempfile::NamedTempFile::new().unwrap();
    temporary
        .write_all(&archive(&[("data.lines", b"record\nnext\n")]))
        .unwrap();
    let file = temporary.reopen().unwrap();
    let descriptor = format!("/proc/self/fd/{}", file.as_raw_fd());
    let target = std::fs::read_link(&descriptor).unwrap();
    let mut stream = Records::new(file);
    assert_eq!(stream.next().unwrap().unwrap(), "record\n");
    drop(stream);
    assert_ne!(std::fs::read_link(descriptor).ok(), Some(target));
}
