# devondb C API

Build the shared and static libraries:

```sh
cargo build -p devondb-c
```

Include `include/devondb.h`; each handle must be used by only one thread.
Returned JSON strings are released with `devondb_string_free`.

```c
devondb_database *db = NULL;
devondb_create("app.devondb", 4096, &db);
devondb_execute(db, "create node table Item (id Int64 primary key)");
devondb_close(db);
```

Compile and link on macOS/Linux with
`cc -std=c99 app.c -I crates/devondb-c/include -L target/debug -ldevondb_c -Wl,-rpath,target/debug`.
