# Golden-file fixtures

Each `.postcard` file in this tree stores a `(input, output)` pair for
one of pgmcp's pure-algorithm components. Tests load the fixture and
assert that re-running the algorithm against the input still produces
the captured output. This catches silent drift in behaviour that
invariant-style tests (shape, range, monotonicity) can't detect.

## Format

Fixtures are serialised via [`postcard`](https://docs.rs/postcard) —
a compact binary schema-strict encoding that handles f32/f64 losslessly.
Each file is a serialised
[`pgmcp_testing::golden::Golden<I, O>`](../../src/golden.rs) envelope:

```rust
struct Golden<I, O> {
    schema_version: u32,   // bumped on envelope-format changes
    input: I,              // canonical input
    output: O,             // frozen output
    tolerance: Option<f64>,// for float-valued outputs
    generated_at_iso: String,
    source: String,
}
```

The tolerance is used by tests that run floating-point comparisons
(`assert_match_epsilon`). Discrete goldens leave it `None` and compare
via `PartialEq`.

## Regeneration workflow

When an algorithm's output legitimately changes — a bug fix, a
deliberate tightening of a tolerance, a new chunk boundary rule —
regenerate every affected fixture:

```bash
cargo run --release -p pgmcp-testing --bin regen-goldens
```

The binary walks every registered generator, writes the new payload,
and prints one line per fixture (`new`, `unchanged`, or `updated`).
Exit code 2 means at least one fixture changed; CI rejects unstaged
diffs so every change goes through review. Inspect the diff, confirm
it matches your intent, then `git add` and commit.

## Adding a new fixture

1. In `pgmcp-testing/src/bin/regen_goldens.rs`, add a generator
   function that builds `(input, output, tolerance)` and calls
   `regen_golden(name, input, output, tolerance)`.
2. Append `(name, generator_fn)` to the `GENERATORS` registry slice.
3. Run the regen binary — the new fixture is reported as `new`.
4. In `pgmcp-testing/tests/golden_<component>.rs`, add a test that
   calls `assert_match_exact` or `assert_match_epsilon` with the
   same `name`.
5. Commit the generator + test + fixture file together.

## Layout

```
fixtures/golden/
├── chunker/                # chunker::chunk_content boundaries
├── claude_chunker/         # Claude JSONL transcript parsing
├── import_extractor/       # per-language import regex output
├── ctf_idf/                # c-TF-IDF keyword scores
├── fcm/                    # Fuzzy C-Means centroids + memberships
└── merge_toml/             # config.rs merge_toml_values output
```

The `_sanity/` subdirectory is reserved for throwaway fixtures
written by `pgmcp-testing/src/golden.rs`'s own unit tests; they are
cleaned up after each run and should never appear in a commit.
