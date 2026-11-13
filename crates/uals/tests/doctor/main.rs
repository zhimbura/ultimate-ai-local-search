//! `uals doctor`, run as the binary: its exit code and its output. The home folder, the Claude
//! Code config, the executables on PATH and the HTTP side (Milvus, the embedder) are replaced;
//! see `support.rs`.
#![cfg(unix)]

mod support;

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use support::{FakeHttp, Sandbox, claude_context_config, healthy_answer};

const NO_SEMANTIC: &str = "[semantic]\ndefault = \"none\"\n";

/// A sandbox with a current ast-index and a config that turns semantic search off.
fn without_semantic() -> (Sandbox, std::path::PathBuf) {
    let sandbox = Sandbox::new();
    sandbox.versioned("ast-index", "ast-index 3.56.0");
    let config = sandbox.config(NO_SEMANTIC);
    (sandbox, config)
}

/// A sandbox with a working claude-context install whose HTTP side is `http`.
fn claude_context(http: &FakeHttp, overrides: &[(&str, &str)]) -> (Sandbox, std::path::PathBuf) {
    let sandbox = Sandbox::new();
    sandbox.versioned("ast-index", "ast-index 3.56.0");
    sandbox.versioned("node", "v22.13.1");
    let entry = sandbox.package("0.1.16");
    let config = sandbox.config(&claude_context_config(&entry, http, overrides));
    (sandbox, config)
}

/// Whether a process with this id runs (`kill -0` sends no signal).
fn process_exists(pid: &str) -> bool {
    std::process::Command::new("kill").args(["-0", pid]).output().unwrap().status.success()
}

// config

#[test]
fn without_a_config_file_the_defaults_are_checked() {
    let sandbox = Sandbox::new();
    sandbox.versioned("ast-index", "ast-index 3.56.0");
    sandbox.fake_semble("0.7.3");

    let run = sandbox.doctor(None, &[]);

    assert_eq!(run.code(), Some(0), "{}", run.stdout());
    let lines: Vec<String> = run.stdout().lines().map(str::to_owned).collect();
    assert_eq!(lines[0], "✓ config: no file: defaults");
    assert!(lines[1].starts_with("✓ ast-index: 3.56.0 at "), "{}", lines[1]);
    assert!(lines[2].starts_with("✓ semble: 0.7.3 at "), "{}", lines[2]);
    assert_eq!(lines[3], "! lsp: no language server found: usages are found by name");
    assert!(lines[4].starts_with("    hint: install the language server"), "{}", lines[4]);
    assert!(lines[5].starts_with("! mcp: "), "{}", lines[5]);
    assert!(
        lines[6].starts_with("    hint: in the uals checkout: scripts/configure-mcp.sh --uals-bin "),
        "{}",
        lines[6]
    );
    assert_eq!(lines[7], "3 ok, 2 warn, 0 fail, 0 skip");
    assert_eq!(lines.len(), 8);
}

#[test]
fn installed_language_servers_are_listed() {
    let sandbox = Sandbox::new();
    sandbox.versioned("gopls", "golang.org/x/tools/gopls v0.21.0");
    sandbox.versioned("pyright-langserver", "pyright 1.1.400");

    let run = sandbox.doctor(None, &[]);

    let line = run.check("lsp");
    assert!(line.starts_with("✓ lsp: found gopls, pyright; not installed: kotlin-lsp, jdtls, rust-analyzer"), "{line}");
}

#[test]
fn precise_usages_turned_off_skip_the_language_servers() {
    let sandbox = Sandbox::new();
    let config = sandbox.config("[lsp]\nenabled = false\n");

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.check("lsp"), "- lsp: precise usages are off (lsp.enabled = false)");
}

#[test]
fn a_broken_config_fails_its_check_and_skips_the_rest() {
    let sandbox = Sandbox::new();
    let config = sandbox.config("[semantic]\ndefault = \"nonsense\"\n");

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(1));
    assert_eq!(run.stderr(), "");
    let stdout = run.stdout();
    assert!(stdout.starts_with("✗ config: invalid config "), "{stdout}");
    assert!(stdout.contains("nonsense"), "the parser's text is shown: {stdout}");
    assert!(stdout.contains("\n    hint: "), "{stdout}");
    for name in ["ast-index", "semantic", "lsp", "mcp"] {
        assert!(run.check(name).starts_with(&format!("- {name}: not checked")), "{name}");
    }
    assert!(stdout.ends_with("0 ok, 0 warn, 1 fail, 4 skip\n"), "{stdout}");
}

#[test]
fn an_invalid_value_in_a_valid_file_fails_the_config_check() {
    let sandbox = Sandbox::new();
    let config = sandbox.config("[output]\ndefault_limit = 0\n");

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(1));
    assert!(run.check("config").contains("output.default_limit"), "{}", run.stdout());
}

// ast-index

#[test]
fn a_missing_ast_index_is_a_warning() {
    let sandbox = Sandbox::new();
    let config = sandbox.config(NO_SEMANTIC);

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(0), "a warning is not a failure");
    assert_eq!(
        run.check("ast-index"),
        "! ast-index: ast-index not found: symbol search is off\n    hint: run install.sh to install ast-index"
    );
}

#[test]
fn an_ast_index_older_than_the_tested_one_is_a_warning() {
    let sandbox = Sandbox::new();
    sandbox.versioned("ast-index", "ast-index 3.9.2");
    let config = sandbox.config(NO_SEMANTIC);

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(0));
    let line = run.check("ast-index");
    assert!(line.starts_with("! ast-index: 3.9.2 at "), "{line}");
    assert!(line.contains("older than 3.56.0; uals was tested with 3.56.0"), "{line}");
}

#[test]
fn a_newer_ast_index_is_fine() {
    let sandbox = Sandbox::new();
    sandbox.versioned("ast-index", "ast-index 3.60.1");
    let config = sandbox.config(NO_SEMANTIC);

    let run = sandbox.doctor(Some(&config), &[]);

    assert!(run.check("ast-index").starts_with("✓ ast-index: 3.60.1 at "), "{}", run.stdout());
}

// semble

#[test]
fn semantic_search_that_is_off_is_skipped() {
    let (sandbox, config) = without_semantic();

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.check("semantic"), "- semantic: semantic search is off (semantic.default = \"none\")");
}

#[test]
fn a_missing_semble_fails() {
    let sandbox = Sandbox::new();
    let config = sandbox.config("[semantic]\ndefault = \"semble\"\n");

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(1));
    assert_eq!(run.check("semble"), "✗ semble: semble not found\n    hint: run install.sh to install Semble");
}

#[test]
fn a_semble_that_answers_gives_an_ok_with_the_time() {
    let sandbox = Sandbox::new();
    sandbox.fake_semble("0.7.3");
    let config = sandbox.config("[semantic]\ndefault = \"semble\"\n");

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(0), "{}", run.stdout());
    let line = run.check("semble");
    assert!(line.starts_with("✓ semble: 0.7.3 at "), "{line}");
    assert!(line.contains("; a search took "), "{line}");
}

#[test]
fn a_semble_of_another_version_is_a_warning() {
    let sandbox = Sandbox::new();
    sandbox.fake_semble("0.7.2");
    let config = sandbox.config("[semantic]\ndefault = \"semble\"\n");

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(0));
    let line = run.check("semble");
    assert!(line.starts_with("! semble: 0.7.2 at "), "{line}");
    assert!(line.contains("uals was tested with 0.7.3, this is 0.7.2"), "{line}");
    assert!(line.ends_with("hint: run install.sh to install Semble 0.7.3"), "{line}");
}

#[test]
fn a_semble_that_cannot_answer_a_search_fails_with_its_reason() {
    for (mode, reason) in [
        ("broken", "Failed to index the folder: boom"),
        ("empty", "the search of a folder with one source file found nothing"),
    ] {
        let sandbox = Sandbox::new();
        sandbox.fake_semble("0.7.3");
        let config = sandbox.config(&format!(
            "[semantic]\ndefault = \"semble\"\n\n[semantic.semble]\ncommand = [\"semble\"]\nenv = {{ FAKE_SEMBLE_MODE = \"{mode}\" }}\n"
        ));

        let run = sandbox.doctor(Some(&config), &[]);

        assert_eq!(run.code(), Some(1), "{mode}");
        let line = run.check("semble");
        assert!(line.starts_with("✗ semble: 0.7.3; the search failed after "), "{mode}: {line}");
        assert!(line.contains(reason), "{mode}: {line}");
    }
}

#[test]
fn the_semble_server_is_stopped_and_the_test_folder_is_gone_when_doctor_ends() {
    let sandbox = Sandbox::new();
    sandbox.fake_semble("0.7.3");
    let pidfile = sandbox.path("semble.pid");
    let config = sandbox.config(&format!(
        "[semantic]\ndefault = \"semble\"\n\n[semantic.semble]\nenv = {{ FAKE_SEMBLE_PIDFILE = {:?} }}\n",
        pidfile.display().to_string()
    ));

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(0), "{}", run.stdout());
    let pid = std::fs::read_to_string(&pidfile).expect("the fake server started");
    assert!(!process_exists(pid.trim()), "the semble server {pid} is still running");
    let cache = std::fs::read_to_string(sandbox.path("semble.pid.cache")).unwrap();
    assert!(cache.trim().contains("/uals-doctor-"), "the test index stays out of the user's Semble cache: {cache}");
    let left: Vec<_> = std::fs::read_dir(sandbox.path(""))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("uals-doctor-"))
        .collect();
    assert_eq!(left, Vec::<String>::new(), "the temporary folder is removed");
}

// claude-context: node and package

#[test]
fn a_node_older_than_22_12_fails() {
    let http = FakeHttp::healthy(768);
    let (sandbox, config) = claude_context(&http, &[]);
    sandbox.versioned("node", "v20.19.0");

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(1));
    let line = run.check("claude-context node");
    assert!(line.starts_with("✗ claude-context node: 20.19.0 at "), "{line}");
    assert!(line.contains("older than 22.12.0"), "{line}");
}

#[test]
fn a_missing_node_fails() {
    let http = FakeHttp::healthy(768);
    let (sandbox, config) = claude_context(&http, &[]);
    std::fs::remove_file(sandbox.path("bin/node")).unwrap();

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(1));
    assert!(run.check("claude-context node").starts_with("✗ claude-context node: node not found"), "{}", run.stdout());
}

#[test]
fn a_missing_entry_point_fails_with_the_install_hint() {
    let http = FakeHttp::healthy(768);
    let (sandbox, config) = claude_context(&http, &[]);
    std::fs::remove_file(sandbox.path("claude-context/dist/index.js")).unwrap();

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(1));
    let line = run.check("claude-context package");
    assert!(line.starts_with("✗ claude-context package: not installed: "), "{line}");
    assert!(line.ends_with("hint: run install.sh --semantic claude-context"), "{line}");
}

#[test]
fn a_package_of_another_version_is_a_warning() {
    let http = FakeHttp::healthy(768);
    let (sandbox, config) = claude_context(&http, &[]);
    sandbox.package("0.1.9");

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(0), "{}", run.stdout());
    let line = run.check("claude-context package");
    assert!(line.starts_with("! claude-context package: 0.1.9 at "), "{line}");
    assert!(line.contains("tested with 0.1.16"), "{line}");
}

// claude-context: embedder

#[test]
fn a_wrong_embedding_dimension_fails_and_names_the_right_one() {
    let http = FakeHttp::healthy(1024);
    let (sandbox, config) = claude_context(&http, &[("EMBEDDING_DIMENSION", "768")]);

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(1));
    let line = run.check("claude-context embedder");
    assert!(
        line.starts_with(
            "✗ claude-context embedder: model test-model returns 1024 values but EMBEDDING_DIMENSION is 768"
        ),
        "{line}"
    );
    assert!(line.contains("hint: set EMBEDDING_DIMENSION=1024"), "{line}");
}

#[test]
fn an_embedder_that_is_down_fails() {
    let http = FakeHttp::healthy(768);
    let unused = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let down = format!("http://127.0.0.1:{unused}/v1");
    let (sandbox, config) = claude_context(&http, &[("OPENAI_BASE_URL", &down)]);

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(1));
    assert!(
        run.check("claude-context embedder")
            .starts_with(&format!("✗ claude-context embedder: cannot reach {down}/embeddings")),
        "{}",
        run.stdout()
    );
}

#[test]
fn an_embedder_that_rejects_the_model_fails_with_its_message() {
    let http = FakeHttp::start(|request| {
        if request.path.ends_with("/embeddings") {
            (404, json!({"error": {"message": "model not found"}}).to_string())
        } else {
            healthy_answer(request, 768)
        }
    });
    let (sandbox, config) = claude_context(&http, &[]);

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(1));
    let line = run.check("claude-context embedder");
    assert!(line.contains("answered 404 without a vector"), "{line}");
    assert!(line.contains("model not found"), "{line}");
}

#[test]
fn the_key_goes_to_the_embedder_and_never_into_the_output() {
    let http = FakeHttp::healthy(768);
    let (sandbox, config) = claude_context(&http, &[("OPENAI_API_KEY", "sk-very-secret")]);

    let run = sandbox.doctor(Some(&config), &["--format", "json"]);

    assert_eq!(run.code(), Some(0), "{}", run.stdout());
    assert!(!run.stdout().contains("sk-very-secret") && !run.stderr().contains("sk-very-secret"));
    let embedding = http.requests().into_iter().find(|request| request.path == "/v1/embeddings").unwrap();
    assert_eq!(embedding.authorization.as_deref(), Some("Bearer sk-very-secret"));
    assert_eq!(embedding.body, json!({"model": "test-model", "input": "test"}));
}

/// `config` with `names` listed in `pass_env` of claude-context, as the installer writes a key.
fn with_pass_env(config: &str, names: &[&str]) -> String {
    let list: Vec<String> = names.iter().map(|name| format!("{name:?}")).collect();
    config.replace("\nenv = {", &format!("\npass_env = [{}]\nenv = {{", list.join(", ")))
}

#[test]
fn a_key_named_in_pass_env_goes_from_the_environment_to_the_embedder() {
    let http = FakeHttp::healthy(768);
    let (sandbox, config) = claude_context(&http, &[]);
    let config = sandbox.config(&with_pass_env(&std::fs::read_to_string(&config).unwrap(), &["OPENAI_API_KEY"]));

    let run = sandbox.doctor_with(Some(&config), &[], &[("OPENAI_API_KEY", "sk-from-environment")]);

    assert_eq!(run.code(), Some(0), "{}", run.stdout());
    assert!(!run.stdout().contains("sk-from-environment") && !run.stderr().contains("sk-from-environment"));
    let embedding = http.requests().into_iter().find(|request| request.path == "/v1/embeddings").unwrap();
    assert_eq!(embedding.authorization.as_deref(), Some("Bearer sk-from-environment"));
}

#[test]
fn a_key_in_the_environment_that_pass_env_does_not_name_is_not_sent() {
    let http = FakeHttp::healthy(768);
    let (sandbox, config) = claude_context(&http, &[]);

    let run = sandbox.doctor_with(Some(&config), &[], &[("OPENAI_API_KEY", "sk-from-environment")]);

    assert_eq!(run.code(), Some(0), "{}", run.stdout());
    let embedding = http.requests().into_iter().find(|request| request.path == "/v1/embeddings").unwrap();
    assert_eq!(embedding.authorization, None);
}

#[test]
fn an_embedder_that_redirects_is_not_followed() {
    let elsewhere = FakeHttp::healthy(768);
    let target = format!("http://{}/v1/embeddings", elsewhere.address());
    let embedder = FakeHttp::start(move |_| (302, target.clone()));
    let milvus = FakeHttp::healthy(768);
    let (sandbox, config) = claude_context(&embedder, &[("MILVUS_ADDRESS", &milvus.address())]);

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(1), "{}", run.stdout());
    assert!(run.check("claude-context embedder").starts_with("✗ claude-context embedder: "), "{}", run.stdout());
    assert_eq!(embedder.paths(), ["/v1/embeddings"]);
    assert_eq!(elsewhere.paths(), Vec::<String>::new(), "the redirect was followed");
}

#[test]
fn an_ollama_embedder_is_asked_the_ollama_way() {
    let http = FakeHttp::start(|request| {
        if request.path == "/api/embeddings" {
            (200, json!({"embedding": vec![0.5; 768]}).to_string())
        } else {
            healthy_answer(request, 768)
        }
    });
    let host = format!("http://{}", http.address());
    let (sandbox, config) = claude_context(&http, &[("EMBEDDING_PROVIDER", "Ollama"), ("OLLAMA_HOST", &host)]);

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(0), "{}", run.stdout());
    let ollama = http.requests().into_iter().find(|request| request.path == "/api/embeddings").unwrap();
    assert_eq!(ollama.body, json!({"model": "test-model", "prompt": "test"}));
}

#[test]
fn a_remote_embedder_is_not_probed() {
    let http = FakeHttp::healthy(768);
    let (sandbox, config) = claude_context(&http, &[("OPENAI_BASE_URL", "https://embeddings.example.com/v1")]);

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(0), "{}", run.stdout());
    assert!(
        run.check("claude-context embedder")
            .starts_with("- claude-context embedder: doctor probes local endpoints only"),
        "{}",
        run.stdout()
    );
    assert!(!http.paths().iter().any(|path| path.ends_with("/embeddings")), "nothing was sent to the embedder");
}

// claude-context: Milvus

#[test]
fn a_milvus_that_works_gets_a_collection_that_is_dropped_again() {
    let http = FakeHttp::healthy(768);
    let (sandbox, config) = claude_context(&http, &[]);

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(0), "{}", run.stdout());
    assert!(
        run.check("claude-context milvus").contains("accepted a test collection at dimension 768"),
        "{}",
        run.stdout()
    );
    let milvus: Vec<_> = http.requests().into_iter().filter(|request| request.path.starts_with("/v2/")).collect();
    let steps: Vec<&str> = milvus.iter().map(|request| request.path.as_str()).collect();
    assert_eq!(
        steps,
        [
            "/v2/vectordb/collections/create",
            "/v2/vectordb/entities/insert",
            "/v2/vectordb/entities/search",
            "/v2/vectordb/collections/drop"
        ]
    );
    let collection = milvus[0].body["collectionName"].as_str().unwrap().to_owned();
    assert!(collection.starts_with("uals_doctor_"), "{collection}");
    assert!(milvus.iter().all(|request| request.body["collectionName"] == collection), "one collection, all steps");
    assert_eq!(milvus[0].body["dimension"], 768);
    let rows = milvus[1].body["data"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["vector"].as_array().unwrap().len(), 768);
    assert_eq!(milvus[2].body["consistencyLevel"], "Strong");
}

#[test]
fn a_milvus_that_rejects_the_insert_fails_and_the_collection_is_still_dropped() {
    let http = FakeHttp::start(|request| {
        if request.path.ends_with("/entities/insert") {
            (200, json!({"code": 1100, "message": "dimension mismatch: expected 768"}).to_string())
        } else {
            healthy_answer(request, 768)
        }
    });
    let (sandbox, config) = claude_context(&http, &[]);

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(1));
    let line = run.check("claude-context milvus");
    assert!(line.contains("rejected an insert at dimension 768"), "{line}");
    assert!(line.contains("dimension mismatch: expected 768"), "the answer of Milvus is shown: {line}");
    assert_eq!(http.paths().last().map(String::as_str), Some("/v2/vectordb/collections/drop"));
}

#[test]
fn a_milvus_that_finds_nothing_in_the_collection_fails_after_a_few_tries() {
    let http = FakeHttp::start(|request| {
        if request.path.ends_with("/entities/search") {
            (200, json!({"code": 0, "data": []}).to_string())
        } else {
            healthy_answer(request, 768)
        }
    });
    let (sandbox, config) = claude_context(&http, &[]);

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(1));
    assert!(run.check("claude-context milvus").contains("found nothing"), "{}", run.stdout());
    let searches = http.paths().iter().filter(|path| path.ends_with("/entities/search")).count();
    assert_eq!(searches, 5);
}

#[test]
fn a_milvus_that_is_down_fails() {
    let http = FakeHttp::healthy(768);
    let unused = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let (sandbox, config) = claude_context(&http, &[("MILVUS_ADDRESS", &format!("127.0.0.1:{unused}"))]);

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(1));
    let line = run.check("claude-context milvus");
    assert!(line.contains("cannot create a test collection"), "{line}");
    assert!(line.contains("hint: start Milvus"), "{line}");
}

#[test]
fn a_milvus_address_that_is_not_loopback_fails_without_a_request() {
    let http = FakeHttp::healthy(768);
    let (sandbox, config) = claude_context(&http, &[("MILVUS_ADDRESS", "10.1.2.3:19530")]);

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(1));
    assert!(
        run.check("claude-context milvus")
            .contains("not a plain loopback address; uals keeps its backends on 127.0.0.1"),
        "{}",
        run.stdout()
    );
    assert!(!http.paths().iter().any(|path| path.starts_with("/v2/")));
}

#[test]
fn without_a_vector_length_the_milvus_probe_is_skipped() {
    let http = FakeHttp::healthy(768);
    let (sandbox, config) = claude_context(&http, &[("EMBEDDING_PROVIDER", ""), ("EMBEDDING_DIMENSION", "")]);

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(0), "{}", run.stdout());
    assert!(run.check("claude-context milvus").starts_with("- claude-context milvus: not probed"), "{}", run.stdout());
    assert!(http.paths().is_empty());
}

#[test]
fn the_dimension_of_the_config_is_used_when_the_embedder_is_not_probed() {
    let http = FakeHttp::healthy(768);
    let (sandbox, config) = claude_context(
        &http,
        &[("OPENAI_BASE_URL", "https://embeddings.example.com/v1"), ("EMBEDDING_DIMENSION", "384")],
    );

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(0), "{}", run.stdout());
    let create = http.requests().into_iter().find(|request| request.path.ends_with("/collections/create")).unwrap();
    assert_eq!(create.body["dimension"], 384);
}

// mcp

#[test]
fn uals_registered_at_the_top_level_is_ok() {
    let (sandbox, config) = without_semantic();
    sandbox.claude_json(&json!({"mcpServers": {"uals": {"type": "stdio", "command": "/x/uals", "args": ["mcp"]}}}));

    let run = sandbox.doctor(Some(&config), &[]);

    let line = run.check("mcp");
    assert!(line.starts_with("✓ mcp: uals is registered in "), "{line}");
    assert!(line.ends_with("(top level)"), "{line}");
}

#[test]
fn uals_registered_in_projects_says_how_many() {
    let (sandbox, config) = without_semantic();
    sandbox.claude_json(&json!({"projects": {
        "/p/one": {"mcpServers": {"uals": {}}},
        "/p/two": {"mcpServers": {"uals": {}}},
        "/p/three": {"mcpServers": {}},
    }}));

    let run = sandbox.doctor(Some(&config), &[]);

    assert!(run.check("mcp").ends_with("(2 projects)"), "{}", run.stdout());
    assert_eq!(run.code(), Some(0));
}

#[test]
fn uals_that_is_not_registered_is_a_warning_with_the_command_that_registers_it() {
    let (sandbox, config) = without_semantic();
    sandbox.claude_json(&json!({"mcpServers": {"other": {}}}));

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(0));
    let line = run.check("mcp");
    assert!(line.starts_with("! mcp: uals is not registered in "), "{line}");
    assert!(line.contains("\n    hint: in the uals checkout: scripts/configure-mcp.sh --uals-bin "), "{line}");
    assert!(line.ends_with("/uals"), "the hint names the binary that runs: {line}");
}

#[test]
fn a_missing_or_broken_claude_code_config_is_a_warning() {
    let (sandbox, config) = without_semantic();

    let missing = sandbox.doctor(Some(&config), &[]);
    std::fs::write(sandbox.path("claude.json"), "{not json").unwrap();
    let broken = sandbox.doctor(Some(&config), &[]);

    assert!(missing.check("mcp").contains("does not exist: uals is not registered"), "{}", missing.stdout());
    assert!(broken.check("mcp").contains("is not valid JSON"), "{}", broken.stdout());
    assert_eq!((missing.code(), broken.code()), (Some(0), Some(0)));
}

#[test]
fn a_direct_claude_context_entry_next_to_uals_is_a_warning_when_claude_context_is_the_backend() {
    let http = FakeHttp::healthy(768);
    let (sandbox, config) = claude_context(&http, &[]);
    sandbox.claude_json(&json!({"mcpServers": {
        "uals": {},
        "claude-context": {"env": {"OPENAI_API_KEY": "sk-in-claude-json"}},
    }}));

    let run = sandbox.doctor(Some(&config), &[]);

    assert_eq!(run.code(), Some(0));
    let line = run.check("mcp");
    assert!(line.starts_with("! mcp: uals is registered in "), "{line}");
    assert!(line.contains("two claude-context servers race on ~/.context"), "{line}");
    assert!(line.contains("hint: in the uals checkout: scripts/configure-mcp.sh --uals-bin /"), "{line}");
    assert!(line.ends_with("/uals --remove-claude-context"), "{line}");
    assert!(!run.stdout().contains("sk-in-claude-json"), "values are never printed");
}

#[test]
fn a_direct_claude_context_entry_in_a_project_counts_too() {
    let http = FakeHttp::healthy(768);
    let (sandbox, config) = claude_context(&http, &[]);
    sandbox.claude_json(&json!({
        "mcpServers": {"uals": {}},
        "projects": {"/p/one": {"mcpServers": {"claude-context": {}}}},
    }));

    let run = sandbox.doctor(Some(&config), &[]);

    assert!(run.check("mcp").contains("two claude-context servers race"), "{}", run.stdout());
}

#[test]
fn a_direct_claude_context_entry_is_fine_when_the_backend_is_another() {
    let (sandbox, config) = without_semantic();
    sandbox.claude_json(&json!({"mcpServers": {"uals": {}, "claude-context": {}}}));

    let run = sandbox.doctor(Some(&config), &[]);

    assert!(run.check("mcp").starts_with("✓ mcp: "), "{}", run.stdout());
}

// output

#[test]
fn json_output_has_the_checks_in_order_and_the_verdict() {
    let http = FakeHttp::healthy(1024);
    let (sandbox, config) = claude_context(&http, &[]);
    sandbox.claude_json(&json!({"mcpServers": {"uals": {}}}));

    let run = sandbox.doctor(Some(&config), &["--format", "json"]);

    assert_eq!(run.code(), Some(1));
    let report: Value = serde_json::from_str(&run.stdout()).unwrap();
    assert_eq!(report["ok"], false);
    let checks = report["checks"].as_array().unwrap();
    let names: Vec<&str> = checks.iter().map(|check| check["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "config",
            "ast-index",
            "claude-context node",
            "claude-context package",
            "claude-context embedder",
            "claude-context milvus",
            "lsp",
            "mcp"
        ]
    );
    let statuses: Vec<&str> = checks.iter().map(|check| check["status"].as_str().unwrap()).collect();
    assert_eq!(statuses, ["ok", "ok", "ok", "ok", "fail", "ok", "warn", "ok"]);
    assert!(checks[0].get("hint").is_none(), "an ok check has no hint");
    assert!(checks[4]["hint"].as_str().unwrap().contains("EMBEDDING_DIMENSION=1024"));
    assert!(checks[4]["detail"].as_str().unwrap().contains("1024"));
}

#[test]
fn json_output_of_a_healthy_setup_says_ok() {
    let (sandbox, config) = without_semantic();
    sandbox.claude_json(&json!({"mcpServers": {"uals": {}}}));

    let run = sandbox.doctor(Some(&config), &["--format", "json"]);

    assert_eq!(run.code(), Some(0));
    let report: Value = serde_json::from_str(&run.stdout()).unwrap();
    assert_eq!(report["ok"], true);
}

// live

/// The real semble (0.7.3 or the one on PATH) through the real adapter. Ignored by default, like
/// `semble_live`; a missing `semble` fails it. The model comes from the real Hugging Face cache.
#[test]
#[ignore = "live: needs `semble` on PATH"]
fn the_real_semble_answers_a_search() {
    let semble = which_semble();
    let sandbox = Sandbox::new();
    std::os::unix::fs::symlink(&semble, sandbox.path("bin/semble")).unwrap();
    sandbox.versioned("ast-index", "ast-index 3.56.0");
    let config = sandbox.config("[semantic]\ndefault = \"semble\"\n\n[semantic.semble]\npass_env = [\"HF_HOME\", \"HF_ENDPOINT\", \"HF_HUB_OFFLINE\"]\n");
    let huggingface = std::env::var_os("HF_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| real_home().join(".cache/huggingface"));

    let run = sandbox.doctor_with(Some(&config), &[], &[("HF_HOME", &huggingface)]);

    assert_eq!(run.code(), Some(0), "{}", run.stdout());
    let line = run.check("semble");
    assert!(line.starts_with("✓ semble: "), "{line}");
}

fn which_semble() -> std::path::PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|folder| folder.join("semble"))
        .find(|path| path.is_file())
        .expect("live test needs `semble` on PATH (install Semble 0.7.x)")
}

fn real_home() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"))
}
