# DevonGrid — devondb geospatial profile 0 (frozen)

**Status: frozen with `format_version` 1.** The `GeoPoint` catalog spelling,
column payload, `GEO_COLUMNS` feature bit, and profile-0 cell-assignment
semantics are part of the permanent v1 compatibility promise. Changes follow
the major-release and in-place-upgrade rules in `docs/FORMAT.md`; the committed
geo anchor mechanically enforces the existing spellings.

The runtime is implemented in-tree, with `h3o` used only as a development
oracle. Read with `docs/FORMAT.md` (catalog + column encodings),
`docs/PLAN_IR.md` (future geo operators), `docs/ONTOLOGY.md` § 2 (the
`Locatable` interface geo grounds).

## 1. What DevonGrid is

A discrete global grid for indexing `GeoPoint` data: every point on Earth
maps to a 64-bit **cell index** at each of 16 resolutions (0 = coarsest,
15 = finest). devondb uses it so that geospatial predicates ("within 2
miles of X") compile to contiguous u64 ranges over the engine's ordered
indexes — no dedicated spatial access method, deterministic, pinnable.

The property that motivates DevonGrid over stock H3 is **congruence**: a
parent cell's region is *exactly* the union of its children's regions.
Stock H3 is non-congruent by construction (hexagons cannot tile into
hexagons; children overhang their parents, and point→cell assignment at
adjacent resolutions can disagree near boundaries). DevonGrid keeps H3's
lattice, projection, and 64-bit index spelling — and repairs the hierarchy
by definition:

- **Atom.** The res-15 cell of a point, computed by H3-compatible
  assignment at res 15 exactly. Atoms are the only cells points are ever
  assigned to directly. (H3 res-15 hexagons average ~0.9 m²; that is the
  profile-0 positional granularity of cell indexing. Exact coordinates are stored
  in the `GeoPoint` value itself — the atom bounds index precision, not
  data precision.)
- **Cell of a point at res r.** `cell(p, r) := truncate(atom(p), r)` —
  digit truncation only, never re-assignment. Hence for all p and r:
  `parent(cell(p, r)) == cell(p, r-1)`. Congruent by construction over
  the universe of indexed points.
- **Region of a cell.** The union of its atoms' res-15 hexagons. At coarse
  resolutions this converges toward the Gosper-island limit shape of
  aperture-7 subdivision: hexagon-like with a crinkled edge. Rendering
  uses the finite atom-union boundary (or coarser approximations); no
  fractal math exists anywhere in the engine.

**The crinkle-band caveat (binding, must be documented at every user
surface):** for points near cell boundaries, `cell(p, r)` may differ from
stock H3's `latLngToCell(p, r)` — DevonGrid picks the answer consistent
across resolutions; stock H3 at res r is the answer consistent with
nothing (it disagrees with its own res r±1 assignments in the same band).
Across 32,768 uniform (point, res 0..=15) samples against
the oracle, **5.91%** of pairs fall in the band, and every divergence
names an edge-neighbor of the direct cell — the band is real, bounded,
and strictly adjacent.
`h3_compat_cell(p, r)` (direct res-r assignment, bit-identical to the
oracle) is provided for interop verification and migration checks; it is
non-congruent and never used for indexing.

## 2. Profiles

A DevonGrid **profile** fixes (lattice, projection, atom depth). Profile 0 pins:

- **Profile 0 — H3-compatible (the only frozen profile):** H3's icosahedral
  lattice, base-cell layout, aperture-7 digit system, and gnomonic
  face projection; atom depth 15. Cell indexes ARE H3 indexes
  (`to_h3` on any DevonGrid cell is the identity), so the entire H3
  ecosystem (h3-pg, h3 python, h3o rendering) consumes them directly.
- **Format v1 is profile 0.** It has no stored profile field: the v1 freeze
  itself is the source of truth. A future profile is a feature-bit format
  extension that carries its profile ID under `FORMAT.md`'s registry law.
  Until then, the geo crate's explicit `Profile` type is the plumbing seam;
  format-v1 shorthands name their `FORMAT_V1_PROFILE` pin explicitly.

## 3. Cell index format (u64, profile 0)

Little-endian u64, identical to the H3 cell-mode index spelling:

| Bits | Width | Field | Profile 0 rule |
|------|-------|-------|---------|
| 63 | 1 | reserved | 0 |
| 59–62 | 4 | mode | 1 (cell) |
| 56–58 | 3 | mode-dependent | 0 in cell mode |
| 52–55 | 4 | resolution | 0–15 |
| 45–51 | 7 | base cell | 0–121 |
| 0–44 | 45 | digits 1–15, 3 bits each | digit *i* at bits (45−3*i*)…(47−3*i*); value 0–6 for *i* ≤ res, 7 for *i* > res |

Validity (all enforced on any raw-u64 ingestion, staged-rejection style):
reserved bit clear; mode == 1; mode-dependent == 0; res ≤ 15; base cell
≤ 121; digits ≤ 6 up to res and exactly 7 beyond; pentagon base cells
(the 12-entry set `[4, 14, 24, 38, 49, 58, 63, 72, 83, 97, 107, 117]`,
frozen as a const and locked by oracle test) forbid digit value 1 in the
first non-CENTER position — leading 0 (center-child) digits are skipped,
and the first digit that is not 0 must not be 1 (the deleted K axis).
The first active digit that is not 0 determines the deleted-axis check;
using the first non-7 digit would wrongly reject active digits such as
`[0, 1]` under a rule the oracle does not enforce.

**Oracle-normative clause.** The constant VALUES of this profile — the
table above, the pentagon set, base-cell numbering, digit geometry — are
normative from the H3 reference as exposed by the pinned `h3o`
dev-dependency, not from this document's prose. If an oracle test
contradicts this file, the oracle governs the profile constants. This clause
covers profile-0 H3 constants only; DevonGrid semantics (atoms,
truncation, ranges, canonical forms) are normative HERE.

### Atom keys and descendant ranges

An **atom key** is the res-15 cell index verbatim (mode/res bits
included) — u64-sortable, and identical to the H3 res-15 index.

For a cell C at res r, its descendant atoms occupy exactly the interval
`[lo, hi]` where `lo` = C with resolution field set to 15 and digits
r+1…15 set to 0, and `hi` = the same with those digits set to 6. Values
inside the interval that are not valid atom keys (any digit 7, wrong
mode) never occur as stored keys, so an ordered-index range scan over
`[lo, hi]` returns exactly C's atoms. Pentagon-descended cells skip the
deleted-axis digit; their atoms are a subset of the interval — the scan
stays exact. This interval derivation is what makes every geo predicate a
range scan; it must be oracle-tested against `cell_to_children` extremes.

## 4. Determinism policy (binding for every assignment path)

Cell assignment must produce identical bits on every platform, forever —
pinned plans and the golden corpus depend on it.

- **No platform libm on any assignment path.** `sin`/`cos`/`asin`/`acos`/
  `atan2` come from devondb-geo's own vendored, musl-derived pure-Rust
  kernels (provenance comments mandatory; MIT-compatible; every ported
  item's license is reproduced in `crates/devondb-geo/THIRD_PARTY_LICENSES.md`). `sqrt` may use
  the hardware/std intrinsic ONLY on the non-negative domain, where IEEE
  correct rounding makes it bit-deterministic; negative/NaN inputs return
  the canonical quiet NaN in our code, never the intrinsic's (NaN sign and
  payload are platform-defined there — x86 "indefinite" vs ARM default
  NaN). Bit-pattern golden tests lock every kernel.
- **h3o is a dev-dependency ONLY** — the verification oracle, never a
  runtime dependency, never outside `#[cfg(test)]`/`tests/`. The runtime
  geo surface is 100% this repo's code.
- **Golden corpus.** A committed corpus of (lat, lng, res) → cell-index
  triples, generated once against the oracle and frozen; CI reproduces it
  bit-exactly on every platform from our implementation alone. Grows,
  never changes — FORMAT.md golden rules apply.
- **Oracle-comparison tolerances are physical, never ulp.** Assignment
  (integer cell ids) is compared bit-exact — measured exact over 20k
  points. Float outputs (cell centers) are compared to the
  oracle in ABSOLUTE degrees (1e-11° ≈ 1 μm), because ulp-of-degrees is
  magnitude-skewed and two correct trig implementations legitimately
  diverge ~1 ulp, amplified by spherical cancellation (measured: max
  279 ulp ≈ 8e-12°). Bit-exactness of OUR outputs
  across platforms is the goldens' job and carries no tolerance.
- Deterministic tests only: pinned seeds, no wall-clock, no ambient
  randomness.

## 5. The `GeoPoint` value type

`LogicalType::GeoPoint` / `Value::GeoPoint` is a WGS84 coordinate pair in
degrees, represented by `lat_deg`/`lng_deg` f64 values. The type is handled
consistently across execution, storage, server, and CLI surfaces.

Canonical form (enforced at construction AND on every decode path;
non-canonical input is rejected on decode, normalized only by the
convenience constructor):

- both finite; `lat_deg ∈ [-90, 90]`; `lng_deg ∈ [-180, 180)` — +180
  spells as −180;
- at the poles (`lat_deg == ±90`), `lng_deg` must be 0.

Spellings (canonical, tested like the VectorEncoded spellings):
serde `{"GeoPoint":{"lat_deg":…,"lng_deg":…}}` with unknown/missing
fields rejected; plan-JSON natural literal `{"geo":{…}}` (PLAN_IR.md
§ Literals); text literal `geo(<lat>, <lng>)` and text type name
`GeoPoint` (the parser normalizes only the two canonical foldings);
Display `geo(<lat>, <lng>)` using Rust's default f64 formatting; catalog
`ty` spelling `"GeoPoint"`. Budget policy: approximately 24 bytes (MVCC
§7.2 policy, not format). GeoPoint node columns work end to end through text
DDL, inserts, WAL recovery, checkpointed node groups through the geo codec,
and feature bit 4
`GEO_COLUMNS` derived at catalog save (FORMAT § Feature flag registry;
the first supported-but-not-read-safe bit). Relationship properties
reject the type (no CSR geo layout, like VectorEncoded). Comparisons and sorts
reject it because GeoPoint is not scalar-comparable.

Storage column encoding: per row 16 bytes —
`lat_deg` f64 LE then `lng_deg` f64 LE; nulls via the existing
column-null machinery; bytes must round-trip exactly (no re-canonicalization
on read).

## 6. Engine consumption: `WithinScan`

The `WithinScan` plan operator (`docs/PLAN_IR.md` § Query operators;
result law in its "within semantics" block) is the first engine consumer
of the covering compiler. This section is binding for the executor.

### Execution law (Profile 0)

`WithinScan { table, column, center, meters }` compiles
`cover_disc(center.lat, center.lng, meters, res)` once per execution
(`devondb-geo` `covering.rs`) and then scans the table's GeoPoint column:

1. For each non-NULL stored point, compute its atom key (resolution-15
   cell index — the same assignment path §4 pins bit-for-bit).
2. Atom key inside a `DiscCovering.full` range (binary search over the
   sorted, merged, inclusive ranges) → **emit with no distance
   evaluation**. The covering contract guarantees full-range membership
   implies containment.
3. Otherwise truncate the atom key to the covering's target resolution;
   if that cell is in `DiscCovering.boundary` (binary search) →
   **re-check** with `great_circle_meters(point, center) <= meters` and
   emit on true.
4. Otherwise skip the row. NULL never matches.

Profile 0 is a full column scan that replaces per-row trigonometry with integer
range membership for every interior hit. Zone maps (`docs/SCALE.md` §3) can
let step 1 skip whole row groups whose
atom-key min/max misses every range — the same covering, one more
pruning layer.

### Equivalence requirement

The accelerated path MUST be row-for-row identical — same rows, same
order (ScanNodes order) — to the naive form
(`nodes(T) | filter great_circle_meters(c, center) <= meters` evaluated
per row). An end-to-end test runs both paths over a fixture
spanning: interior points, points within ~1 cell width of the disc edge
on both sides, a center near an icosahedral vertex, a pole, an
antimeridian-crossing disc, and radii at the resolution table's
boundaries. Any divergence is a bug in the covering consumer, never
tolerable slack — `cover_disc`'s conservative boundary makes exactness
achievable, so exactness is the contract.

### Resolution choice (deterministic, non-semantic)

Any target resolution in [0,15] yields correct results under the
equivalence law — res affects work, never output. The executor MUST
derive res deterministically from `meters` alone (no adaptivity to data,
load, or platform), so identical plans do identical work everywhere. The
default rule is derived from the covering module's edge-scale table. Changing
it later is legal and invisible to results, pins included.

### NL and UI intents (bound at their own seams)

- NL (`docs/NL.md` §12): "within N <unit> of <place>" — units convert by
  pinned exact constants (mi = 1609.344 m, km = 1000 m, ft = 0.3048 m);
  a named place grounds per the grounding law to a unique entity with
  exactly one GeoPoint column, and its location is resolved AT COMPILE
  TIME into a literal `center` in the emitted plan (the plan the user
  confirms and may pin shows the coordinates — a later move of the
  entity never silently changes a pinned plan).
- UI (`docs/UI.md` § Map view): map rendering of GeoPoint columns and
  within-result overlays.
