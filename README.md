# sqlparse-rs

Rust-accelerated lexer and statement splitter for [sqlparse](https://github.com/andialbrecht/sqlparse).
Output is identical to the original; anything the Rust core does not handle is passed to the
original implementation.

```python
import sqlparse, sqlparse_rs
sqlparse_rs.patch()            # sqlparse.split / parse / format now use the Rust lexer
sqlparse.split(script)         # ~50-70x faster
sqlparse_rs.split_many(scripts)  # parallel, GIL released
sqlparse_rs.unpatch()
```

## Speed

sqlparse 0.6.1, Python 3.13, 2 cores, 467 KB of generated queries plus sqlparse's own sample files
(`bench/bench.py`):

| operation | original | Rust | speedup |
|---|---:|---:|---:|
| `lexer.tokenize` (467 KB script) | 702 ms | 39 ms | 18x |
| `split` (467 KB script) | 1038 ms | 15 ms | 69x |
| `split` each of 1500 queries | 1118 ms | 22 ms | 50x |
| `split_many` (6000 queries, parallel) | 4240 ms | 50 ms | 86x |
| `parse` x500 | 1122 ms | 848 ms | 1.3x |
| `format(reindent)` x500 | 1557 ms | 1144 ms | 1.4x |

`parse` and `format` gain less because most of their time is spent in sqlparse's grouping pass
and token-object construction, which stay in Python. Porting grouping is the obvious next step.

## What is ported

* The whole `SQL_REGEX` rule list, as hand-written matchers that reproduce the backtracking order
  of the original patterns (including quirks such as `12abc` lexing as `1` + `2abc`).
* Dollar-quoted literals and multiline comments (`find_delimited_spans`).
* Keyword lookup, built from the live `Lexer` dictionaries.
* `StatementSplitter` (BEGIN/END, CASE, IF, LOOP, DECLARE blocks, GO, ...) for `split()`.

## Fallbacks (results stay identical)

Bytes input, lone surrogates, customised lexers (`set_SQL_REGEX`), non-ASCII words that need
Python's `str.upper()`, and non-ASCII text when Python's Unicode database differs from the one the
character tables were generated from (`tools/gen_tables.py`). Keywords added via `add_keywords` are
picked up automatically.

## Testing

* `tests/diff_test.py`: compares token streams (same token-type objects, same values) and
  `split()` with and without `strip_semicolon` against the original on sample files and fuzzed
  input; about 2M cases passed against sqlparse 0.6.1.
* sqlparse's own test suite passes unchanged with `sqlparse_rs.patch()` applied.
* `tests/test_basic.py`: unit tests.

## License

BSD 3-Clause, same as sqlparse; see `LICENSE`.
