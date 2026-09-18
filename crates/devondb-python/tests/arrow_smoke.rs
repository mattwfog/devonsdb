//! Smoke test for the Arrow PyCapsule Interface on `devondb.QueryResult`:
//! spawns python3 (SKIPs when absent, like `python_smoke.rs`). The Python
//! script uses pyarrow when it is importable and otherwise walks the
//! capsules via ctypes, so the test never silently passes.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

const PYTHON_SCRIPT: &str = r#"
import ctypes
import gc
import os

import devondb

path = os.environ["DEVONDB_SMOKE_PATH"]
with devondb.Database.create(path, page_size=4096, memory_limit=8 * 1024 * 1024) as db:
    db.execute("create node table Scalar (id Int64 primary key, flag Bool, num Int64, score Float64, name String, embedding Vector(3), place GeoPoint, happened Timestamp, payload Bytes, amount Decimal(10, 2), document Json, nothing String)")
    db.execute(r'insert into Scalar values (1, true, -42, 1.5, "devon", [1.0, 2.0, 3.0], geo(45.5, -122.625), timestamp("2024-08-09T00:00:00.123456Z"), bytes("00ff1a"), decimal("12345678.90"), json("{\"k\":1}"), null), (2, null, null, null, null, null, null, null, null, null, null, null)')
    result = db.query("nodes(Scalar) as s | sort s.id | project s.flag as flag, s.num as num, s.score as score, s.name as name, s.embedding as embedding, s.place as place, s.happened as happened, s.payload as payload, s.amount as amount, s.document as document, s.nothing as nothing")

pythonapi = ctypes.pythonapi
pythonapi.PyCapsule_GetPointer.restype = ctypes.c_void_p
pythonapi.PyCapsule_GetPointer.argtypes = [ctypes.py_object, ctypes.c_char_p]
pythonapi.PyCapsule_GetName.restype = ctypes.c_char_p
pythonapi.PyCapsule_GetName.argtypes = [ctypes.py_object]


class ArrowSchema(ctypes.Structure):
    pass


class ArrowArray(ctypes.Structure):
    pass


SCHEMA_RELEASE = ctypes.CFUNCTYPE(None, ctypes.c_void_p)
ARRAY_RELEASE = ctypes.CFUNCTYPE(None, ctypes.c_void_p)

ArrowSchema._fields_ = [
    ("format", ctypes.c_char_p),
    ("name", ctypes.c_char_p),
    ("metadata", ctypes.c_void_p),
    ("flags", ctypes.c_int64),
    ("n_children", ctypes.c_int64),
    ("children", ctypes.POINTER(ctypes.POINTER(ArrowSchema))),
    ("dictionary", ctypes.POINTER(ArrowSchema)),
    ("release", ctypes.c_void_p),
    ("private_data", ctypes.c_void_p),
]
ArrowArray._fields_ = [
    ("length", ctypes.c_int64),
    ("null_count", ctypes.c_int64),
    ("offset", ctypes.c_int64),
    ("n_buffers", ctypes.c_int64),
    ("n_children", ctypes.c_int64),
    ("buffers", ctypes.POINTER(ctypes.c_void_p)),
    ("children", ctypes.POINTER(ctypes.POINTER(ArrowArray))),
    ("dictionary", ctypes.POINTER(ArrowArray)),
    ("release", ctypes.c_void_p),
    ("private_data", ctypes.c_void_p),
]


def capsule_pointer(capsule, name):
    assert pythonapi.PyCapsule_GetName(capsule) == name
    pointer = pythonapi.PyCapsule_GetPointer(capsule, name)
    assert pointer
    return pointer


def words(array, index):
    return ctypes.cast(array.buffers[index], ctypes.POINTER(ctypes.c_uint64))


def i64s(array, index):
    return ctypes.cast(array.buffers[index], ctypes.POINTER(ctypes.c_int64))


def f64s(array, index):
    return ctypes.cast(array.buffers[index], ctypes.POINTER(ctypes.c_double))


def offsets(array):
    return ctypes.cast(array.buffers[1], ctypes.POINTER(ctypes.c_int32))


def metadata_entries(schema):
    raw = ctypes.string_at(schema.metadata, 4 + 4 + 20 + 4 + 10)
    count = int.from_bytes(raw[0:4], "little")
    assert count == 1
    key_length = int.from_bytes(raw[4:8], "little")
    key = raw[8 : 8 + key_length].decode()
    cursor = 8 + key_length
    value_length = int.from_bytes(raw[cursor : cursor + 4], "little")
    value = raw[cursor + 4 : cursor + 4 + value_length].decode()
    return [(key, value)]


def release_schema_tree(pointer):
    node = pointer.contents
    children = [node.children[i] for i in range(node.n_children)]
    for child in children:
        release_schema_tree(child)
    assert node.release
    SCHEMA_RELEASE(node.release)(ctypes.addressof(node))
    # The node's struct is still alive here: a node's struct allocation is
    # freed by its parent's release, so this check never reads freed memory.
    assert not pointer.contents.release
    assert not pointer.contents.private_data


def release_array_tree(pointer):
    node = pointer.contents
    children = [node.children[i] for i in range(node.n_children)]
    for child in children:
        release_array_tree(child)
    assert node.release
    ARRAY_RELEASE(node.release)(ctypes.addressof(node))
    assert not pointer.contents.release
    assert not pointer.contents.private_data


EXPECTED = [
    ("flag", b"b"),
    ("num", b"l"),
    ("score", b"g"),
    ("name", b"u"),
    ("embedding", b"+w:3"),
    ("place", b"+s"),
    ("happened", b"tsu:UTC"),
    ("payload", b"z"),
    ("amount", b"d:10,2"),
    ("document", b"u"),
    ("nothing", b"n"),
]


def walk(schema, array):
    assert schema.format == b"+s" and schema.name is None
    assert schema.flags == 0 and schema.n_children == 11
    assert not schema.dictionary
    for index, (name, fmt) in enumerate(EXPECTED):
        child = schema.children[index].contents
        assert child.name.decode() == name, (index, child.name)
        assert child.format == fmt, (name, child.format)
        assert child.flags == 2, name
        if name != "document":
            assert not child.metadata, name
    assert metadata_entries(schema.children[9].contents) == [
        ("ARROW:extension:name", "arrow.json")
    ]
    embedding = schema.children[4].contents
    assert embedding.n_children == 1
    item = embedding.children[0].contents
    assert (item.format, item.name, item.flags) == (b"f", b"item", 0)
    place = schema.children[5].contents
    assert place.n_children == 2
    lat, lng = place.children[0].contents, place.children[1].contents
    assert (lat.format, lat.name) == (b"g", b"lat_deg")
    assert (lng.format, lng.name) == (b"g", b"lng_deg")

    assert array.length == 2 and array.null_count == 0
    assert array.n_buffers == 1 and not array.buffers[0]
    assert array.n_children == 11

    flag = array.children[0].contents
    assert flag.null_count == 1 and flag.n_buffers == 2
    assert words(flag, 0)[0] == 0b01 and words(flag, 1)[0] == 0b01

    num = array.children[1].contents
    assert words(num, 0)[0] == 0b01
    assert [i64s(num, 1)[0], i64s(num, 1)[1]] == [-42, 0]

    score = array.children[2].contents
    assert [f64s(score, 1)[0], f64s(score, 1)[1]] == [1.5, 0.0]

    name = array.children[3].contents
    assert name.n_buffers == 3
    assert [offsets(name)[i] for i in range(3)] == [0, 5, 5]
    assert ctypes.string_at(name.buffers[2], 5) == b"devon"

    embedding_array = array.children[4].contents
    assert embedding_array.n_buffers == 1
    assert words(embedding_array, 0)[0] == 0b01
    item_array = embedding_array.children[0].contents
    assert item_array.length == 6 and item_array.null_count == 0
    elements = ctypes.cast(item_array.buffers[1], ctypes.POINTER(ctypes.c_float))
    assert [elements[i] for i in range(6)] == [1.0, 2.0, 3.0, 0.0, 0.0, 0.0]

    place_array = array.children[5].contents
    assert words(place_array, 0)[0] == 0b01
    lat_array, lng_array = place_array.children[0].contents, place_array.children[1].contents
    assert [f64s(lat_array, 1)[0], f64s(lat_array, 1)[1]] == [45.5, 0.0]
    assert [f64s(lng_array, 1)[0], f64s(lng_array, 1)[1]] == [-122.625, 0.0]

    happened = array.children[6].contents
    assert [i64s(happened, 1)[0], i64s(happened, 1)[1]] == [1723161600123456, 0]

    payload = array.children[7].contents
    assert [offsets(payload)[i] for i in range(3)] == [0, 3, 3]
    assert ctypes.string_at(payload.buffers[2], 3) == b"\x00\xff\x1a"

    amount = array.children[8].contents
    assert amount.n_buffers == 2
    digits = words(amount, 1)
    assert (digits[0], digits[1], digits[2], digits[3]) == (1234567890, 0, 0, 0)

    document = array.children[9].contents
    assert [offsets(document)[i] for i in range(3)] == [0, 7, 7]
    assert ctypes.string_at(document.buffers[2], 7) == b'{"k":1}'

    nothing = array.children[10].contents
    assert nothing.n_buffers == 0 and not nothing.buffers
    assert nothing.length == 2 and nothing.null_count == 2
    assert nothing.n_children == 0


try:
    import pyarrow as pa
except ImportError:
    pa = None

if pa is not None:
    import datetime
    import decimal

    batch = pa.record_batch(result)
    try:
        table = pa.table(result)
    except Exception:
        table = pa.Table.from_batches([batch])
    assert table.num_rows == 2
    schema = batch.schema
    assert schema.field("flag").type == pa.bool_()
    assert schema.field("num").type == pa.int64()
    assert schema.field("score").type == pa.float64()
    assert schema.field("name").type == pa.string()
    assert schema.field("embedding").type == pa.list_(pa.float32(), 3)
    assert schema.field("place").type == pa.struct(
        [
            pa.field("lat_deg", pa.float64(), nullable=False),
            pa.field("lng_deg", pa.float64(), nullable=False),
        ]
    )
    assert schema.field("happened").type == pa.timestamp("us", tz="UTC")
    assert schema.field("payload").type == pa.binary()
    assert schema.field("amount").type == pa.decimal128(10, 2)
    assert schema.field("document").type == pa.string()
    assert schema.field("document").metadata == {b"ARROW:extension:name": b"arrow.json"}
    assert schema.field("nothing").type == pa.null()
    assert batch.column("flag").to_pylist() == [True, None]
    assert batch.column("num").to_pylist() == [-42, None]
    assert batch.column("score").to_pylist() == [1.5, None]
    assert batch.column("name").to_pylist() == ["devon", None]
    assert batch.column("embedding").to_pylist() == [[1.0, 2.0, 3.0], None]
    assert batch.column("place").to_pylist() == [
        {"lat_deg": 45.5, "lng_deg": -122.625},
        None,
    ]
    assert batch.column("happened").to_pylist() == [
        datetime.datetime(2024, 8, 9, 0, 0, 0, 123456, tzinfo=datetime.timezone.utc),
        None,
    ]
    assert batch.column("payload").to_pylist() == [b"\x00\xff\x1a", None]
    assert batch.column("amount").to_pylist() == [decimal.Decimal("12345678.90"), None]
    assert batch.column("document").to_pylist() == ['{"k":1}', None]
    assert batch.column("nothing").to_pylist() == [None, None]
    print("path=pyarrow")
else:
    # No pyarrow: walk the capsules via ctypes so the test cannot silently
    # pass. `requested_schema` is accepted (and ignored) per the spec.
    schema_capsule = result.__arrow_c_schema__()
    array_pair = result.__arrow_c_array__()
    ignored_pair = result.__arrow_c_array__(requested_schema=None)
    schema_pointer = ctypes.cast(
        capsule_pointer(schema_capsule, b"arrow_schema"), ctypes.POINTER(ArrowSchema)
    )
    pair_schema_pointer = ctypes.cast(
        capsule_pointer(array_pair[0], b"arrow_schema"), ctypes.POINTER(ArrowSchema)
    )
    array_pointer = ctypes.cast(
        capsule_pointer(array_pair[1], b"arrow_array"), ctypes.POINTER(ArrowArray)
    )
    walk(schema_pointer.contents, array_pointer.contents)
    release_schema_tree(schema_pointer)
    release_schema_tree(pair_schema_pointer)
    release_array_tree(array_pointer)
    # The ignored_requested_schema pair goes back un-released: its capsule
    # destructors must release it without crashing at garbage collection.
    del ignored_pair
    del schema_capsule, array_pair
    gc.collect()
    print("path=ctypes")
"#;

#[test]
fn python_arrow_capsules_export() {
    if !python3_available() {
        println!("SKIP: python3 is not on PATH; devondb Python arrow smoke test skipped");
        return;
    }

    let directory = TestDirectory::new().expect("temporary directory should be created");
    let module = directory.path().join("devondb.so");
    fs::copy(cdylib_path(), &module).expect("devondb extension should be copied");

    let output = Command::new("python3")
        .arg("-c")
        .arg(PYTHON_SCRIPT)
        .env("PYTHONPATH", directory.path())
        .env("DEVONDB_SMOKE_PATH", directory.path().join("arrow.devondb"))
        .output()
        .expect("python3 arrow smoke test should start");

    assert!(
        output.status.success(),
        "python3 arrow smoke test failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("python stdout should be UTF-8");
    let path = stdout.trim();
    println!("python3 arrow smoke output: {path}");
    assert!(
        path == "path=pyarrow" || path == "path=ctypes",
        "unexpected smoke test outcome: {path}"
    );
}

fn python3_available() -> bool {
    match Command::new("python3").arg("--version").output() {
        Ok(output) => output.status.success(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => panic!("failed to check python3 availability: {error}"),
    }
}

fn cdylib_path() -> PathBuf {
    let executable = std::env::current_exe().expect("test executable path should resolve");
    let deps = executable
        .parent()
        .expect("test executable should be in target/<profile>/deps");
    let profile = deps
        .parent()
        .expect("deps should be inside target/<profile>");
    let extension = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    let artifact = profile.join(format!("libdevondb_python.{extension}"));
    build_cdylib(profile);
    assert!(
        artifact.is_file(),
        "cdylib not found at {}",
        artifact.display()
    );
    artifact
}

fn build_cdylib(profile: &Path) {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("binding crate should be inside the workspace");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command.args(["rustc", "-p", "devondb-python", "--lib"]);
    if cfg!(target_os = "macos") {
        command.args([
            "--",
            "-C",
            "link-arg=-undefined",
            "-C",
            "link-arg=dynamic_lookup",
        ]);
    }
    let output = command
        .current_dir(workspace)
        .env(
            "CARGO_TARGET_DIR",
            profile.parent().expect("profile has target root"),
        )
        .output()
        .expect("cargo build for the cdylib should start");
    assert!(
        output.status.success(),
        "cargo build for the cdylib failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> io::Result<Self> {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should follow the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "devondb-python-arrow-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
