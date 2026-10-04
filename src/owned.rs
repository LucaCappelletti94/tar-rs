use std::borrow::Cow;
use std::io::{self, Read};
use std::path::Path;

use crate::archive::ArchiveInner;
use crate::entry::EntryFields;
use crate::{other, Archive, Header, PaxExtensions};

/// An owning sequential TAR session that is `Send` when its reader is `Send`.
///
/// ```compile_fail
/// use tar::{OwnedArchive, OwnedStep};
/// let mut archive = OwnedArchive::new(std::io::empty());
/// if let OwnedStep::Entry(entry) = archive.advance().unwrap() {
///     let next = archive.advance();
///     drop(entry);
/// }
/// ```
pub struct OwnedArchive<R: Read> {
    archive: Archive<R>,
    next: u64,
    ended: bool,
    raw: bool,
}

/// The next member or the ended session, retaining ownership of the reader.
pub enum OwnedStep<R: Read> {
    /// A member whose completion returns the owning archive.
    Entry(OwnedEntry<R>),
    /// An ended archive from which the reader can be recovered.
    End(OwnedArchive<R>),
}

/// A bounded owning member that rejects further I/O after errors other than `Interrupted`.
pub struct OwnedEntry<R: Read> {
    fields: EntryFields<ArchiveInner<R>>,
    next: u64,
    raw: bool,
    failed: bool,
}

impl<R: Read> OwnedArchive<R> {
    /// Creates an owning session over a reader positioned at the archive start.
    pub fn new(reader: R) -> Self {
        Self {
            archive: Archive::new(reader),
            next: 0,
            ended: false,
            raw: false,
        }
    }

    /// Configures whether zero headers are skipped instead of ending the archive.
    pub fn set_ignore_zeros(&mut self, ignore: bool) {
        self.archive.set_ignore_zeros(ignore);
    }

    /// Configures raw iteration without GNU, PAX, or sparse preprocessing.
    pub fn raw(mut self, raw: bool) -> Self {
        self.raw = raw;
        self
    }

    /// Consumes the session to yield a member or end state, releasing it on error.
    pub fn advance(mut self) -> io::Result<OwnedStep<R>> {
        if self.ended {
            return Ok(OwnedStep::End(self));
        }
        let (entry, next) = self.archive.next_owned(self.next, self.raw)?;
        self.next = next;
        match entry {
            Some(fields) => Ok(OwnedStep::Entry(OwnedEntry {
                fields: fields.map_reader(self.archive.inner),
                next,
                raw: self.raw,
                failed: false,
            })),
            None => {
                self.ended = true;
                Ok(OwnedStep::End(self))
            }
        }
    }

    /// Returns the reader at its actual position without draining or validation.
    pub fn into_inner(self) -> R {
        self.archive.into_inner()
    }
}

impl<R: Read> OwnedEntry<R> {
    /// Returns the effective path including GNU and PAX metadata.
    pub fn path(&self) -> io::Result<Cow<'_, Path>> {
        self.fields.path()
    }

    /// Returns the effective path bytes including GNU and PAX metadata.
    pub fn path_bytes(&self) -> Cow<'_, [u8]> {
        self.fields.path_bytes()
    }

    /// Returns the effective link target including GNU and PAX metadata.
    pub fn link_name(&self) -> io::Result<Option<Cow<'_, Path>>> {
        self.fields.link_name()
    }

    /// Returns the effective link target bytes including GNU and PAX metadata.
    pub fn link_name_bytes(&self) -> Option<Cow<'_, [u8]>> {
        self.fields.link_name_bytes()
    }

    /// Returns PAX metadata, reading the body if this member is an extension header.
    pub fn pax_extensions(&mut self) -> io::Result<Option<PaxExtensions<'_>>> {
        if self.failed {
            return Err(other("cannot read a failed member"));
        }
        let result = self.fields.pax_extensions();
        if let Err(error) = &result {
            self.failed = error.kind() != io::ErrorKind::Interrupted;
        }
        result
    }

    /// Returns the header with the parser's metadata adjustments.
    pub fn header(&self) -> &Header {
        &self.fields.header
    }

    /// Returns the effective member size, including PAX and sparse adjustments.
    pub fn size(&self) -> u64 {
        self.fields.size
    }

    /// Returns the header's byte position in the uncompressed archive.
    pub fn raw_header_position(&self) -> u64 {
        self.fields.header_pos
    }

    /// Returns the body's byte position in the uncompressed archive.
    pub fn raw_file_position(&self) -> u64 {
        self.fields.file_pos
    }

    /// Consumes the member and skips unread data and padding, releasing state on error.
    pub fn finish(self) -> io::Result<OwnedArchive<R>> {
        if self.failed {
            return Err(other("cannot finish a failed member"));
        }
        let archive = Archive {
            inner: self.fields.reader,
        };
        archive.finish_owned(self.next)?;
        Ok(OwnedArchive {
            archive,
            next: self.next,
            ended: false,
            raw: self.raw,
        })
    }

    /// Abandons traversal and returns the reader without draining or validation.
    pub fn into_inner(self) -> R {
        Archive {
            inner: self.fields.reader,
        }
        .into_inner()
    }
}

impl<R: Read> Read for OwnedEntry<R> {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        if self.failed {
            return Err(other("cannot read a failed member"));
        }
        let result = self.fields.read(into);
        if let Err(error) = &result {
            self.failed = error.kind() != io::ErrorKind::Interrupted;
        }
        result
    }
}
