# Changelog

## Unreleased

### Fixed

- `FilePtr` now treats `FilePtrArgs::offset` as the **absolute base address**
  documented on the field, instead of seeking to `current_position + offset`.
  Previously, when the caller’s reader position was non-zero *and* `offset`
  was non-zero, the pointer was resolved from the wrong base
  (`caller_pos + offset + ptr`), reading the wrong target (or running past the
  end of the stream). The pointer value itself (including signed `i8`–`i128`
  pointers) is still applied on top of that base via
  `IntoSeekFrom::into_seek_from`, and custom `parse_with` parsers are still
  invoked at the resolved position.

## Structure of the fix

The shared root cause lived in a single private entry point used by every
`FilePtr` read path (`FilePtr::read_options`/`BinRead`, `FilePtr::parse` and
`FilePtr::with`/`parse_with`):

- `binrw/src/file_ptr.rs` — `FilePtr::read_value`:
  - The caller position is saved **before** any movement
    (`reader.stream_position()`).
  - The reader now seeks to `SeekFrom::Start(args.offset)` (absolute base)
    instead of `SeekFrom::Current(args.offset as i64)`, then the pointer is
    applied with the unchanged `ptr.into_seek_from()`.
  - A private `RestorePosition` drop guard rewinds to the saved position on
    every exit path, including while a custom parser is unwinding from a
    panic. A normal return performs an explicit rewind and disarms the guard
    so the guard never seeks twice.
  - Error handling follows the existing `__private` abstractions rather than
    a parallel implementation: a failure while reaching the target goes
    through `__private::restore_position`, and a simultaneous parser +
    rewind failure is combined with the newly exposed
    `__private::restore_position_err` (in `binrw/src/private.rs`), keeping
    the original parser error as the root cause with the rewind error and
    context attached.
- No special-casing of literal sample data is present; all `FilePtr`
  constructors and pointer integer types flow through the same code.

## Previous coverage gap

The existing tests and doctests only exercised `FilePtr` with the default
`offset = 0` and with the reader at position zero when the `FilePtr` began,
so `SeekFrom::Current(offset)` and `SeekFrom::Start(offset)` were
observationally identical. The “absolute base” contract of the non-zero
`offset` argument (as documented on `FilePtrArgs::offset` and used by the
derive `offset(...)` directive) was therefore never asserted. Five
`file_ptr` doctests in the snapshot failed for the same underlying reason
once a pointer followed already-consumed bytes; they now pass without
modification.

## Most dangerous counterexample and its targeted test

The easiest assumption to miss is *“the caller is always at position zero
when `FilePtr` starts”*: with caller position 4, `offset = 9` and a `u16`
pointer of `5`, the buggy seek lands at `6 + 9 + 5 = 20` (past EOF) instead
of the absolute target `9 + 5 = 14`.

The regression tests live in `binrw/tests/file_ptr.rs`; each test asserts
observable intermediate state (the exact position passed to the inner
parser and the reader position after the call), not only the returned
value:

- `absolute_offset_with_non_zero_caller_position` — non-zero caller
  position + non-zero `offset`: the inner parser observes absolute
  `offset + ptr = 14`, reads the sentinel `0xEE`, and the reader is restored
  to position 6. This test fails (EOF) against the old
  `SeekFrom::Current(offset)` implementation.
- `zero_offset_is_absolute_base_zero` — default `offset = 0` with a
  non-zero caller position still resolves the pointer from the start of the
  stream (absolute base zero).
- `signed_pointer_positive_and_negative_from_absolute_base` — `i16`
  pointers `+3` and `-2` are both applied on top of the absolute base;
  covers the signed-pointer branch of `into_seek_from`.
- `nested_file_ptr_uses_independent_absolute_bases` — an outer
  `FilePtr<i16, FilePtr<i16, _>>` resolves each layer against its own
  absolute base (`2` and `10`) and the outer layer restores the caller
  position.
- `parser_error_preserves_cause_pos_and_restores_reader` — a custom
  `parse_with` parser returning `Error::Custom`: the original cause
  (`custom_err::<Explosion>()`), its precise `pos = 14` and the variant are
  preserved (no wrapping backtrace frame), and the reader is at position 6
  after the error.
- `reader_position_restored_through_parser_panic` — a panicking custom
  parser: a drop guard observes the reader already back at position 6
  *while unwinding*, and the position is still 6 after `catch_unwind`.
  Against the old implementation the guard’s assertion fired during unwind
  and aborted the test process.

## Deliberately preserved public behaviour

- `FilePtrArgs::offset` defaults to `0`; with `offset = 0` the pointer is
  measured from the start of the stream, matching the documented contract.
- Signed and unsigned pointer integer types (`i8`–`i128`, `u8`–`u128`, and
  their `NonZero*` aliases) keep their existing `IntoSeekFrom` semantics and
  out-of-range error.
- `FilePtr::parse`, `FilePtr::with`/`parse_with` and the derived
  `offset(...)` directive keep their signatures and continue to share the
  same internal read path.
- After any `FilePtr` read — success, parser error, target-seek error, or
  parser panic — the outer reader ends at the saved caller position (right
  after the pointer slot). Parser errors remain the root cause, with rewind
  errors added as context rather than replacing them.
