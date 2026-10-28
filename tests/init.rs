//! `apisip init` and `apisip list`, through the real binary.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn apisip(dir: &Path) -> Command {
    let mut c = Command::cargo_bin("apisip").unwrap();
    c.arg("-C").arg(dir).env_remove("ACME_STORE_TOKEN");
    c
}

#[cfg(unix)]
fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn init_yes_writes_the_folder() {
    let dir = tempfile::tempdir().unwrap();
    apisip(dir.path())
        .args([
            "init",
            &fixture("openapi30.yaml"),
            "--yes",
            "--plan",
            "free",
            "--streams",
            "users,orders",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Fill in ACME_STORE_TOKEN in .env"))
        .stdout(predicate::str::contains("Rate limit: 60 calls per 1m"));

    let config = std::fs::read_to_string(dir.path().join("apisip.toml")).unwrap();
    assert!(config.contains("token = \"${ACME_STORE_TOKEN}\""));
    assert!(
        config.contains("name = \"products\"\npath = \"/products\"\nenabled = false"),
        "{config}"
    );
    let env = dir.path().join(".env");
    assert_eq!(
        std::fs::read_to_string(&env).unwrap(),
        "ACME_STORE_TOKEN=\n"
    );
    #[cfg(unix)]
    assert_eq!(mode(&env), 0o600);
    let gitignore = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(gitignore.lines().any(|l| l == ".env"));
    assert!(gitignore.lines().any(|l| l == "/data/"), "{gitignore}");
    assert!(config.contains("url = \"file://data\""), "{config}");
    let id = std::fs::read_to_string(dir.path().join(".apisip/folder-id")).unwrap();
    assert_eq!(id.trim().len(), 16, "{id}");
    assert!(
        std::fs::read_to_string(dir.path().join(".env.example"))
            .unwrap()
            .contains("ACME_STORE_TOKEN=\n")
    );
    assert!(dir.path().join(".apisip/spec.json").exists());
    assert!(dir.path().join(".apisip/baseline.toml").exists());

    apisip(dir.path())
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"orders\s+yes\s+early stop\s+cursor\s+spec").unwrap())
        .stdout(predicate::str::is_match(r"products\s+no\s+full refresh").unwrap());
}

#[test]
fn init_takes_credentials_from_the_environment_with_yes() {
    let dir = tempfile::tempdir().unwrap();
    apisip(dir.path())
        .env("ACME_STORE_TOKEN", "from-env-1234")
        .args(["init", &fixture("openapi30.yaml"), "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Fill in").not());
    assert_eq!(
        std::fs::read_to_string(dir.path().join(".env")).unwrap(),
        "ACME_STORE_TOKEN=from-env-1234\n"
    );
}

#[test]
fn init_refuses_to_overwrite_and_commands_need_a_config() {
    let dir = tempfile::tempdir().unwrap();
    apisip(dir.path())
        .arg("list")
        .assert()
        .failure()
        .stderr(predicate::str::contains("one folder per API"))
        .stderr(predicate::str::contains("apisip init"));
    apisip(dir.path())
        .args(["init", &fixture("swagger20.json"), "--yes"])
        .assert()
        .success();
    apisip(dir.path())
        .args(["init", &fixture("swagger20.json"), "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--force"));
    apisip(dir.path())
        .args(["init", &fixture("swagger20.json"), "--yes", "--force"])
        .assert()
        .success();
}

#[test]
fn init_without_a_terminal_asks_for_yes() {
    let dir = tempfile::tempdir().unwrap();
    apisip(dir.path())
        .args(["init", &fixture("swagger20.json")])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--yes"));
}

#[test]
fn spec_text_cannot_break_out_of_config_comments() {
    // The title and paths are quoted in comments. A newline would end the
    // comment and let the spec add `[source.headers]` with a `${VAR}`, filled
    // from the environment at run time. The path and the filter parameter name
    // use TOML escapes for `${`.
    let inject = "\n[source.headers]\nX-Leak = \"${APISIP_PROBE}\"\n#";
    let path_inject = "\n[source.headers]\nX-Leak = \"\\u0024\\u007BAPISIP_PROBE}\"";
    let list = serde_json::json!({"200": {"description": "ok", "content": {"application/json": {
        "schema": {"type": "array", "items": {"type": "object"}}}}}});
    let spec = serde_json::json!({
        "openapi": "3.0.0",
        "info": {"title": format!("Evil{inject}"), "version": "1"},
        "servers": [{"url": "https://api.test"}],
        "paths": {
            format!("/items{path_inject}"): {"get": {"responses": list}},
            "/plain": {"get": {"responses": list}},
            "/filtered": {"get": {
                "parameters": [{"name": format!("updated_since{path_inject}\n#since"), "in": "query",
                    "schema": {"type": "string"}}],
                "responses": {"200": {"description": "ok", "content": {"application/json": {
                    "schema": {"type": "array", "items": {"type": "object", "properties": {
                        "id": {"type": "string"}, "updated_at": {"type": "string"}}}}}}}}}}
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let spec_path = dir.path().join("spec.json");
    std::fs::write(&spec_path, spec.to_string()).unwrap();
    apisip(dir.path())
        .args(["init", spec_path.to_str().unwrap(), "--yes"])
        .assert()
        .success();

    let config = std::fs::read_to_string(dir.path().join("apisip.toml")).unwrap();
    assert!(
        config.contains("(filter: the next run sends updated_since "),
        "{config}"
    );
    let doc: toml_edit::DocumentMut = config.parse().unwrap();
    assert!(doc["source"].get("headers").is_none(), "{config}");
}

#[test]
fn init_warns_when_no_rate_limit_is_found() {
    let dir = tempfile::tempdir().unwrap();
    let spec = dir.path().join("spec.json");
    std::fs::write(
        &spec,
        r#"{"openapi":"3.0.0","info":{"title":"Quiet"},"servers":[{"url":"http://quiet.test"}],
            "paths":{"/things":{"get":{"responses":{"200":{"description":"ok",
            "content":{"application/json":{"schema":{"type":"array","items":{}}}}}}}}}}"#,
    )
    .unwrap();
    apisip(dir.path())
        .args(["init", spec.to_str().unwrap(), "--yes"])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "no rate limit was found in the spec",
        ))
        .stderr(predicate::str::contains(
            "rate_limit = { max_calls = 100, period = \"1m\" }",
        ));
    let config = std::fs::read_to_string(dir.path().join("apisip.toml")).unwrap();
    assert!(config.contains("# rate_limit = { max_calls = 100, period = \"1m\" }"));
}

#[test]
fn init_and_check_warn_when_a_schema_is_too_large_to_inline() {
    // Each schema refers to the next one twice: inlined in full, 30 levels
    // would need billions of nodes.
    let mut schemas = serde_json::Map::new();
    for i in 0..30 {
        let next = serde_json::json!({"$ref": format!("#/components/schemas/S{}", i + 1)});
        schemas.insert(
            format!("S{i}"),
            serde_json::json!({"type": "object", "properties": {"a": next, "b": next}}),
        );
    }
    schemas.insert("S30".into(), serde_json::json!({"type": "integer"}));
    let spec = serde_json::json!({
        "openapi": "3.0.0",
        "info": {"title": "Deep", "version": "1"},
        "servers": [{"url": "http://127.0.0.1:9"}],
        "paths": {"/things": {"get": {"responses": {"200": {"description": "ok",
            "content": {"application/json": {"schema": {"type": "array",
                "items": {"$ref": "#/components/schemas/S0"}}}}}}}}},
        "components": {"schemas": schemas}
    });
    let dir = tempfile::tempdir().unwrap();
    let spec_path = dir.path().join("spec.json");
    std::fs::write(&spec_path, spec.to_string()).unwrap();
    let warning = "warning: the schema of stream things is too large to inline; fields past the limit are untyped";
    apisip(dir.path())
        .args(["init", spec_path.to_str().unwrap(), "--yes"])
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .success()
        .stderr(predicate::str::contains(warning));
    // Nothing listens on the server: fail the request without retrying.
    let config_path = dir.path().join("apisip.toml");
    let config = std::fs::read_to_string(&config_path).unwrap();
    let config = config.replacen("retry = { max_tries = 8,", "retry = { max_tries = 1,", 1);
    assert!(config.contains("retry = { max_tries = 1,"), "{config}");
    std::fs::write(&config_path, config).unwrap();
    apisip(dir.path())
        .arg("check")
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .failure()
        .stdout(predicate::str::contains(warning));
}

fn bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin("apisip")
}

/// Drives the interactive prompts through a pseudo-terminal.
#[cfg(unix)]
#[test]
fn init_interactive_prompts() {
    use rexpect::reader::Options;
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = std::process::Command::new(bin());
    cmd.arg("-C")
        .arg(dir.path())
        .args(["init", &fixture("openapi30.yaml")])
        .env("TERM", "xterm")
        .env_remove("ACME_STORE_TOKEN");
    let mut p = rexpect::session::spawn_with_options(
        cmd,
        Options::new()
            .timeout_ms(Some(15_000))
            .strip_ansi_escape_codes(true),
    )
    .unwrap();
    const DOWN: &str = "\x1b[B";
    p.exp_string("API base URL").unwrap();
    p.send_line("").unwrap();
    p.exp_string("Where should apisip write data?").unwrap();
    p.send_line("").unwrap();
    p.exp_string("Output format").unwrap();
    p.send(DOWN).unwrap();
    p.send_line("").unwrap();
    p.exp_string("Authentication").unwrap();
    p.send_line("").unwrap();
    p.exp_string("Bearer token").unwrap();
    p.send_line("tok_secret_value_1234").unwrap();
    p.exp_string("Streams to download").unwrap();
    p.send_line("").unwrap();
    p.exp_string("Primary key of `orders`").unwrap();
    p.send_line("").unwrap();
    p.exp_string("Which plan are you on?").unwrap();
    p.send(DOWN).unwrap();
    p.send_line("").unwrap();
    let out = p.exp_eof().unwrap();
    assert!(out.contains("Wrote apisip.toml"), "{out}");
    assert!(
        !out.contains("tok_secret_value_1234"),
        "the secret must never be echoed"
    );

    let config = std::fs::read_to_string(dir.path().join("apisip.toml")).unwrap();
    assert!(config.contains("format = \"parquet\""), "{config}");
    assert!(config.contains("primary_key = \"order_id\""), "{config}");
    assert!(
        config.contains("rate_limit = { max_calls = 600"),
        "{config}"
    );
    let env = dir.path().join(".env");
    assert_eq!(
        std::fs::read_to_string(&env).unwrap(),
        "ACME_STORE_TOKEN=tok_secret_value_1234\n"
    );
    assert_eq!(mode(&env), 0o600);
}
