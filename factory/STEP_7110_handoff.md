# Step 7110 (Error umbrella collapse) — handoff

This branch implements repository-area consolidation on the umbrella [`Error`](grit-lib/src/error.rs):

- New `#[non_exhaustive]` [`RepositoryError`](grit-lib/src/error.rs) holds discovery/layout/format failures formerly as top-level `Error` variants.
- Umbrella uses `Repository(#[from] RepositoryError)`; [`Error::kind_name`](grit-lib/src/error.rs) delegates to [`RepositoryError::kind_name`](grit-lib/src/error.rs) (stable slugs unchanged).
- Legacy `Error::Message(String)` is `Error::Verbatim(String)`; serialized `kind` slug remains **`message`** (stable); [`Error::area`](grit-lib/src/error.rs) is additive.
- [`IndexError::Nested`](grit-lib/src/index/error.rs) removed: write-tree maps ODB failures into typed corrupt reasons instead of wrapping the umbrella [`Error`](grit-lib/src/error.rs).

Call sites, CLI error mapping, contract tests, [`error_contract`](grit-lib/tests/error_contract.rs) expectations, docs ([errors](content/docs/library/errors.md), [embedding](content/docs/library/embedding.md)), and api-ratchet (`Error::Verbatim(` site count; catch-all set includes `Verbatim`) were updated. Regenerate [`public-api.txt`](grit-lib/public-api.txt) and [`api-ratchet.toml`](grit-lib/api-ratchet.toml) with `make public-api` and `make api-ratchet` after merging.

Further area collapses (config, object, ref, …) remain for later steps.
