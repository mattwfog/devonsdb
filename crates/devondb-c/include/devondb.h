#ifndef DEVONDB_H
#define DEVONDB_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/*
 * Thread-safety: a devondb handle is Send (movable between threads) but
 * NOT Sync-safe for lock-free concurrent access. Internal state is
 * serialized by a mutex, so concurrent misuse of one handle serializes
 * rather than races; the recommended contract remains one handle per
 * thread. All input strings are NUL-terminated UTF-8. JSON strings belong
 * to the caller and must be freed with devondb_string_free. A failed
 * open/create stores NULL in `out`.
 */
typedef struct devondb_database devondb_database;
typedef enum { DEVONDB_OK = 0, DEVONDB_ERR = 1 } devondb_status;

/*
 * Options struct with explicit ABI size for forward growth. Zero-valued
 * fields select library defaults (page_size 4096, memory_limit 64 MiB).
 */
typedef struct {
    size_t size_bytes; /* Must be >= sizeof(devondb_options). */
    uint32_t page_size; /* 0 selects the default. */
    size_t memory_limit; /* 0 selects the default. */
} devondb_options;

devondb_status devondb_open(const char *path, devondb_database **out);
devondb_status devondb_create(const char *path, uint32_t page_size,
                              devondb_database **out);
devondb_status devondb_open_with(const char *path,
                                 const devondb_options *options,
                                 devondb_database **out);
devondb_status devondb_create_with(const char *path,
                                   const devondb_options *options,
                                   devondb_database **out);
devondb_status devondb_execute(devondb_database *db, const char *text);
/*
 * devondb_query_json runs one DevonPlan text query and returns compact
 * natural-value JSON. Non-finite floats (NaN, +/-infinity) are emitted in
 * the tagged form {"f64":"NaN"} / {"f64":"inf"} / {"f64":"-inf"} — both as
 * Float64 scalars and as Vector elements — so they can never collide with
 * a true SQL NULL (JSON null).
 */
devondb_status devondb_query_json(devondb_database *db, const char *text,
                                  char **out_json);

/*
 * Arrow C Data Interface structures, copied spec-verbatim (field order is
 * ABI) from https://arrow.apache.org/docs/format/CDataInterface.html
 * (Apache-2.0). The canonical guard is kept exactly as-is so these
 * definitions coexist with copies from other projects.
 */
#ifndef ARROW_C_DATA_INTERFACE
#define ARROW_C_DATA_INTERFACE

#define ARROW_FLAG_DICTIONARY_ORDERED 1
#define ARROW_FLAG_NULLABLE 2
#define ARROW_FLAG_MAP_KEYS_SORTED 4

struct ArrowSchema {
  /* Array type description */
  const char *format;
  const char *name;
  const char *metadata;
  int64_t flags;
  int64_t n_children;
  struct ArrowSchema **children;
  struct ArrowSchema *dictionary;

  /* Release callback */
  void (*release)(struct ArrowSchema *);
  /* Opaque producer-specific data */
  void *private_data;
};

struct ArrowArray {
  /* Array data description */
  int64_t length;
  int64_t null_count;
  int64_t offset;
  int64_t n_buffers;
  int64_t n_children;
  const void **buffers;
  struct ArrowArray **children;
  struct ArrowArray *dictionary;

  /* Release callback */
  void (*release)(struct ArrowArray *);
  /* Opaque producer-specific data */
  void *private_data;
};

#endif /* ARROW_C_DATA_INTERFACE */

/*
 * devondb_query_arrow runs one DevonPlan text query and exports the result
 * as an Arrow C Data Interface struct array (format "+s") whose children
 * are the result columns. On success the caller owns out_schema/out_array
 * and must call their release callbacks (top-level only; each release
 * releases its children recursively). On failure both are left in the
 * released state (release == NULL), so calling release unconditionally is
 * safe.
 *
 * devondb-to-Arrow type mapping (column type comes from the first non-null
 * value; mixed types in one column are an error naming the column; NULL
 * slots clear the validity bit and hold zero in typed buffers):
 *
 *   Int64                     -> "l"       (int64)
 *   Float64                   -> "g"       (float64)
 *   Bool                      -> "b"       (boolean, bit-packed)
 *   String                    -> "u"       (utf8, int32 offsets; export fails
 *                              past i32::MAX total bytes)
 *   Timestamp (epoch us UTC)  -> "tsu:UTC" (timestamp microseconds, UTC)
 *   Decimal(p, s)             -> "d:p,s"   (decimal128, 16-byte little-endian
 *                              two's-complement digits)
 *   Bytes                     -> "z"       (binary, int32 offsets)
 *   Json                      -> "u" with metadata key "ARROW:extension:name"
 *                              = "arrow.json"
 *   Vector(n)                 -> "+w:n" fixed-size list over child "f"
 *                              (float32); every row has exactly n elements
 *   GeoPoint                  -> "+s" struct with children "lat_deg": "g",
 *                              "lng_deg": "g"
 *   column of only NULLs with unknown type -> "n" (null)
 */
devondb_status devondb_query_arrow(devondb_database *db, const char *text,
                                   struct ArrowSchema *out_schema,
                                   struct ArrowArray *out_array);

devondb_status devondb_schema_json(devondb_database *db, char **out_json);
/* Empty (never NULL) before an error; valid until the next call on `db`. */
const char *devondb_last_error(const devondb_database *db);
void devondb_string_free(char *s);
void devondb_close(devondb_database *db);

#ifdef __cplusplus
}
#endif

#endif
