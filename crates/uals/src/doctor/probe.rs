//! The HTTP probes of `uals doctor`: one embedding call to a local embedder, and one round trip
//! (create, insert, search, drop) through a collection of its own in a local Milvus. Only plain
//! `http://` to 127.0.0.1, localhost or ::1 is probed: uals sends nothing to remote hosts.
//!
//! ureq blocks, so every probe runs on a blocking thread.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};

use super::Check;

const EMBEDDER: &str = "claude-context embedder";
const MILVUS: &str = "claude-context milvus";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// An embedding call may load the model first.
const EMBEDDING_TIMEOUT: Duration = Duration::from_secs(60);
const MILVUS_TIMEOUT: Duration = Duration::from_secs(30);
/// The search of a collection that was just filled may find nothing for a moment.
const SEARCH_ATTEMPTS: u32 = 5;
const SEARCH_PAUSE: Duration = Duration::from_secs(2);
const LOCAL_ONLY: &str = "doctor probes local endpoints only";
const DEFAULT_OLLAMA: &str = "http://127.0.0.1:11434";
const DEFAULT_OPENAI: &str = "https://api.openai.com/v1";
const DEFAULT_OPENROUTER: &str = "https://openrouter.ai/api/v1";

/// `url` as a plain `http://` base without a trailing slash, when it points to this machine
/// (127.0.0.1, localhost or ::1). A URL without a scheme counts as `http://`. Anything else,
/// `https://` and a `user@` part included, is `None`.
fn local_base(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = match url.split_once("://") {
        Some(("http", rest)) => rest,
        Some(_) => return None,
        None => url,
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.contains('@') {
        return None;
    }
    let host = match authority.strip_prefix('[') {
        Some(inside) => inside.split_once(']')?.0,
        None => authority.rsplit_once(':').map_or(authority, |(host, _)| host),
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1").then(|| format!("http://{}", rest.trim_end_matches('/')))
}

/// The embedding call to make and how to read the answer.
#[derive(Debug, PartialEq)]
struct EmbeddingCall {
    url: String,
    /// The `Authorization: Bearer` key, when the config has one. Never printed.
    bearer: Option<String>,
    body: Value,
    answer: AnswerShape,
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum AnswerShape {
    /// `{"embedding": [...]}`
    Ollama,
    /// `{"data": [{"embedding": [...]}]}`
    OpenAi,
}

impl AnswerShape {
    /// The length of the vector in `answer`.
    fn length(self, answer: &Value) -> Option<usize> {
        let vector = match self {
            Self::Ollama => answer.get("embedding"),
            Self::OpenAi => answer.get("data")?.get(0)?.get("embedding"),
        };
        Some(vector?.as_array()?.len())
    }
}

/// What to do about the embedder settings in `env`.
#[derive(Debug, PartialEq)]
enum EmbeddingPlan {
    Call(EmbeddingCall),
    Skip(String),
}

fn plan_embedding(env: &BTreeMap<String, String>) -> EmbeddingPlan {
    let setting = |name: &str| env.get(name).map(String::as_str).filter(|value| !value.is_empty());
    let Some(provider) = setting("EMBEDDING_PROVIDER") else {
        return EmbeddingPlan::Skip("EMBEDDING_PROVIDER is not set".to_owned());
    };
    let Some(model) = setting("EMBEDDING_MODEL") else {
        return EmbeddingPlan::Skip("EMBEDDING_MODEL is not set".to_owned());
    };
    let (base, tail, body, bearer, answer) = match provider {
        "Ollama" => (
            setting("OLLAMA_HOST").unwrap_or(DEFAULT_OLLAMA),
            "api/embeddings",
            json!({"model": model, "prompt": "test"}),
            None,
            AnswerShape::Ollama,
        ),
        "OpenAI" | "OpenRouter" => {
            let (default_base, key) = if provider == "OpenAI" {
                (DEFAULT_OPENAI, setting("OPENAI_API_KEY"))
            } else {
                (DEFAULT_OPENROUTER, setting("OPENROUTER_API_KEY"))
            };
            (
                setting("OPENAI_BASE_URL").unwrap_or(default_base),
                "embeddings",
                json!({"model": model, "input": "test"}),
                key.map(str::to_owned),
                AnswerShape::OpenAi,
            )
        }
        other => return EmbeddingPlan::Skip(format!("{LOCAL_ONLY}; provider {other} is not probed")),
    };
    match local_base(base) {
        Some(base) => EmbeddingPlan::Call(EmbeddingCall { url: format!("{base}/{tail}"), bearer, body, answer }),
        None => EmbeddingPlan::Skip(format!("{LOCAL_ONLY}; {base} is not a plain http address on this machine")),
    }
}

/// An HTTP agent that ignores proxy variables, with short connect and total timeouts.
fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .proxy(None)
        .build()
        .into()
}

/// POSTs `body` as JSON; the status and the JSON answer (a text answer becomes a JSON string).
fn post_json(agent: &ureq::Agent, url: &str, bearer: Option<&str>, body: &Value) -> Result<(u16, Value), String> {
    let mut request = agent.post(url);
    if let Some(key) = bearer {
        request = request.header("Authorization", format!("Bearer {key}"));
    }
    let response = request.send_json(body).map_err(|error| error.to_string())?;
    let status = response.status().as_u16();
    let text = response.into_body().read_to_string().map_err(|error| error.to_string())?;
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::String(text))))
}

/// The first line of an error answer, cut to a length that fits a report line.
fn short(answer: &Value) -> String {
    let text = match answer {
        Value::String(text) => text.clone(),
        other => {
            other.get("message").or_else(|| other.get("error")).map_or_else(|| other.to_string(), Value::to_string)
        }
    };
    text.lines().next().unwrap_or("").chars().take(200).collect()
}

/// The embedder check, and the length of the vector it returned.
pub(super) async fn check_embedder(env: &BTreeMap<String, String>) -> (Check, Option<usize>) {
    let call = match plan_embedding(env) {
        EmbeddingPlan::Skip(reason) => return (Check::skip(EMBEDDER, reason), None),
        EmbeddingPlan::Call(call) => call,
    };
    let url = call.url.clone();
    let answer = tokio::task::spawn_blocking(move || {
        post_json(&agent(EMBEDDING_TIMEOUT), &call.url, call.bearer.as_deref(), &call.body)
            .map(|(status, answer)| (status, call.answer.length(&answer), answer))
    })
    .await;
    let model = env.get("EMBEDDING_MODEL").map_or("", String::as_str);
    let hint = "start the embedder, or fix OLLAMA_HOST or OPENAI_BASE_URL";
    let (status, length, answer) = match answer {
        Ok(Ok(answered)) => answered,
        Ok(Err(error)) => return (Check::fail(EMBEDDER, format!("cannot reach {url}: {error}"), hint), None),
        Err(error) => return (Check::fail(EMBEDDER, format!("the probe stopped: {error}"), hint), None),
    };
    let Some(length) = length.filter(|_| (200..300).contains(&status)) else {
        let reason = format!("{url} answered {status} without a vector: {}", short(&answer));
        return (Check::fail(EMBEDDER, reason, "check EMBEDDING_MODEL and the key"), None);
    };
    let configured = env.get("EMBEDDING_DIMENSION").filter(|value| !value.is_empty());
    let check = match configured.map(|value| value.parse::<usize>()) {
        Some(Ok(dimension)) if dimension != length => Check::fail(
            EMBEDDER,
            format!("model {model} returns {length} values but EMBEDDING_DIMENSION is {dimension}"),
            format!("set EMBEDDING_DIMENSION={length}; with a wrong value every insert into Milvus fails"),
        ),
        Some(Err(_)) => Check::fail(
            EMBEDDER,
            format!("EMBEDDING_DIMENSION is not a number; model {model} returns {length} values"),
            format!("set EMBEDDING_DIMENSION={length}"),
        ),
        Some(Ok(_)) => {
            Check::ok(EMBEDDER, format!("model {model} at {url} returns {length} values, as EMBEDDING_DIMENSION says"))
        }
        None => Check::ok(
            EMBEDDER,
            format!("model {model} at {url} returns {length} values; EMBEDDING_DIMENSION is not set"),
        ),
    };
    (check, Some(length))
}

/// The Milvus check. `dimension` is the vector length of the test collection: the embedder's, else
/// `EMBEDDING_DIMENSION`; without one the probe is skipped.
pub(super) async fn check_milvus(env: &BTreeMap<String, String>, dimension: Option<usize>) -> Check {
    let Some(address) = env.get("MILVUS_ADDRESS").filter(|value| !value.is_empty()) else {
        return Check::fail(
            MILVUS,
            "MILVUS_ADDRESS is not set",
            "set MILVUS_ADDRESS=127.0.0.1:19530 in semantic.claude_context.env",
        );
    };
    let Some(base) = local_base(address) else {
        return Check::fail(
            MILVUS,
            format!("MILVUS_ADDRESS {address} is not a plain loopback address; uals keeps its backends on 127.0.0.1"),
            "set MILVUS_ADDRESS=127.0.0.1:19530",
        );
    };
    let Some(dimension) = dimension.or_else(|| env.get("EMBEDDING_DIMENSION")?.parse().ok()) else {
        return Check::skip(
            MILVUS,
            "not probed: no vector length, the embedder did not answer and EMBEDDING_DIMENSION is not set",
        );
    };
    let outcome = tokio::task::spawn_blocking(move || milvus_round_trip(&base, dimension)).await;
    match outcome {
        Ok(Ok(note)) => {
            Check::ok(MILVUS, format!("{address} accepted a test collection at dimension {dimension}{note}"))
        }
        Ok(Err(failure)) => Check::fail(MILVUS, format!("{address}: {}", failure.detail), failure.hint),
        Err(error) => Check::fail(MILVUS, format!("the probe stopped: {error}"), "run uals doctor again"),
    }
}

struct Failure {
    detail: String,
    hint: &'static str,
}

/// Creates a collection, inserts three vectors, searches and drops the collection, always.
/// Ok carries a note about a collection that could not be dropped (empty when it was).
fn milvus_round_trip(base: &str, dimension: usize) -> Result<String, Failure> {
    let agent = agent(MILVUS_TIMEOUT);
    let collection = format!("uals_doctor_{}", std::process::id());
    let call = |path: &str, body: Value| -> Result<Value, String> {
        let (status, answer) = post_json(&agent, &format!("{base}/v2/vectordb/{path}"), None, &body)?;
        let code = answer.get("code").and_then(Value::as_i64);
        if (200..300).contains(&status) && matches!(code, None | Some(0 | 200)) {
            Ok(answer)
        } else {
            Err(format!("answered {status}: {}", short(&answer)))
        }
    };
    let start = "start Milvus, or fix MILVUS_ADDRESS";
    call(
        "collections/create",
        json!({"collectionName": collection, "dimension": dimension, "metricType": "COSINE", "autoID": true}),
    )
    .map_err(|error| Failure { detail: format!("cannot create a test collection: {error}"), hint: start })?;

    let vector = vec![0.1_f32; dimension];
    let tested = (|| {
        call(
            "entities/insert",
            json!({"collectionName": collection, "data": [{"vector": vector}, {"vector": vector}, {"vector": vector}]}),
        )
        .map_err(|error| Failure {
            detail: format!("rejected an insert at dimension {dimension}: {error}"),
            hint: "EMBEDDING_DIMENSION must equal the length of the model's vectors",
        })?;
        let search = json!({
            "collectionName": collection, "data": [vector], "annsField": "vector", "limit": 3,
            "consistencyLevel": "Strong",
        });
        for attempt in 1..=SEARCH_ATTEMPTS {
            let found = call("entities/search", search.clone())
                .map_err(|error| Failure { detail: format!("search failed: {error}"), hint: start })?;
            if found.get("data").and_then(Value::as_array).is_some_and(|hits| !hits.is_empty()) {
                return Ok(());
            }
            if attempt < SEARCH_ATTEMPTS {
                std::thread::sleep(SEARCH_PAUSE);
            }
        }
        Err(Failure { detail: "a search of the inserted vectors found nothing".to_owned(), hint: start })
    })();

    let dropped = call("collections/drop", json!({"collectionName": collection}));
    tested?;
    Ok(match dropped {
        Ok(_) => String::new(),
        Err(error) => format!("; could not drop {collection}: {error}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    #[test]
    fn only_plain_http_to_this_machine_is_probed() {
        let local = [
            ("http://127.0.0.1:1234/v1/", "http://127.0.0.1:1234/v1"),
            ("localhost:11434", "http://localhost:11434"),
            ("http://[::1]:9/x", "http://[::1]:9/x"),
            ("127.0.0.1", "http://127.0.0.1"),
        ];
        for (url, base) in local {
            assert_eq!(local_base(url).as_deref(), Some(base), "{url}");
        }
        let remote = [
            "https://127.0.0.1:1234",
            "http://example.com",
            "http://127.0.0.1@example.com/v1",
            "http://127.0.0.1:80@example.com/v1",
            "http://localhost.example.com",
            "http://10.0.0.5:19530",
            "ftp://127.0.0.1",
        ];
        for url in remote {
            assert_eq!(local_base(url), None, "{url}");
        }
    }

    #[test]
    fn the_embedding_call_follows_the_provider() {
        let ollama = plan_embedding(&env(&[("EMBEDDING_PROVIDER", "Ollama"), ("EMBEDDING_MODEL", "nomic")]));
        let openai = plan_embedding(&env(&[
            ("EMBEDDING_PROVIDER", "OpenAI"),
            ("EMBEDDING_MODEL", "m"),
            ("OPENAI_BASE_URL", "http://127.0.0.1:1234/v1"),
            ("OPENAI_API_KEY", "key"),
        ]));

        assert_eq!(
            ollama,
            EmbeddingPlan::Call(EmbeddingCall {
                url: "http://127.0.0.1:11434/api/embeddings".into(),
                bearer: None,
                body: json!({"model": "nomic", "prompt": "test"}),
                answer: AnswerShape::Ollama,
            })
        );
        assert_eq!(
            openai,
            EmbeddingPlan::Call(EmbeddingCall {
                url: "http://127.0.0.1:1234/v1/embeddings".into(),
                bearer: Some("key".into()),
                body: json!({"model": "m", "input": "test"}),
                answer: AnswerShape::OpenAi,
            })
        );
    }

    #[test]
    fn a_provider_or_address_that_is_not_local_is_skipped() {
        let skipped = |pairs: &[(&str, &str)]| matches!(plan_embedding(&env(pairs)), EmbeddingPlan::Skip(_));
        assert!(skipped(&[]));
        assert!(skipped(&[("EMBEDDING_PROVIDER", "Gemini"), ("EMBEDDING_MODEL", "m")]));
        assert!(skipped(&[("EMBEDDING_PROVIDER", "OpenAI"), ("EMBEDDING_MODEL", "m")]), "the default base is remote");
        assert!(skipped(&[("EMBEDDING_PROVIDER", "Ollama")]), "no model");
    }
}
