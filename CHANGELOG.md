# Changelog

## Unreleased

### Fixed

- `FilePtr` now treats `FilePtrArgs.offset` as the absolute base position it
  is documented to be. Previously the base was applied with
  `SeekFrom::Current(offset)`, so when the caller's reader position was
  non-zero the pointer was resolved from `caller_position + offset` instead
  of from `offset`. Signed (`Ptr = i8`/`i16`/…) and custom-parser
  (`FilePtr::parse_with` / `FilePtr::with`) paths share the same fix because
  they all funnel through the single private `FilePtr::read_value` routine.
- The caller's reader position is now restored even if the pointed-to parser
  panics, via a `RestorePosition` drop guard in `read_value`; the explicit
  restore on the normal (success/`Err`) path is unchanged, so its error
  semantics are preserved.

### Structure choice

- The fix lives entirely in `binrw/src/file_ptr.rs` inside the existing
  `FilePtr::read_value` helper used by the `BinRead` impl, `FilePtr::parse`,
  `FilePtr::parse_with`, and `FilePtr::with`. No parallel implementation was
  introduced, so signed pointers, non-zero pointer types, and custom parsers
  keep their existing behaviour.
- The only added code is the change from `SeekFrom::Current(args.offset)` to
  `SeekFrom::Start(args.offset)` (also removing the lossy `offset as i64`
  cast) and a small private drop guard used solely for panic recovery.

### Previous coverage gap

- No test exercised `FilePtr` with a non-zero caller position together with a
  non-zero `offset`. The documentation and `FilePtrArgs::offset` both promise
  an absolute base, and the existing derive test
  `r#struct::all_the_things` (whose pointer table is read at position 12 but
  whose entries live at absolute file offsets) only passes under absolute
  semantics; it was failing before this fix. There was also no coverage for
  reader restoration on a parser panic, where the previous post-parser seek
  never ran at all.

### Most dangerous counterexample

- The failure that is easiest to mis-fix is "both positions non-zero". When
  the caller sits at position 5 and `offset = 10` with a pointer value of 4,
  the correct target is `10 + 4 = 14`, while the buggy code seeks to
  `5 + 10 + 4 = 19`. Asserting only the returned value cannot distinguish an
  implementation that forgets to restore the reader, so every regression
  test also asserts the final outer reader position.

### Regression tests (`binrw/tests/file_ptr.rs`)

- `nonzero_offset_is_absolute_when_caller_position_nonzero`: caller position
  5, `offset = 10`, pointer 4 → reads byte `0xAB` at absolute position 14
  and leaves the reader at 5. Failed with "failed to fill whole buffer" at
  position 19 before the fix.
- `signed_pointer_relative_to_absolute_offset`: `FilePtr<i8, _>` with
  pointer `-2` and base 11 reads from 9; the buggy relative seek computes
  `11 + 5 - 2 = 14`, past the data. Pins signed-pointer semantics.
- `nested_file_ptr_uses_absolute_offset`: a `FilePtr<u16, FilePtr<u16, u8>>`
  field read at caller position 6 with `#[br(offset = 8)]`; asserts the
  outer pointer, inner pointer (`13`, resolved from the inner FilePtr's
  default absolute base 0), dereferenced value, and outer position 6.
- `custom_parser_error_keeps_cause_and_restores_position`: a
  `parse_with` parser returning `Error::Custom` verifies the original cause
  survives (`custom_err::<FailAt>()`), its position is the exact target
  position (14), and the reader is restored to 5 on the `Err` path.
- `reader_position_restored_when_parser_panics`: a panicking
  `parse_with` parser observed through `catch_unwind`; asserts the panic
  payload reports the correct target (`0xe`, not `0x13`) and that the reader
  is back at position 5 while unwinding — this is the drop-guard invariant.
- `zero_offset_is_absolute_file_start`: default `offset = 0` keeps pointers
  measured from the start of the stream regardless of caller position
  (returned value 4, not 9).

### Public behaviour deliberately preserved

- `offset = 0` semantics: pointers count from the start of the stream (this
  matches the documented "absolute offset" promise and the existing
  `all_the_things` data).
- `FilePtr::ptr` / `FilePtr::value` fields, the `Deref`/`DerefMut` impls, and
  the `FilePtr8`/`FilePtr16`/…/`NonZero*` aliases are unchanged.
- Error shape and context: parser errors bubble up untouched, and an explicit
  restore seek after the parser keeps returning the restore seek error on
  the `Err` path exactly as before; the new drop guard only handles panics.
- `parse_from_iter` / `parse_from_iter_with` are intentionally untouched:
  their offsets are defined relative to the stream position when collection
  parsing begins, not via `FilePtrArgs.offset`.
