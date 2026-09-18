//! Re-export of the typed-column seam (docs/SCALE.md §6.2).
//!
//! The types live in `devondb-types` so storage decoders (task S3-b) can
//! build typed columns without depending on the executor; executor code
//! imports them from here.

pub use devondb_types::column::{Bitmap, Column};
