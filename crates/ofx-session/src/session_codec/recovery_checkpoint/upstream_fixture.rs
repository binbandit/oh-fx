pub(crate) const PNG_DATA: &str = "iVBORw0KGgoAAAANSUhEUg==";
pub(crate) const PNG_SHA256: &str =
    "02a3e298f1533f62558c58e4c70edcab9af5a50d62d925fd5390942020fb0fb8";
pub(crate) const CONFIGURED_IDENTITY: &str =
    "40122b758656199048961e6e8369383c25ebcdeddced75b64ad736e527014da8";
pub(crate) const EDIT_HANDLE: &str = "result-edit_file-0123456789abcdef-fedcba9876543210.txt";
pub(crate) const REPLAY_HANDLE: &str =
    "fx-command-replay-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.bin";

pub(crate) fn image_user() -> String {
    format!(
        r#"{{"text":"fix the build","images":[{{"id":1,"path":"/Users/me/shot.png","media_type":"image/png","snapshot_path":"images/shot-1.png","snapshot_sha256":"{PNG_SHA256}"}}]}}"#
    )
}

pub(crate) fn presentation() -> &'static str {
    r#"{"path":"a.rs","kind":"edited","lines":[{"kind":"deletion","old_line":1,"new_line":null,"text":"old"},{"kind":"addition","old_line":null,"new_line":1,"text":"new"}],"additions":1,"deletions":1,"truncated":false,"previous_content":"old\n","after_content":"new\n","lifecycle_id":{"turn_id":7,"call_id":"call_edit"}}"#
}

pub(crate) fn fields_step() -> String {
    let presentation = presentation();
    let calls = [
        r#"{"id":"call_edit","name":"edit_file","arguments_json":"{\"path\":\"a.rs\"}","provider_result":null}"#,
        r#"{"id":"call_ls","name":"shell","arguments_json":"{\"command\":\"ls\"}","provider_result":null}"#,
        r#"{"id":"call_search","name":"web_search","arguments_json":"{\"query\":\"zig\"}","provider_result":"{\"results\":[]}"}"#,
        r#"{"id":"call_shot","name":"screenshot","arguments_json":"{}","provider_result":null}"#,
    ]
    .join(",");
    let results = [
        format!(
            r#"{{"tool_call_id":"call_edit","tool_name":"edit_file","status":"success","output":"","output_handle":"{EDIT_HANDLE}","preview":"Edited a.rs","output_bytes":5000,"stored_output_bytes":5000,"truncated":true,"provider_native":false,"review_feedback":false,"created_at_ms":1700000000001,"permission_feedback":[],"committed_file_presentation":{presentation},"command_output_replay":null,"command_process_presentation":null,"terminal_action_presentation":null}}"#
        ),
        format!(
            r#"{{"tool_call_id":"call_ls","tool_name":"shell","status":"success","output":"a.rs\n","output_handle":null,"preview":null,"output_bytes":5,"stored_output_bytes":5,"truncated":false,"provider_native":false,"review_feedback":false,"created_at_ms":1700000000002,"permission_feedback":[],"committed_file_presentation":null,"command_output_replay":{{"kind":"available","handle":"{REPLAY_HANDLE}","framed_bytes":22}},"command_process_presentation":{{"kind":"exit_code","value":0}},"terminal_action_presentation":null}}"#
        ),
        r#"{"tool_call_id":"call_search","tool_name":"web_search","status":"success","output":"{\"results\":[]}","output_handle":null,"preview":null,"output_bytes":14,"stored_output_bytes":14,"truncated":false,"provider_native":true,"review_feedback":false,"created_at_ms":1700000000003,"permission_feedback":[],"committed_file_presentation":null,"command_output_replay":null,"command_process_presentation":null,"terminal_action_presentation":null}"#.to_owned(),
        format!(
            r#"{{"tool_call_id":"call_shot","tool_name":"screenshot","status":"success","output":"Captured.","output_handle":null,"preview":null,"output_bytes":9,"stored_output_bytes":9,"truncated":false,"provider_native":false,"review_feedback":false,"created_at_ms":1700000000004,"permission_feedback":[],"committed_file_presentation":null,"command_output_replay":null,"command_process_presentation":null,"terminal_action_presentation":null,"tool_images":[{{"type":"image","mimeType":"image/png","data":"{PNG_DATA}"}}]}}"#
        ),
    ]
    .join(",");
    format!(
        r#"{{"assistant":"Editing.","provider_replay":null,"tool_calls":[{calls}],"tool_results":[{results}]}}"#
    )
}

pub(crate) fn fields_checkpoint() -> String {
    let user = image_user();
    let step = fields_step();
    format!(
        r#"{{"version":2,"turn_id":7,"user":{user},"assistant_source":"Looking at","execution":{{"schema_version":10,"tool_steps":[{step}],"files":[],"steering":[],"turn_summary":null}},"cause":"rate_limited","action":"retrying_request","tool_state":"confirmed","authority":{{"provider":"gateway","model":"openai/gpt-5","credential_source":"configured","credential_identity":"{CONFIGURED_IDENTITY}"}},"requested_fast_mode":false,"fast_mode":false,"max_provider_attempts":10,"consumed_provider_attempts":1,"outstanding_reservation":false}}"#
    )
}
