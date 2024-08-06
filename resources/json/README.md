# Test JSON

This directory contains test JSON files from https://github.com/nst/JSONTestSuite as of commit 984defc.

It only has the positive and questionable JSON inputs, as our JSON parser is extremely forgiving, by design.

## Filtered tests

Some of the questionable tests have been removed:

- `i_structure_UTF-8_BOM_empty_object.json` removed because we don't handle BOMs.
- `i_string_utf16LE_no_BOM.json` removed because we don't speak UTF16.
- `i_string_utf16BE_no_BOM.json` removed because we don't speak UTF16.
- `i_string_UTF-16LE_with_BOM.json` removed because we don't speak UTF16.
