//! Round trips: write documents with ciclostile and read them back with the
//! `zip` crate. Whatever no replacement touched comes out exactly as it
//! went in.

pub mod common;

use std::io::{self, Write};

use ciclostile::Docx;
use common::Fixture;

#[test]
fn untouched_entries_are_copied() {
    // The template has deflated XML parts, a stored relationships part, a
    // stored picture and a directory. Read raw, without decompressing: the
    // compressed entries and the directory are copied byte for byte, and
    // the stored ones keep their content, CRC-32 and size but come out
    // compressed.
    let zip = Fixture::template().zip();
    let output = Docx::from_bytes(zip.clone()).unwrap().to_bytes();
    common::assert_copied(&zip, &output);
}

#[test]
fn rewriting_an_output_changes_nothing() {
    let once = common::untouched(&Fixture::template().zip());
    let twice = common::untouched(&once);
    assert_eq!(twice, once);
}

#[test]
fn every_entry_reads_back() {
    let fixture = Fixture::template();
    let output = Docx::from_bytes(fixture.zip()).unwrap().to_bytes();
    assert_eq!(common::names(&output), fixture.names());
    for name in fixture.names() {
        if !name.ends_with('/') {
            assert_eq!(common::read(&output, name), fixture.content(name), "{name}");
        }
    }
}

#[test]
fn write_to_writes_what_to_bytes_returns() {
    let docx = Docx::from_bytes(Fixture::template().zip()).unwrap();
    let mut written = Vec::new();
    // Any writer works, trait objects included.
    let writer: &mut dyn Write = &mut written;
    docx.write_to(writer).unwrap();
    assert_eq!(written, docx.to_bytes());
}

#[test]
fn output_only_depends_on_the_input() {
    let zip = Fixture::template().zip();
    let first = Docx::from_bytes(zip.clone()).unwrap();
    let second = Docx::from_bytes(zip).unwrap();
    assert_eq!(first.to_bytes(), first.to_bytes());
    assert_eq!(first.to_bytes(), second.to_bytes());
}

#[test]
fn writer_errors_are_returned_unchanged() {
    /// Accepts `room` bytes, then fails like a full disk.
    struct Full {
        room: usize,
    }

    impl Write for Full {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.room == 0 {
                return Err(io::Error::new(io::ErrorKind::StorageFull, "disk full"));
            }
            let written = buf.len().min(self.room);
            self.room -= written;
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let docx = Docx::from_bytes(Fixture::template().zip()).unwrap();
    let size = docx.to_bytes().len();
    for room in [0, 1, 1000, size / 2, size - 1] {
        let err = docx.write_to(&mut Full { room }).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::StorageFull, "room {room}");
        assert_eq!(err.to_string(), "disk full", "room {room}");
    }
}

#[test]
fn clones_write_the_same_document() {
    let docx = Docx::from_bytes(Fixture::template().zip()).unwrap();
    assert_eq!(docx.clone().to_bytes(), docx.to_bytes());
}

#[test]
fn debug_does_not_dump_the_xml() {
    // The parts of a document can be megabytes long.
    let docx = Docx::from_bytes(Fixture::template().zip()).unwrap();
    let debug = format!("{docx:?}");
    assert!(
        !debug.contains("w:body") && !debug.contains("{{"),
        "{debug}"
    );
}
