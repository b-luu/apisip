//! End to end: the real binary against the mock API in `examples/mock-api`.
//!
//! The DuckDB checks are skipped when `duckdb` is not on the PATH, unless
//! `APISIP_REQUIRE_DUCKDB` is set. The S3
//! test needs `APISIP_TEST_S3=1` and a bucket (see `s3_destination`), and the
//! live-spec tests are `#[ignore]`d: `cargo test -- --ignored`.

#[path = "../examples/mock-api/server.rs"]
#[allow(dead_code)]
mod mock;

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

struct Mock {
    rt: tokio::runtime::Runtime,
    base: String,
}

impl Mock {
    fn start() -> Self {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let addr = rt
            .block_on(mock::serve(([127, 0, 0, 1], 0).into()))
            .unwrap();
        Mock {
            rt,
            base: format!("http://{addr}"),
        }
    }

    fn spec_url(&self) -> String {
        format!("{}/openapi.json", self.base)
    }

    fn post(&self, path: &str) {
        let url = format!("{}{path}", self.base);
        let status = self
            .rt
            .block_on(reqwest::Client::new().post(url).send())
            .unwrap()
            .status();
        assert!(status.is_success(), "{path}: {status}");
    }

    /// Requests served so far, per path.
    fn requests(&self) -> Value {
        let url = format!("{}/_admin/requests", self.base);
        self.rt.block_on(async {
            reqwest::get(url)
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        })
    }
}

fn count(requests: &Value, path: &str) -> u64 {
    requests[path].as_u64().unwrap_or(0)
}

fn apisip(dir: &Path) -> Command {
    let mut c = Command::cargo_bin("apisip").unwrap();
    c.arg("-C")
        .arg(dir)
        .env_remove("MOCKSHOP_TOKEN")
        .env_remove("APISIP_FAIL_POINT");
    c
}

fn init(mock: &Mock, dir: &Path, extra: &[&str]) {
    apisip(dir)
        .args(["init", "--yes", &mock.spec_url(), "--auth", "bearerAuth"])
        .args(extra)
        .env("MOCKSHOP_TOKEN", mock::TOKEN)
        .assert()
        .success();
}

fn stdout(a: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stdout).into_owned()
}

fn stderr(a: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stderr).into_owned()
}

fn files(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(files(&p, ext));
            } else if p.extension().is_some_and(|e| e == ext) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Committed JSONL records of a stream (quarantine excluded).
fn records(dir: &Path, stream: &str) -> Vec<Value> {
    files(&dir.join("data").join(stream), "jsonl")
        .iter()
        .filter(|p| !p.to_string_lossy().contains("_quarantine"))
        .flat_map(|p| {
            std::fs::read_to_string(p)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect::<Vec<Value>>()
        })
        .collect()
}

/// Runs `query` after loading the generated views. `None` without duckdb.
fn duckdb(dir: &Path, query: &str) -> Option<String> {
    let out = std::process::Command::new("duckdb")
        .arg("-csv")
        .arg("-noheader")
        .arg("-c")
        .arg(format!(
            ".read {}",
            dir.join("data/_apisip/views/all.sql").display()
        ))
        .arg("-c")
        .arg(query)
        .output();
    let out = match out {
        Ok(out) => out,
        Err(e) if std::env::var_os("APISIP_REQUIRE_DUCKDB").is_some() => {
            panic!("APISIP_REQUIRE_DUCKDB is set but duckdb cannot run: {e}")
        }
        Err(_) => return None,
    };
    assert!(
        out.status.success(),
        "duckdb failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8(out.stdout).unwrap().trim().to_string())
}

#[test]
fn jsonl_init_check_run_and_incremental_rerun() {
    let mock = Mock::start();
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    init(&mock, d, &[]);
    assert_eq!(
        std::fs::read_to_string(d.join(".env")).unwrap(),
        "MOCKSHOP_TOKEN=mock-token\n"
    );

    let check = apisip(d).arg("check").assert().success();
    let out = stdout(&check) + &stderr(&check);
    assert!(
        !out.contains(mock::TOKEN),
        "the token must never be printed"
    );

    let before = mock.requests();
    let first = apisip(d).arg("run").assert().success();
    let first_out = stdout(&first);
    assert!(
        first_out.contains("Views: data/_apisip/views/all.sql"),
        "{first_out}"
    );
    let after_first = mock.requests();
    for (stream, n) in [("users", 25), ("orders", 30), ("events", 20), ("plans", 6)] {
        assert_eq!(records(d, stream).len(), n, "{stream}");
    }
    // The undeclared `nickname` and the mistyped `age` are reported as drift.
    let drift = stdout(&first) + &stderr(&first);
    assert!(drift.contains("nickname"), "{drift}");
    assert!(drift.contains("age"), "{drift}");

    // Change some records, then run again.
    mock.post("/_admin/touch/users/3");
    mock.post("/_admin/touch/users/4");
    mock.post("/_admin/touch/orders/5");
    mock.post("/_admin/touch/plans/2");
    mock.post("/_admin/add/users");
    apisip(d).arg("run").assert().success();
    let after_second = mock.requests();

    let delta = |a: &Value, b: &Value, p: &str| count(b, p) - count(a, p);
    // The first users run also retried the mock's one-off 429.
    assert!(
        delta(&after_first, &after_second, "/users") < delta(&before, &after_first, "/users"),
        "filter mode fetches fewer pages: {before} {after_first} {after_second}"
    );
    assert!(
        delta(&after_first, &after_second, "/orders") < delta(&before, &after_first, "/orders"),
        "early stop fetches fewer pages"
    );
    // The events endpoint lies about its sort: the guard falls back to a full
    // fetch and `list` says so.
    assert_eq!(
        delta(&after_first, &after_second, "/events"),
        delta(&before, &after_first, "/events")
    );
    apisip(d)
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("early stop (unreliable)"));

    // users: 25 + 3 changed or new records + the inclusive boundary record.
    let users = records(d, "users");
    assert!(users.len() < 25 + 26, "{}", users.len());
    assert_eq!(
        users
            .iter()
            .filter(|u| u["id"] == 3 && u["name"] == "user 3 (edited)")
            .count(),
        1
    );
    assert_eq!(records(d, "plans").len(), 12, "a second full snapshot");
    let plan_runs: std::collections::BTreeSet<_> = files(&d.join("data/plans"), "jsonl")
        .iter()
        .map(|p| p.parent().unwrap().to_path_buf())
        .collect();
    assert_eq!(plan_runs.len(), 2, "one run_id= folder per snapshot");

    let Some(current) = duckdb(
        d,
        "select (select count(*) from users_current), (select count(distinct id) from users_current), \
         (select name from users_current where id = 3), (select count(*) from orders_current), \
         (select count(*) from plans_current), (select name from plans_current where id = 2)",
    ) else {
        eprintln!("duckdb not found: skipping the view checks");
        return;
    };
    assert_eq!(current, "26,26,user 3 (edited),30,6,plan 2 (edited)");
    let history = duckdb(
        d,
        "select string_agg(name || ':' || (valid_to is null) || ':' || is_current, ' ' order by valid_from) \
         from users_history where id = 3",
    )
    .unwrap();
    assert_eq!(history, "user 3:false:false user 3 (edited):true:true");
    let plans = duckdb(
        d,
        "select string_agg(name || ':' || is_current, ' ' order by valid_from) from plans_history where id = 2",
    )
    .unwrap();
    assert_eq!(plans, "plan 2:false plan 2 (edited):true");
}

#[test]
fn parquet_types_follow_the_spec_and_bad_records_are_quarantined() {
    let mock = Mock::start();
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    init(&mock, d, &["--format", "parquet", "--streams", "users"]);
    let run = apisip(d).arg("run").assert().success();
    assert!(stdout(&run).contains("1 quarantined"), "{}", stdout(&run));
    assert_eq!(files(&d.join("data/users"), "parquet").len(), 1);

    let quarantine = files(&d.join("data/users/_quarantine"), "jsonl");
    assert_eq!(quarantine.len(), 1);
    let q = std::fs::read_to_string(&quarantine[0]).unwrap();
    assert_eq!(q.lines().count(), 1);
    assert!(q.contains("\"id\":13") && q.contains("age"), "{q}");

    if let Some(types) = duckdb(
        d,
        "select string_agg(column_name || ' ' || column_type, ', ' order by column_name) from (describe users_current) \
         where column_name in ('id', 'updated_at', 'address', 'age', '_apisip_extra')",
    ) {
        assert_eq!(
            types.trim_matches('"'),
            "_apisip_extra VARCHAR, address STRUCT(city VARCHAR), age BIGINT, id BIGINT, updated_at TIMESTAMP WITH TIME ZONE"
        );
        let extra = duckdb(d, "select _apisip_extra from users_current where id = 7").unwrap();
        assert!(extra.contains("nickname"), "{extra}");
        let n = duckdb(d, "select count(*) from users_current").unwrap();
        assert_eq!(n, "24", "25 users minus the quarantined one");
    } else {
        eprintln!("duckdb not found: skipping the type checks");
    }

    apisip(d)
        .args(["run", "--full-refresh", "--strict"])
        .assert()
        .failure()
        .stdout(predicate::str::contains(
            "FAILED: a record does not match the spec",
        ));
}

#[test]
fn check_explains_a_missing_token() {
    let mock = Mock::start();
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    apisip(d)
        .args(["init", "--yes", &mock.spec_url(), "--auth", "bearerAuth"])
        .assert()
        .success();
    apisip(d)
        .arg("check")
        .assert()
        .failure()
        .stderr(predicate::str::contains("MOCKSHOP_TOKEN"));
}

#[test]
fn a_missing_rate_limit_is_warned_about() {
    let mock = Mock::start();
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    init(&mock, d, &["--streams", "plans"]);
    let config = d.join("apisip.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    let text: String = text
        .lines()
        .filter(|l| !l.starts_with("rate_limit"))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&config, text).unwrap();
    for cmd in ["check", "run"] {
        apisip(d)
            .arg(cmd)
            .assert()
            .success()
            .stderr(predicate::str::contains("no rate_limit in apisip.toml"));
    }
}

/// Fail points exist in debug builds only.
#[cfg(debug_assertions)]
#[test]
fn a_crash_after_staging_commits_nothing() {
    let mock = Mock::start();
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    init(&mock, d, &["--streams", "users"]);
    apisip(d)
        .arg("run")
        .env("APISIP_FAIL_POINT", "after_staging")
        .assert()
        .failure();
    assert!(records(d, "users").is_empty(), "no data was committed");
    let state = std::fs::read_to_string(d.join("data/_apisip/state.json")).unwrap_or_default();
    assert!(!state.contains("watermark"), "{state}");

    apisip(d).arg("run").assert().success();
    assert_eq!(records(d, "users").len(), 25);
}

// Fail points exist in debug builds only.
#[cfg(debug_assertions)]
#[test]
fn a_crash_after_the_move_only_repeats_records() {
    let mock = Mock::start();
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    init(&mock, d, &["--streams", "users"]);
    apisip(d)
        .arg("run")
        .env("APISIP_FAIL_POINT", "after_move")
        .assert()
        .failure();
    assert_eq!(records(d, "users").len(), 25, "the files were moved");
    let state = std::fs::read_to_string(d.join("data/_apisip/state.json")).unwrap_or_default();
    assert!(!state.contains("watermark"), "{state}");

    // The next run fetches the same records again; the view keeps one per key.
    apisip(d).arg("run").assert().success();
    assert_eq!(records(d, "users").len(), 50);
    if let Some(n) = duckdb(d, "select count(*) from users_current") {
        assert_eq!(n, "25");
    }
}

// Fail points exist in debug builds only.
#[cfg(debug_assertions)]
#[test]
fn an_uncommitted_snapshot_is_not_current_in_any_view() {
    let mock = Mock::start();
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    init(&mock, d, &["--streams", "plans"]);
    apisip(d).arg("run").assert().success();
    mock.post("/_admin/touch/plans/2");
    apisip(d)
        .arg("run")
        .env("APISIP_FAIL_POINT", "after_move")
        .assert()
        .failure();
    // The views of the first run read the files on disk, the moved snapshot included.
    let q = |view: &str| {
        duckdb(
            d,
            &format!("select string_agg(name, ',') from {view} id = 2"),
        )
    };
    let Some(current) = q("plans_current where") else {
        return;
    };
    assert_eq!(current, "plan 2");
    assert_eq!(q("plans_history where is_current and").unwrap(), current);

    apisip(d).arg("run").assert().success();
    assert_eq!(q("plans_current where").unwrap(), "plan 2 (edited)");
    assert_eq!(
        q("plans_history where is_current and").unwrap(),
        "plan 2 (edited)"
    );
}

#[test]
fn a_stray_cursor_value_never_moves_the_watermark() {
    for format in ["jsonl", "parquet"] {
        let mock = Mock::start();
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        init(&mock, d, &["--streams", "users", "--format", format]);
        // "N/A" sorts above every date as text, and the server returns it for
        // any `updated_since`.
        mock.post("/_admin/stray/users/3");
        apisip(d).arg("run").assert().success();
        let state: Value = serde_json::from_str(
            &std::fs::read_to_string(d.join("data/_apisip/state.json")).unwrap(),
        )
        .unwrap();
        let wm = state["streams"]["users"]["watermark"].as_str().unwrap();
        assert!(wm.starts_with("2026-"), "{format}: watermark {wm}");
        let manifest = files(&d.join("data/_apisip/runs"), "json").pop().unwrap();
        let manifest = std::fs::read_to_string(manifest).unwrap();
        if format == "jsonl" {
            assert!(manifest.contains("do not move the watermark"), "{manifest}");
        } else {
            assert!(manifest.contains("\"quarantined\": 2"), "{manifest}");
        }

        // A later change is still picked up.
        mock.post("/_admin/touch/users/5");
        let before = mock.requests();
        apisip(d).arg("run").assert().success();
        assert!(count(&mock.requests(), "/users") > count(&before, "/users"));
        let manifest = files(&d.join("data/_apisip/runs"), "json").pop().unwrap();
        let manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(manifest).unwrap()).unwrap();
        let users = &manifest["streams"]["users"];
        assert!(
            users["records"].as_u64().unwrap() >= 1,
            "{format}: the touched user was missed: {users}"
        );
    }
}

#[test]
fn records_already_written_at_the_watermark_are_not_written_again() {
    let mock = Mock::start();
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    init(&mock, d, &["--streams", "users,orders"]);
    apisip(d).arg("run").assert().success();

    // Nothing changed: the records at the watermark come back, and are skipped.
    let again = apisip(d).arg("run").assert().success();
    assert!(stdout(&again).contains("unchanged"), "{}", stdout(&again));
    assert_eq!(records(d, "users").len(), 25);
    assert_eq!(records(d, "orders").len(), 30);

    // A change and a new record within the watermark's timestamp are kept.
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(d.join("data/_apisip/state.json")).unwrap())
            .unwrap();
    let wm = state["streams"]["users"]["watermark"].as_str().unwrap();
    mock.post(&format!("/_admin/touch/users/3?at={wm}"));
    mock.post(&format!("/_admin/add/users?at={wm}"));
    apisip(d).arg("run").assert().success();
    let users = records(d, "users");
    assert_eq!(users.len(), 27, "the edited user 3 and the new user 26");
    assert!(users.iter().any(|u| u["name"] == "user 3 (edited)"));
    assert!(users.iter().any(|u| u["id"] == 26));

    // Both runs' records at that timestamp are now known.
    apisip(d).arg("run").assert().success();
    assert_eq!(records(d, "users").len(), 27);
}

#[test]
fn a_destination_belongs_to_one_folder_and_its_copies() {
    let mock = Mock::start();
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = (dir_a.path(), dir_b.path());
    init(&mock, a, &["--streams", "users"]);
    apisip(a).arg("run").assert().success();

    // Another folder pointed at the same destination is refused before it writes.
    let shared = format!("file://{}", a.join("data").display());
    init(&mock, b, &["--streams", "users", "--destination", &shared]);
    let runs = a.join("data/_apisip/runs");
    let manifests = files(&runs, "json").len();
    let check = apisip(b).arg("check").assert().failure();
    assert!(
        stdout(&check).contains("belongs to another apisip folder"),
        "{}",
        stdout(&check)
    );
    let refused = apisip(b).arg("run").assert().failure();
    assert!(
        stderr(&refused).contains("copy its .apisip/folder-id here"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(files(&runs, "json").len(), manifests, "nothing written");

    // With the owner's folder id, it is a copy and may write.
    std::fs::copy(a.join(".apisip/folder-id"), b.join(".apisip/folder-id")).unwrap();
    let check = apisip(b).arg("check").assert().success();
    assert!(stdout(&check).contains("belongs to this folder"));
    mock.post("/_admin/touch/users/3");
    apisip(b).arg("run").assert().success();
    apisip(a).arg("run").assert().success();
    if let Some(n) = duckdb(a, "select count(*), count(distinct id) from users_current") {
        assert_eq!(n, "25,25");
    }

    // A state written before destinations had an owner is adopted.
    let state_path = a.join("data/_apisip/state.json");
    let mut state: Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    state.as_object_mut().unwrap().remove("owner").unwrap();
    std::fs::write(&state_path, state.to_string()).unwrap();
    apisip(a).arg("run").assert().success();
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    let id = std::fs::read_to_string(a.join(".apisip/folder-id")).unwrap();
    assert_eq!(state["owner"].as_str(), Some(id.trim()));
}

#[test]
fn schema_files_are_written_once_and_edits_change_the_types() {
    let mock = Mock::start();
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    init(&mock, d, &["--streams", "users", "--format", "parquet"]);
    let read = |p: &str| -> Value {
        serde_json::from_str(&std::fs::read_to_string(d.join(p)).unwrap()).unwrap()
    };
    assert_eq!(read("schemas/users.json")["x-apisip-source"], "spec");
    assert_eq!(
        read("schemas/users.json"),
        read(".apisip/schemas/users.json")
    );

    // A stream the spec does not describe gets its schema from the first run.
    let config = d.join("apisip.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        text + "\n[[streams]]\nname = \"projects\"\npath = \"/projects\"\n",
    )
    .unwrap();
    apisip(d).arg("run").assert().success();
    let mut projects = read("schemas/projects.json");
    assert_eq!(projects["x-apisip-source"], "inferred");
    assert_eq!(projects["properties"]["id"]["type"], "integer");
    let list = apisip(d).arg("list").assert().success();
    assert!(
        stdout(&list)
            .lines()
            .any(|l| l.starts_with("projects") && l.contains("inferred")),
        "{}",
        stdout(&list)
    );

    // The file is the schema from now on: an edit changes the next run's
    // columns, and the run does not infer again.
    projects["properties"]["id"] = serde_json::json!({"type": "string"});
    std::fs::write(d.join("schemas/projects.json"), projects.to_string()).unwrap();
    let list = apisip(d).arg("list").assert().success();
    assert!(
        stdout(&list)
            .lines()
            .any(|l| l.starts_with("projects") && l.contains("edited")),
        "{}",
        stdout(&list)
    );
    apisip(d).arg("run").assert().success();
    assert_eq!(read("schemas/projects.json"), projects);
    let last = files(&d.join("data/projects"), "parquet").pop().unwrap();
    if let Some(t) = duckdb(
        d,
        &format!(
            "select typeof(id) from read_parquet('{}') limit 1",
            last.display()
        ),
    ) {
        assert_eq!(t, "VARCHAR");
    }
}

#[test]
fn an_empty_stream_does_not_break_the_views() {
    let mock = Mock::start();
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    init(&mock, d, &[]);
    mock.post("/_admin/clear/plans");
    let a = apisip(d).arg("run").assert().success();
    assert!(!stdout(&a).contains("plans_current"), "{}", stdout(&a));
    let views = d.join("data/_apisip/views");
    assert!(views.join("users_current.sql").exists());
    assert!(!views.join("plans_current.sql").exists());
    let all = std::fs::read_to_string(views.join("all.sql")).unwrap();
    assert!(!all.contains("plans"), "{all}");
    if let Some(n) = duckdb(d, "select count(*) from users_current") {
        assert_eq!(n, "25");
    }
}

#[test]
fn a_second_run_in_the_same_folder_is_refused() {
    let mock = Mock::start();
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    init(&mock, d, &["--streams", "users"]);
    let held = std::fs::File::create(d.join(".apisip/run.lock")).unwrap();
    held.lock().unwrap();
    let a = apisip(d).arg("run").assert().failure();
    assert!(
        stderr(&a).contains("another `apisip run` is in progress"),
        "{}",
        stderr(&a)
    );
    assert!(!d.join("data/_apisip/runs").exists(), "nothing was started");
    drop(held);
    apisip(d).arg("run").assert().success();
}

#[cfg(unix)]
#[test]
fn update_merges_the_v2_spec() {
    use rexpect::reader::Options;
    let mock = Mock::start();
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    init(&mock, d, &[]);

    // Edit the orders page size by hand, with a comment.
    let config = d.join("apisip.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    let edited = text.replace(
        "limit_param = \"limit\", limit = 10 }",
        "limit_param = \"limit\", limit = 5 } # my edit",
    );
    assert_ne!(edited, text);
    std::fs::write(&config, &edited).unwrap();
    let env_before = std::fs::read_to_string(d.join(".env")).unwrap();
    // And the orders schema.
    let orders_schema = d.join("schemas/orders.json");
    let mut orders: Value =
        serde_json::from_str(&std::fs::read_to_string(&orders_schema).unwrap()).unwrap();
    orders["description"] = "my edit".into();
    std::fs::write(&orders_schema, orders.to_string()).unwrap();

    let v2 = format!("{}/v2/openapi.json", mock.base);
    let check = apisip(d).args(["update", "--check", &v2]).assert().code(1);
    let diff = stdout(&check);
    assert!(diff.contains("projects"), "{diff}");
    assert!(diff.contains("plans"), "{diff}");
    assert!(diff.contains("+ phone"), "{diff}");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), edited);

    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("apisip"));
    cmd.arg("-C")
        .arg(d)
        .args(["update", &v2])
        .env("TERM", "xterm")
        .env_remove("MOCKSHOP_TOKEN");
    let mut p = rexpect::session::spawn_with_options(
        cmd,
        Options::new()
            .timeout_ms(Some(15_000))
            .strip_ansi_escape_codes(true),
    )
    .unwrap();
    p.exp_string("streams.orders.pagination was edited here and changed in the spec")
        .unwrap();
    p.send_line("").unwrap(); // keep mine
    p.exp_string("New endpoints to download").unwrap();
    p.send_line("").unwrap(); // none
    p.exp_string("Authentication (Enter keeps the current one)")
        .unwrap();
    p.send_line("").unwrap();
    p.exp_string("Enter keeps the current value").unwrap();
    p.send_line("").unwrap();
    let out = p.exp_eof().unwrap();
    assert!(!out.contains(mock::TOKEN), "the token must never be echoed");
    assert!(out.contains("schemas/orders.json was edited here"), "{out}");
    let users_schema = std::fs::read_to_string(d.join("schemas/users.json")).unwrap();
    assert!(users_schema.contains("\"phone\""), "unedited, updated");
    assert_eq!(
        serde_json::from_str::<Value>(&std::fs::read_to_string(&orders_schema).unwrap()).unwrap(),
        orders,
        "edited, kept"
    );

    let merged = std::fs::read_to_string(&config).unwrap();
    assert!(
        merged.contains("size = 20"),
        "users: unedited, updated\n{merged}"
    );
    assert!(
        merged.contains("limit = 5 } # my edit"),
        "orders: the edit and its comment are kept\n{merged}"
    );
    assert!(merged.contains(&format!("spec = \"{v2}\"")), "{merged}");
    let plans = merged.split("name = \"plans\"").nth(1).unwrap();
    assert!(
        plans.contains("enabled = false"),
        "plans was removed\n{merged}"
    );
    let projects = merged.split("name = \"projects\"").nth(1).unwrap();
    assert!(projects.contains("enabled = false"), "{merged}");
    assert!(merged.contains("(added by apisip update)"), "{merged}");
    assert_eq!(std::fs::read_to_string(d.join(".env")).unwrap(), env_before);

    apisip(d)
        .args(["update", "--check"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No change"));
    apisip(d)
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"plans\s+no").unwrap());
}

/// Needs a reachable S3-compatible bucket:
///
/// ```sh
/// APISIP_TEST_S3=1 APISIP_TEST_S3_URL=s3://apisip-test/e2e \
///   AWS_ENDPOINT_URL=http://127.0.0.1:9000 AWS_ACCESS_KEY_ID=... \
///   AWS_SECRET_ACCESS_KEY=... AWS_REGION=us-east-1 cargo test --test e2e s3
/// ```
#[test]
fn s3_destination() {
    if std::env::var("APISIP_TEST_S3").as_deref() != Ok("1") {
        eprintln!("APISIP_TEST_S3 is not 1: skipping");
        return;
    }
    let url = std::env::var("APISIP_TEST_S3_URL").expect("APISIP_TEST_S3_URL");
    let endpoint = std::env::var("AWS_ENDPOINT_URL").expect("AWS_ENDPOINT_URL");
    let prefix = format!(
        "{}/{}",
        url.trim_end_matches('/'),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let mock = Mock::start();
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    init(
        &mock,
        d,
        &[
            "--destination",
            &prefix,
            "--format",
            "parquet",
            "--s3-endpoint",
            &endpoint,
        ],
    );
    apisip(d).arg("check").assert().success();
    apisip(d).arg("run").assert().success();
    mock.post("/_admin/add/users");
    apisip(d).arg("run").assert().success();

    // Read the data back from the bucket with DuckDB's httpfs.
    let host = endpoint
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let setup = format!(
        "install httpfs; load httpfs; create secret (type s3, key_id '{}', secret '{}', region '{}', endpoint '{host}', url_style 'path', use_ssl {});",
        std::env::var("AWS_ACCESS_KEY_ID").unwrap(),
        std::env::var("AWS_SECRET_ACCESS_KEY").unwrap(),
        std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".into()),
        endpoint.starts_with("https"),
    );
    let sql = std::process::Command::new("duckdb")
        .arg("-csv")
        .arg("-noheader")
        .arg("-c")
        .arg(&setup)
        .arg("-c")
        .arg(format!(
            "select count(*) from read_parquet('{prefix}/users/dt=*/**/*.parquet')"
        ))
        .output();
    match sql {
        Ok(out) if out.status.success() => {
            let n = String::from_utf8_lossy(&out.stdout);
            // 24 + the inclusive boundary record + the new user (user 13 is quarantined).
            assert_eq!(n.lines().last().unwrap().trim(), "26", "{n}");
        }
        Ok(out) => panic!("duckdb: {}", String::from_utf8_lossy(&out.stderr)),
        Err(_) => eprintln!("duckdb not found: skipping the read-back"),
    }
}

/// The live Petstore v3 server: `init --yes` then `run`.
#[test]
#[ignore = "needs network"]
fn live_petstore() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    apisip(d)
        .args([
            "init",
            "--yes",
            "https://petstore3.swagger.io/api/v3/openapi.json",
        ])
        // The demo server accepts this documented key.
        .env("PETSTORE_API_KEY", "special-key")
        .assert()
        .success();
    apisip(d)
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("/pet/findByStatus"))
        .stdout(predicate::str::contains("logout").not());
    apisip(d).arg("run").assert().success();
    assert!(!records(d, "pet_find_by_status").is_empty());
}

/// GitHub's REST spec (about 10 MB) as a parser and detection stress test.
#[test]
#[ignore = "needs network"]
fn live_github_detection() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    apisip(d)
        .args([
            "init",
            "--yes",
            "https://raw.githubusercontent.com/github/rest-api-description/main/descriptions/api.github.com/api.github.com.json",
        ])
        .assert()
        .success();
    apisip(d)
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("/repositories"));
}

/// A query-string API key appears in reqwest's error URLs: it must never
/// reach the terminal or the manifest.
#[test]
fn a_query_api_key_is_never_shown() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    init_keyed(d, &format!("http://{closed}"));

    let mut shown = String::new();
    for cmd in ["check", "run"] {
        let a = apisip(d).args([cmd, "-v"]).assert().failure();
        shown += &stdout(&a);
        shown += &stderr(&a);
    }
    for f in files(&d.join("data/_apisip/runs"), "json") {
        shown += &std::fs::read_to_string(f).unwrap();
    }
    assert!(shown.contains("FAILED: GET http://127.0.0.1"), "{shown}");
    assert!(!shown.contains(KEY), "the API key leaked:\n{shown}");
}

#[test]
fn a_next_link_that_repeats_the_key_is_never_logged() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // Every page links to the same URL, key included, with new records each
    // time, so paging stops on the "already fetched" check.
    std::thread::spawn(move || {
        for (n, conn) in listener.incoming().enumerate() {
            let Ok(mut conn) = conn else { continue };
            let mut buf = [0u8; 4096];
            let _ = conn.read(&mut buf);
            let body = format!("[{{\"id\": {n}}}]");
            let _ = write!(
                conn,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Link: <http://{addr}/things?api_key={KEY}>; rel=\"next\"\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    init_keyed(d, &format!("http://{addr}"));
    // An empty RUST_LOG (the nix build sets one) must not silence -vv.
    let a = apisip(d)
        .args(["run", "-vv"])
        .env("RUST_LOG", "")
        .assert()
        .success();
    let shown = stdout(&a) + &stderr(&a);
    assert!(shown.contains("already fetched"), "{shown}");
    assert!(!shown.contains(KEY), "the API key leaked:\n{shown}");
}

const KEY: &str = "qk_live_SECRET_9f8e7d";

/// A folder for an API that takes its key in the query string.
fn init_keyed(d: &Path, server: &str) {
    let spec = d.join("spec.json");
    std::fs::write(
        &spec,
        serde_json::json!({
            "openapi": "3.0.3",
            "info": {"title": "Keyed", "version": "1"},
            "servers": [{"url": server}],
            "security": [{"key": []}],
            "components": {"securitySchemes": {
                "key": {"type": "apiKey", "in": "query", "name": "api_key"}
            }},
            "paths": {"/things": {"get": {"responses": {"200": {
                "description": "ok",
                "content": {"application/json": {"schema": {"type": "array", "items": {
                    "type": "object", "properties": {"id": {"type": "integer"}}
                }}}}
            }}}}}
        })
        .to_string(),
    )
    .unwrap();
    apisip(d)
        .args(["init", "--yes", spec.to_str().unwrap()])
        .env("KEYED_API_KEY", KEY)
        .assert()
        .success();
    let config = d.join("apisip.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(
        text.contains("in = \"query\"") || text.contains("location = \"query\""),
        "{text}"
    );
    std::fs::write(
        &config,
        text.replace("max_tries = 8", "max_tries = 2")
            .replace("max_backoff = \"1m\"", "max_backoff = \"1s\""),
    )
    .unwrap();
}
