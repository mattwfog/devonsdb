//! Search text remains data from grounded question through pin replay.
use devondb::{
    Database, Value,
    text::{
        parser::{Parsed, parse},
        printer::print_plan,
    },
};
use devondb_nl::{Compiled, DeterministicCompiler, IntentCompiler, NL_VERSION};
fn execute(db: &mut Database, text: &str) {
    let Parsed::Statement(stmt) = parse(text).unwrap() else {
        panic!("statement")
    };
    db.execute(&stmt.stmt).unwrap();
}
#[test]
fn search_compiles_executes_and_replays_after_reopen() {
    assert_eq!(NL_VERSION, 10);
    let path =
        std::env::temp_dir().join(format!("devondb-nl-search-{}.devondb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut db = Database::create(&path, 4096).unwrap();
    execute(
        &mut db,
        "create node table Document (id Int64 primary key, body String)",
    );
    execute(
        &mut db,
        "insert into Document values (9, \"rust graph\"), (1, \"rust graph\"), (2, \"other\")",
    );
    let question = "search Document body for \"rust\"";
    let compiler = DeterministicCompiler;
    let Compiled::Plan(plan) = compiler.compile_with_database(question, &mut db) else {
        panic!("search should compile")
    };
    assert_eq!(
        print_plan(&plan).unwrap(),
        "textscan(Document.body, \"rust\", k=10) as document"
    );
    let expected = db.run(&plan).unwrap();
    assert_eq!(
        expected
            .rows
            .iter()
            .map(|row| row[0].clone())
            .collect::<Vec<_>>(),
        vec![Value::Int64(1), Value::Int64(9)]
    );
    db.pin("rust", question, &plan).unwrap();
    db.checkpoint().unwrap();
    drop(db);
    let mut db = Database::open(&path).unwrap();
    assert_eq!(db.run_pin("rust").unwrap(), expected);
    for question in [
        "search Document id for \"rust\"",
        "search Missing body for \"rust\"",
        "search Document absent for \"rust\"",
        "search Document body for rust",
        "search Document body for \"rust\" trailing",
        "search Document body for \"unclosed",
    ] {
        let Compiled::NoParse(report) = compiler.compile(question, &db.schema_summary()) else {
            panic!("must refuse {question}")
        };
        assert!(!report.unrecognized.is_empty(), "{question}: {report:?}");
        assert!(
            report
                .nearest
                .iter()
                .any(|hint| hint.example.starts_with("search ")),
            "{report:?}"
        );
    }
    let injected = "search Document body for \"rust | limit 0\"";
    let Compiled::Plan(plan) = compiler.compile(injected, &db.schema_summary()) else {
        panic!("literal is data")
    };
    assert!(print_plan(&plan).unwrap().contains("\"rust | limit 0\""));
    drop(db);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("devondb-wal"));
}
