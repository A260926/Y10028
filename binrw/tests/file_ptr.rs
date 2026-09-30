use binrw::{
    BinRead, BinResult, Endian, Error,
    file_ptr::{FilePtr, FilePtrArgs},
    io::{Cursor, Read, Seek, SeekFrom},
};

/// A parser that fails after recording the exact position it was invoked at,
/// so tests can assert both the target location and error propagation.
#[derive(Debug, Eq, PartialEq)]
struct FailAt(u64);

impl core::fmt::Display for FailAt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "parser deliberately failed at 0x{:x}", self.0)
    }
}

impl std::error::Error for FailAt {}

fn failing_parser<R: Read + Seek>(reader: &mut R, _: Endian, (): ()) -> BinResult<u8> {
    Err(Error::Custom {
        pos: reader.stream_position().unwrap(),
        err: Box::new(FailAt(reader.stream_position().unwrap())),
    })
}

fn panicking_parser<R: Read + Seek>(reader: &mut R, _: Endian, (): ()) -> BinResult<u8> {
    panic!(
        "parser deliberately panicked at 0x{:x}",
        reader.stream_position().unwrap()
    )
}

/// Layout shared by the raw-parser tests:
///
/// ```text
/// 00 01 02 03 04 05 06 07 08 09 0a 0b 0c 0d 0e
///           [ptr = 4]                 [value]
/// ```
///
/// The caller position after the one-byte pointer is read is always 5. With
/// an absolute base of 10 the pointer lands at 14; with no base it lands at
/// 5 + 4 = 9. With the buggy `Current(offset)` seek the base was added to the
/// caller position, sending the reader to 19 instead of 14.
const DATA: &[u8] = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0xAA, 0xBB, 0xCC, 0xDD, 0xAB];
const CALLER_POS: u64 = 5;
const BASE: u64 = 10;

fn positioned_cursor() -> Cursor<&'static [u8]> {
    let mut cursor = Cursor::new(DATA);
    // Simulate a caller that is already partway through the stream.
    cursor.seek(SeekFrom::Start(4)).unwrap();
    cursor
}

fn args_with_base() -> FilePtrArgs<()> {
    FilePtrArgs::builder().offset(BASE).finalize()
}

#[test]
fn nonzero_offset_is_absolute_when_caller_position_nonzero() {
    let mut cursor = positioned_cursor();

    let result: FilePtr<u8, u8> = FilePtr::read_be_args(&mut cursor, args_with_base()).unwrap();

    assert_eq!(result.ptr, 4);
    assert_eq!(result.value, 0xAB);
    // The caller's stream must be left exactly where it was after the pointer
    // field, even though reading happened at an unrelated absolute offset.
    assert_eq!(cursor.stream_position().unwrap(), CALLER_POS);
}

#[test]
fn signed_pointer_relative_to_absolute_offset() {
    // A negative pointer must be measured from the absolute base, never from
    // `base + caller_position`. The pointer byte at index 4 is -2 (0xfe), so
    // the target is 11 + (-2) = 9; the buggy seek would start from
    // 11 + 5 = 16 and land at 14, past the end of the data.
    let mut data = DATA.to_vec();
    data[4] = 0xfe;
    let mut cursor = Cursor::new(data.as_slice());
    cursor.seek(SeekFrom::Start(4)).unwrap();

    let result: FilePtr<i8, u8> =
        FilePtr::read_be_args(&mut cursor, FilePtrArgs::builder().offset(11).finalize()).unwrap();

    assert_eq!(result.ptr, -2);
    assert_eq!(result.value, 9);
    assert_eq!(cursor.stream_position().unwrap(), CALLER_POS);
}

#[test]
fn nested_file_ptr_uses_absolute_offset() {
    #[derive(binrw::BinRead, Debug)]
    #[br(big)]
    struct Outer {
        _prefix: [u8; 4],
        // Caller position is 6 when this pointer is read, and its target must
        // still be resolved against the absolute base 8.
        #[br(offset = 8)]
        nested: FilePtr<u16, FilePtr<u16, u8>>,
    }

    // 0..4: prefix, 4..6: outer ptr = 0,
    // At 8: 00 0d (inner FilePtr ptr = 13; its default base 0 means it reads
    // from absolute position 13)
    // At 13: 7e
    let data: [u8; 14] = [
        0xAA, 0xAA, 0xAA, 0xAA, 0x00, 0x00, 0xBB, 0xBB, 0x00, 0x0D, 0xCC, 0xCC, 0xCC, 0x7E,
    ];
    let mut cursor = Cursor::new(data.as_slice());

    let result = Outer::read(&mut cursor).unwrap();

    assert_eq!(result.nested.ptr, 0);
    assert_eq!(result.nested.value.ptr, 13);
    assert_eq!(**result.nested, 0x7E);
    // Outer position is restored to just after the u16 outer pointer.
    assert_eq!(cursor.stream_position().unwrap(), 6);
}

#[test]
fn custom_parser_error_keeps_cause_and_restores_position() {
    let mut cursor = positioned_cursor();

    let parser = FilePtr::<u8, u8>::parse_with(failing_parser);
    let result = parser(&mut cursor, Endian::Big, args_with_base());

    let err = result.unwrap_err();
    // The original cause survives...
    assert_eq!(err.custom_err::<FailAt>(), Some(&FailAt(BASE + 4)));
    // ...with the precise position where the parser actually ran...
    match err {
        Error::Custom { pos, .. } => assert_eq!(pos, BASE + 4),
        other => panic!("expected a custom error, got {other:?}"),
    }
    // ...and the caller position is restored even on the error path.
    assert_eq!(cursor.stream_position().unwrap(), CALLER_POS);
}

#[test]
fn reader_position_restored_when_parser_panics() {
    let mut cursor = positioned_cursor();

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let parser = FilePtr::<u8, u8>::parse_with(panicking_parser);
        // The panic payload itself records where the parser ran; the buggy
        // seek would mention 0x13 instead of 0xe.
        parser(&mut cursor, Endian::Big, args_with_base()).unwrap();
    }));
    std::panic::set_hook(previous_hook);

    assert!(panic_result.is_err());
    let payload = panic_result.unwrap_err();
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("<non-string panic payload>");
    assert_eq!(
        message, "parser deliberately panicked at 0xe",
        "parser must run at absolute base + ptr, not at base + caller position + ptr"
    );
    // The observable invariant required by the drop path: the outer reader is
    // back at the caller position before the panic unwinds.
    assert_eq!(cursor.stream_position().unwrap(), CALLER_POS);
}

#[test]
fn zero_offset_is_absolute_file_start() {
    // With the default base of zero the pointer counts from the start of the
    // stream, regardless of the caller's current position.
    let mut cursor = Cursor::new(DATA);
    cursor.seek(SeekFrom::Start(4)).unwrap();

    let result: FilePtr<u8, u8> =
        FilePtr::read_be_args(&mut cursor, FilePtrArgs::default()).unwrap();

    assert_eq!(result.ptr, 4);
    assert_eq!(result.value, 4);
    assert_eq!(cursor.stream_position().unwrap(), CALLER_POS);
}
