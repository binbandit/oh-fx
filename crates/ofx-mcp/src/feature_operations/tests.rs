use std::sync::atomic::AtomicU64;
use std::time::Duration;

use super::*;
use crate::features::common::ResourceData;
use crate::mcp_contract::{EnvVar, McpServerConfig};
use crate::server_transport::ConnectOptions;
use crate::test_support::{ScriptedAnswers, operation_elicitation};
use crate::transport::ShutdownMode;

const FEATURE_SERVER: &str = r#"reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
answer() {
  calls=$((calls + 1))
  if [ "$calls" -gt "${ROUNDS:-1}" ]; then
    reply "$1" "$2"
    return
  fi
  case "$MODE" in
    elicit)
      printf '{"jsonrpc":"2.0","id":"e-1","method":"elicitation/create","params":{"message":"Who?","requestedSchema":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}}}\n'
      IFS= read -r answered
      printf '%s\n' "$answered" > "$STATE/answer"
      reply "$1" "$2" ;;
    bad)
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32042,"message":"URL required","data":{"elicitations":[]}}}\n' "$1" ;;
    *)
      if [ "$NOTIFY" = 1 ]; then
        printf '{"jsonrpc":"2.0","method":"notifications/elicitation/complete","params":{"elicitationId":"e1"}}\n'
      fi
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32042,"message":"URL required","data":{"elicitations":[{"mode":"url","message":"Connect","url":"https://example.test/connect","elicitationId":"e1"}]}}}\n' "$1" ;;
  esac
}
calls=0
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      reply "$id" "{\"protocolVersion\":\"${VERSION:-2025-11-25}\",\"capabilities\":{\"resources\":{},\"prompts\":{}},\"serverInfo\":{\"name\":\"fixture\",\"version\":\"1\"}}" ;;
    *'"method":"tools/list"'*) reply "$id" '{"tools":[]}' ;;
    *'"method":"resources/list"'*) reply "$id" '{"resources":[{"uri":"memory://notes","name":"notes"}]}' ;;
    *'"method":"resources/read"'*) answer "$id" '{"contents":[{"uri":"memory://notes","text":"hello"}]}' ;;
    *'"method":"prompts/list"'*) reply "$id" '{"prompts":[{"name":"review"}]}' ;;
    *'"method":"prompts/get"'*) answer "$id" '{"messages":[{"role":"user","content":{"type":"text","text":"go"}}]}' ;;
  esac
done
"#;

async fn fixture(
    environment: &[(&str, &str)],
    answers: Option<&Arc<ScriptedAnswers>>,
) -> (Arc<Server>, tempfile::TempDir) {
    let state = tempfile::tempdir().unwrap();
    let mut config = McpServerConfig::stdio(
        "fixture",
        "/bin/sh",
        vec!["-c".to_owned(), FEATURE_SERVER.to_owned()],
    );
    config.env.push(EnvVar {
        key: "STATE".to_owned(),
        value: state.path().to_string_lossy().into_owned(),
    });
    config
        .env
        .extend(environment.iter().map(|(key, value)| EnvVar {
            key: (*key).to_owned(),
            value: (*value).to_owned(),
        }));
    let options = ConnectOptions {
        elicitation: answers.map(|answers| operation_elicitation(answers).responder),
        ..ConnectOptions::default()
    };
    let server = Arc::new(Server::new(config, options, Arc::new(AtomicU64::new(0))));
    server.start().await.unwrap();
    (server, state)
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

async fn read(
    environment: &[(&str, &str)],
    answers: Option<&Arc<ScriptedAnswers>>,
    input: FeatureInput,
) -> Result<Arc<[ResourceContent]>, FeatureFailure> {
    let (server, _state) = fixture(environment, answers).await;
    let read = server
        .read_resource("memory://notes", deadline(), input)
        .await;
    server.stop(ShutdownMode::Immediate).await;
    read
}

fn hello(read: &Result<Arc<[ResourceContent]>, FeatureFailure>) -> bool {
    matches!(read, Ok(contents) if matches!(&contents[0].data, ResourceData::Text(text) if text == "hello"))
}

#[tokio::test]
async fn a_url_required_read_is_input_required_unless_it_may_ask() {
    let answers = ScriptedAnswers::new(&["Open URL"]);
    assert_eq!(
        read(&[("NOTIFY", "1")], Some(&answers), FeatureInput::Withheld).await,
        Err(FeatureFailure::Error(McpError::McpInputRequired))
    );
    assert!(answers.questions().is_empty());
    assert_eq!(
        read(&[("NOTIFY", "1")], None, FeatureInput::Ask).await,
        Err(FeatureFailure::Error(McpError::McpInputRequired))
    );
    let asked = read(&[("NOTIFY", "1")], Some(&answers), FeatureInput::Ask).await;
    assert!(hello(&asked), "{asked:?}");
    assert_eq!(answers.questions().len(), 1);
}

#[tokio::test]
async fn a_url_required_prompt_is_retried_once_its_flow_is_completed() {
    let answers = ScriptedAnswers::new(&["Open URL", "I completed it / Retry"]);
    let (server, _state) = fixture(&[], Some(&answers)).await;
    let prompt = server
        .get_prompt("review", "{}", deadline(), FeatureInput::Ask)
        .await;
    server.stop(ShutdownMode::Immediate).await;
    assert_eq!(prompt.unwrap().messages.len(), 1);
    assert_eq!(answers.questions().len(), 2);
    let declined = ScriptedAnswers::new(&["Decline"]);
    let (server, _state) = fixture(&[], Some(&declined)).await;
    let prompt = server
        .get_prompt("review", "{}", deadline(), FeatureInput::Ask)
        .await;
    server.stop(ShutdownMode::Immediate).await;
    assert_eq!(
        prompt,
        Err(FeatureFailure::Error(McpError::McpInputRequired))
    );
}

#[tokio::test]
async fn malformed_or_older_url_required_errors_fail_as_upstream_reports_them() {
    let answers = ScriptedAnswers::new(&[]);
    assert!(matches!(
        read(&[("MODE", "bad")], Some(&answers), FeatureInput::Ask).await,
        Err(FeatureFailure::Error(McpError::McpProtocolError(_)))
    ));
    assert_eq!(
        read(&[("VERSION", "2025-06-18")], Some(&answers), FeatureInput::Ask).await,
        Err(FeatureFailure::Diagnostic(
            "MCP protocol error -32042: URL required; data={\"elicitations\":[{\"mode\":\"url\",\"message\":\"Connect\",\"url\":\"https://example.test/connect\",\"elicitationId\":\"e1\"}]}".to_owned()
        ))
    );
    assert!(answers.questions().is_empty());
}

#[tokio::test]
async fn a_feature_read_is_retried_at_most_eight_times() {
    let answers = ScriptedAnswers::new(&["Open URL"; 8]);
    assert!(hello(
        &read(
            &[("NOTIFY", "1"), ("ROUNDS", "8")],
            Some(&answers),
            FeatureInput::Ask
        )
        .await
    ));
    let answers = ScriptedAnswers::new(&["Open URL"; 8]);
    assert_eq!(
        read(
            &[("NOTIFY", "1"), ("ROUNDS", "9")],
            Some(&answers),
            FeatureInput::Ask
        )
        .await,
        Err(FeatureFailure::Error(
            McpError::McpInputRequiredLimitExceeded
        ))
    );
    assert_eq!(answers.questions().len(), 8);
}

#[tokio::test]
async fn a_form_during_a_feature_read_is_asked_only_when_the_read_may_ask() {
    let answers = ScriptedAnswers::new(&["Ada", "Submit"]);
    let (server, state) = fixture(&[("MODE", "elicit")], Some(&answers)).await;
    let asked = server
        .read_resource("memory://notes", deadline(), FeatureInput::Ask)
        .await;
    assert!(hello(&asked), "{asked:?}");
    assert_eq!(
        std::fs::read_to_string(state.path().join("answer")).unwrap(),
        "{\"jsonrpc\":\"2.0\",\"id\":\"e-1\",\"result\":{\"action\":\"accept\",\"content\":{\"name\":\"Ada\"}}}\n"
    );
    server.stop(ShutdownMode::Immediate).await;
    let (server, state) = fixture(&[("MODE", "elicit")], Some(&answers)).await;
    let withheld = server
        .read_resource("memory://notes", deadline(), FeatureInput::Withheld)
        .await;
    assert!(hello(&withheld), "{withheld:?}");
    assert_eq!(
        std::fs::read_to_string(state.path().join("answer")).unwrap(),
        "{\"jsonrpc\":\"2.0\",\"id\":\"e-1\",\"error\":{\"code\":-32601,\"message\":\"Method not found\"}}\n"
    );
    server.stop(ShutdownMode::Immediate).await;
}
