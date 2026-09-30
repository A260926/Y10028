use std::panic::{self, AssertUnwindSafe};

use binrw::{
    BinRead, Endian, Error, FilePtr,
    file_ptr::FilePtrArgs,
    io::{Cursor, Read, Seek, SeekFrom},
};

/// Reads a `u8` while recording the reader position where parsing started, so
/// a test can observe the exact location the inner parser was invoked at.
struct Marked {
    pos: u64,
    value: u8,
}

impl BinRead for Marked {
    type Args<'a> = ();

    fn read_options<R: Read + Seek>(
        reader: &mut R,
        _endian: Endian,
        _args: Self::Args<'_>,
    ) -> binrw::BinResult<Self> {
        let pos = reader.stream_position()?;
        let mut value = 0u8;
        reader.read_exact(core::slice::from_mut(&mut value))?;
        Ok(Self { pos, value })
    }
}

fn position<R: Seek>(reader: &mut R) -> u64 {
    reader.stream_position().unwrap()
}

/// The shared geometry used by the absolute-offset tests:
///
/// ```text
/// offset  00 01 02 03 04 05 06 07 08 09 0a 0b 0c 0d 0e 0f
/// bytes   .. .. .. .. 00 05 .. .. .. BB .. .. .. .. EE ..
///                      └ptr(slot)   └base(0x09)         └target(0x0e)
/// ```
///
/// The caller enters the `FilePtr` at position 4, so the pointer slot 4..6 is
/// read as the value 5. A correct implementation seeks to the absolute target
/// `offset + ptr = 9 + 5 = 14` and reads 0xEE. The buggy implementation seeks
/// relative to the post-pointer caller position instead (`6 + offset + ptr =
/// 20`, past the end) and fails with an out-of-bounds read.
const ABS_GEOMETRY: [u8; 16] = [
    0x00, 0x00, 0x00, 0x00, // 0x00: filler
    0x00, 0x05, // 0x04: pointer slot read by the FilePtr, value = 5
    0x00, 0x00, 0x00, // 0x06: filler
    0xBB, // 0x09: base sentinel (must not be read as the value)
    0x00, 0x00, 0x00, 0x00, // 0x0a: filler
    0xEE, // 0x0e: the real pointed-to value
    0x00, // 0x0f: filler after the target
];

/// Regression: with a non-zero caller position *and* a non-zero
/// `FilePtrArgs::offset`, the pointer is applied on top of `offset` as an
/// absolute base, not as an increment from the caller's current position.
#[test]
fn absolute_offset_with_non_zero_caller_position() {
    let mut reader = Cursor::new(ABS_GEOMETRY);

    // Non-zero caller position before the FilePtr is entered.
    reader.seek(SeekFrom::Start(4)).unwrap();

    let result = FilePtr::<u16, Marked>::read_be_args(
        &mut reader,
        FilePtrArgs::builder().offset(9).finalize(),
    )
    .unwrap();

    assert_eq!(result.ptr, 5);
    // The inner parser must observe the absolute target `offset + ptr`.
    assert_eq!(result.value.pos, 14);
    assert_eq!(result.value.value, 0xEE);
    // The caller position must be restored to right after the pointer slot.
    assert_eq!(position(&mut reader), 6);
}

/// Default (`offset = 0`) means an absolute base of zero: the pointer is
/// measured from the start of the stream even when the caller is not there.
#[test]
fn zero_offset_is_absolute_base_zero() {
    // The pointer slot at 4..6 encodes 3, pointing at the absolute target 3.
    let mut data = [0u8; 7];
    data[3] = 0xEE;
    data[4] = 0x00;
    data[5] = 0x03;

    let mut reader = Cursor::new(data);
    reader.seek(SeekFrom::Start(4)).unwrap();

    let result = FilePtr::<u16, Marked>::read_be(&mut reader).unwrap();

    assert_eq!(result.ptr, 3);
    assert_eq!(result.value.pos, 3);
    assert_eq!(result.value.value, 0xEE);
    assert_eq!(position(&mut reader), 6);
}

/// Signed pointers are applied relative to the absolute base, in both
/// directions.
#[test]
fn signed_pointer_positive_and_negative_from_absolute_base() {
    // Positive: pointer +3 read at 2..4, base 8 -> target 11.
    let positive = [
        0x00, 0x00, // filler
        0x00, 0x03, // i16 pointer = +3
        0x00, 0x00, 0x00, 0x00, // filler up to base 8
        0xAA, 0xBB, 0xCC, // base..base+2
        0xEE, // base+3 = 11
    ];
    let mut reader = Cursor::new(positive);
    reader.seek(SeekFrom::Start(2)).unwrap();
    let result = FilePtr::<i16, Marked>::read_be_args(
        &mut reader,
        FilePtrArgs::builder().offset(8).finalize(),
    )
    .unwrap();
    assert_eq!(result.ptr, 3);
    assert_eq!(result.value.pos, 11);
    assert_eq!(result.value.value, 0xEE);
    assert_eq!(position(&mut reader), 4);

    // Negative: pointer -2 read at 2..4, base 8 -> target 6.
    let negative = [
        0x00, 0x00, // filler
        0xFF, 0xFE, // i16 pointer = -2
        0x00, 0x00, // filler
        0xEE, // target 6
        0x00, // 7
        0xBB, // base 8
    ];
    let mut reader = Cursor::new(negative);
    reader.seek(SeekFrom::Start(2)).unwrap();
    let result = FilePtr::<i16, Marked>::read_be_args(
        &mut reader,
        FilePtrArgs::builder().offset(8).finalize(),
    )
    .unwrap();
    assert_eq!(result.ptr, -2);
    assert_eq!(result.value.pos, 6);
    assert_eq!(result.value.value, 0xEE);
    assert_eq!(position(&mut reader), 4);
}

/// A nested `FilePtr` resolves its inner pointer against the nested absolute
/// base while the outer pointer is resolved against the outer base; both
/// layers restore their caller position.
#[test]
fn nested_file_ptr_uses_independent_absolute_bases() {
    // Layout (i16, big-endian):
    //   [0..2]  filler
    //   [2]     outer base marker 0xBB
    //   [4..6]  outer pointer +4 -> outer base 2 + 4 = 6
    //   [6..8]  inner pointer -2 -> inner base 10 - 2 = 8
    //   [8]     nested target 0xEE
    //   [10]    inner base marker 0xCC
    let mut data = [0u8; 12];
    data[2] = 0xBB;
    data[4] = 0x00;
    data[5] = 0x04;
    data[6] = 0xFF;
    data[7] = 0xFE;
    data[8] = 0xEE;
    data[10] = 0xCC;

    let mut reader = Cursor::new(data);
    reader.seek(SeekFrom::Start(4)).unwrap();
    let nested = FilePtr::<i16, FilePtr<i16, Marked>>::read_be_args(
        &mut reader,
        FilePtrArgs::builder()
            .offset(2)
            .inner(FilePtrArgs::builder().offset(10).finalize())
            .finalize(),
    )
    .unwrap();

    assert_eq!(nested.ptr, 4);
    assert_eq!(nested.value.ptr, -2);
    assert_eq!(nested.value.value.pos, 8);
    assert_eq!(nested.value.value.value, 0xEE);
    // The outer layer restores to right after the outer pointer slot.
    assert_eq!(position(&mut reader), 6);
}

#[derive(Debug, PartialEq)]
struct Explosion;

impl core::fmt::Display for Explosion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "boom")
    }
}

impl std::error::Error for Explosion {}

fn exploding_parser<R: Read + Seek>(
    reader: &mut R,
    _endian: Endian,
    _args: (),
) -> binrw::BinResult<u8> {
    // The custom parser still runs at the absolute target position.
    assert_eq!(
        reader.stream_position().unwrap(),
        14,
        "parser must run at the absolute target position"
    );
    Err(Error::Custom {
        pos: 14,
        err: Box::new(Explosion),
    })
}

/// When the inner parser returns an error, the original cause and its precise
/// position survive and the caller position is restored.
#[test]
fn parser_error_preserves_cause_pos_and_restores_reader() {
    let mut reader = Cursor::new(ABS_GEOMETRY);
    reader.seek(SeekFrom::Start(4)).unwrap();

    let parse_with = FilePtr::<u16, u8>::parse_with(exploding_parser);
    let error = parse_with(
        &mut reader,
        Endian::Big,
        FilePtrArgs::builder().offset(9).finalize(),
    )
    .unwrap_err();

    // The original cause is preserved without a wrapping backtrace frame.
    assert_eq!(error.custom_err::<Explosion>(), Some(&Explosion));
    match error {
        Error::Custom { pos, err } => {
            assert_eq!(pos, 14);
            assert!(err.is::<Explosion>());
        }
        other => panic!("expected the original custom error, got {other:?}"),
    }
    // Restore happens on the error path as well.
    assert_eq!(position(&mut reader), 6);
}

fn panicking_parser<R: Read + Seek>(
    reader: &mut R,
    _endian: Endian,
    _args: (),
) -> binrw::BinResult<u8> {
    assert_eq!(reader.stream_position().unwrap(), 14);
    panic!("intentional parser panic");
}

/// Asserts the reader is already back at the caller position while a parser
/// panic is unwinding.
struct PositionCheckOnDrop<'a, R: Seek> {
    reader: &'a mut R,
    expected: u64,
}

impl<R: Seek> Drop for PositionCheckOnDrop<'_, R> {
    fn drop(&mut self) {
        assert_eq!(
            self.reader.stream_position().unwrap(),
            self.expected,
            "reader position must be restored while the parser unwinds"
        );
    }
}

/// If the inner parser panics, the reader is restored before unwinding
/// proceeds and remains at the caller position afterwards.
#[test]
fn reader_position_restored_through_parser_panic() {
    let mut reader = Cursor::new(ABS_GEOMETRY);
    reader.seek(SeekFrom::Start(4)).unwrap();

    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let _guard = PositionCheckOnDrop {
            reader: &mut reader,
            expected: 6,
        };
        let parse_with = FilePtr::<u16, u8>::parse_with(panicking_parser);
        parse_with(
            _guard.reader,
            Endian::Big,
            FilePtrArgs::builder().offset(9).finalize(),
        )
        .unwrap();
    }));

    assert!(outcome.is_err());
    assert_eq!(position(&mut reader), 6);
}
