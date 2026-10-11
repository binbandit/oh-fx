use std::fs;
use std::sync::Arc;

use ofx_agent::{Agent, AgentConfig};
use ofx_contract::{
    AutoCompactPercent, BoxFuture, CapabilityLookup, CapabilityResolver, ImageAttachment,
    ImageInputSupport, ModelCapabilities, PermissionMode, TurnOutcome,
};
use ofx_gateway::{GatewayCredential, GatewayEndpoints, GatewayProvider};
use ofx_permissions::PermissionPolicy;
use ofx_testkit::{FakeServer, Reply};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::context::{GATEWAY_SYSTEM_PROMPT, HostRuntimeContext};

const MODEL: &str = "openai/gpt-5.6-sol";
const LARGE_PNG_BYTES: usize = 3 * 1024 * 1024;
const CONTEXT_WINDOW: u32 = 128_000;

struct Vision;

impl CapabilityResolver for Vision {
    fn resolve<'a>(
        &'a self,
        _model: &'a str,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, CapabilityLookup> {
        Box::pin(async {
            CapabilityLookup::Resolved(ModelCapabilities {
                context_window: Some(CONTEXT_WINDOW),
                image_input_support: ImageInputSupport::Native,
                ..ModelCapabilities::default()
            })
        })
    }
}

fn large_png() -> ImageAttachment {
    let mut bytes =
        b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\x03\xe8\0\0\x03\xe8\x08\x06\0\0\0\0\0\0\0".to_vec();
    bytes.resize(LARGE_PNG_BYTES, 0);
    ImageAttachment {
        id: 1,
        path: "shot.png".to_owned(),
        media_type: "image/png".to_owned(),
        snapshot_sha256: Some(ofx_text::lowercase_hex(&Sha256::digest(&bytes))),
        inline_data: Some(bytes),
        ..ImageAttachment::default()
    }
}

fn reply() -> Reply {
    Reply::sse(&[
        r#"{"type":"text-delta","id":"t","delta":"a picture"}"#,
        r#"{"type":"finish","finishReason":{"unified":"stop"},"usage":{"inputTokens":{"total":1200},"outputTokens":{"total":3}}}"#,
    ])
}

fn agent(server: &FakeServer, workspace: &std::path::Path) -> Agent {
    let provider = GatewayProvider::new(
        GatewayCredential::new(Some("vck-measure-0123456789".to_owned()), None),
        "oh-fx/test",
        GatewayEndpoints {
            chat: format!("http://{}/v4/ai/language-model", server.address()),
        },
    )
    .expect("build the gateway provider");
    let workspace = fs::canonicalize(workspace).expect("canonicalize the workspace");
    Agent::new(
        Arc::new(provider),
        Vec::new(),
        Arc::new(HostRuntimeContext::new(
            workspace.clone(),
            PermissionMode::Auto,
            false,
        )),
        Arc::new(PermissionPolicy::new(PermissionMode::Auto, workspace)),
        AgentConfig {
            model: MODEL.to_owned(),
            system_prompt: GATEWAY_SYSTEM_PROMPT.to_owned(),
            max_output_tokens: None,
            step_limit: 0,
            reasoning_effort: None,
            fast_mode: false,
            ultrafast_mode: false,
            auto_compact_percent: AutoCompactPercent::resolve(None, None),
        },
    )
    .with_capability_resolver(Arc::new(Vision))
}

fn image_payload(body: &Value) -> Option<&str> {
    body["prompt"]
        .as_array()?
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .find(|part| part["type"] == "file")?["data"]["data"]
        .as_str()
}

#[tokio::test]
async fn a_large_image_reaches_the_gateway_when_the_context_window_is_known() {
    let server = FakeServer::start([reply()]);
    let workspace = tempfile::tempdir().expect("create a workspace");
    let mut agent = agent(&server, workspace.path());

    let report = agent
        .run_turn_with_images(
            "what is this",
            vec![large_png()],
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await;

    assert_eq!(report.outcome, TurnOutcome::Completed, "{report:?}");
    assert_eq!(report.final_text, "a picture");
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let body = requests[0].json();
    let payload = image_payload(&body).expect("the image file part");
    assert_eq!(payload.len(), LARGE_PNG_BYTES / 3 * 4);
    assert!(payload.starts_with("iVBORw0KGgoAAAANSUhEUgAAA+gAAAPo"));
}
