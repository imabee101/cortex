// Test clients talk to loopback mocks only.
#![allow(clippy::disallowed_methods)]
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cortex_api::jwt::b64url;
use futures_util::SinkExt;
use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const DB: &str = "postgres://127.0.0.1/cortex_api_phase4";
const BRAVE: &str = "phase4-brave-token";

#[tokio::test]
async fn phase4_inventory_routes() {
    let llama = mock_llama().await;
    let brave = mock_brave().await;
    let probe = reqwest::Client::new();
    for _ in 0..50 {
        if probe
            .get(format!("{llama}/props"))
            .send()
            .await
            .ok()
            .is_some_and(|response| response.status().is_success())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    truncate();
    let mut config = cortex_api::Config::new("127.0.0.1:0".parse().expect("addr"), DB.to_owned());
    config.llama_url = Some(llama);
    config.embed_url = config.llama_url.clone();
    config.embed_model = Some("embed-small".to_owned());
    config.embed_dimensions = 768;
    let dist = std::env::temp_dir().join(format!("cortex-dist-{}", std::process::id()));
    std::fs::create_dir_all(&dist).expect("dist dir");
    std::fs::write(dist.join("install.sh"), "#!/bin/bash\necho installer\n").expect("script");
    std::fs::write(dist.join("cortex-9.9.9-linux-x86_64"), b"0123456789").expect("build");
    std::fs::write(dist.join("secret.txt"), "hidden").expect("other");
    std::fs::create_dir_all(dist.join("changelogs")).expect("changelog dir");
    std::fs::write(dist.join("changelogs/9.9.9.external.md"), "- notes\n").expect("notes");
    std::fs::write(
        dist.join("changelogs/9.9.9.external.json"),
        r#"[{"category":"features","description":"x","breaking_change":false}]"#,
    )
    .expect("notes json");
    config.dist_dir = dist.clone();
    config.brave_token = Some(BRAVE.to_owned());
    config.brave_base = brave;
    let running = cortex_api::serve(config).await.expect("serve");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let base = format!("http://{}", running.addr);
    let http = client();
    let access = login(&http, &base, "a@example.com", "state-a").await;
    let other = client();
    let access_b = login(&other, &base, "b@example.com", "state-b").await;

    let settings: Value = http
        .get(format!("{base}/v1/settings"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("settings")
        .json()
        .await
        .expect("settings json");
    assert_eq!(settings["allow_access"], true);
    assert_eq!(settings["image_gen_enabled"], false);
    assert_eq!(settings["video_gen_enabled"], false);
    assert_eq!(settings["voice_mode_enabled"], false);
    assert_eq!(settings["web_fetch_enabled"], false);
    assert_eq!(settings["workspace_command_enabled"], false);
    assert_eq!(settings["managed_mcps_enabled"], false);
    assert_eq!(settings["managed_mcp_gateway_tools_enabled"], false);
    assert!(
        settings["imagine_tools_disabled"]
            .as_array()
            .expect("list")
            .iter()
            .any(|tool| tool == "image_edit")
    );
    assert_eq!(settings["sharing_enabled"], true);
    assert_eq!(settings["web_search_model"], "Qwen3.5-9B");
    assert_eq!(settings["image_description_model"], "Qwen3.5-9B");
    assert_eq!(settings["memory_enabled"], true);
    assert_eq!(settings["memory_embedding_model"], "embed-small");
    assert_eq!(settings["memory_embedding_dimensions"], 768);

    // The pointer file in the dist dir wins over the configured version; junk falls back.
    let pointer = |http: &reqwest::Client, base: &str| {
        let url = format!("{base}/cli/stable");
        let http = http.clone();
        async move {
            http.get(url)
                .send()
                .await
                .expect("stable")
                .text()
                .await
                .expect("text")
        }
    };
    assert_eq!(pointer(&http, &base).await.trim(), "0.0.0");
    std::fs::write(dist.join("stable"), "9.9.9\n").expect("pointer");
    assert_eq!(pointer(&http, &base).await.trim(), "9.9.9");
    std::fs::write(dist.join("stable"), "not a version\n").expect("junk pointer");
    assert_eq!(pointer(&http, &base).await.trim(), "0.0.0");
    std::fs::remove_file(dist.join("stable")).expect("remove pointer");

    // Install scripts and builds are public and exact-name only.
    let script = http
        .get(format!("{base}/cli/install.sh"))
        .send()
        .await
        .expect("script");
    assert_eq!(script.status(), 200);
    assert!(
        script
            .text()
            .await
            .expect("script text")
            .contains("installer")
    );
    let root_build = http
        .get(format!("{base}/cortex-9.9.9-linux-x86_64"))
        .send()
        .await
        .expect("build");
    assert_eq!(root_build.status(), 200);
    let head = http
        .head(format!("{base}/cli/cortex-9.9.9-linux-x86_64"))
        .send()
        .await
        .expect("head");
    assert_eq!(head.headers()["content-length"], "10");
    let ranged = http
        .get(format!("{base}/cli/cortex-9.9.9-linux-x86_64"))
        .header("range", "bytes=2-4")
        .send()
        .await
        .expect("range");
    assert_eq!(ranged.status(), 206);
    assert_eq!(ranged.bytes().await.expect("range body").as_ref(), b"234");
    for path in [
        "/cli/secret.txt",
        "/cli/cortex-9.9.9-linux-x86_64.zst",
        "/cli/cortex-9.9.9-linux-armv7",
        "/cli/cortex-..%2Fsecret-linux-x86_64",
        "/cli/cortex--linux-x86_64",
    ] {
        let denied = http
            .get(format!("{base}{path}"))
            .send()
            .await
            .expect("denied");
        assert_eq!(denied.status(), 404, "{path}");
    }
    let notes = http
        .get(format!("{base}/cli/changelogs/9.9.9.external.json"))
        .send()
        .await
        .expect("notes");
    assert_eq!(notes.status(), 200);
    assert!(notes.text().await.expect("notes text").contains("features"));
    for path in [
        "/cli/changelogs/9.9.9.md",
        "/cli/changelogs/..%2Fsecret.txt",
        "/cli/changelogs/1.0.0.external.md",
    ] {
        let denied = http
            .get(format!("{base}{path}"))
            .send()
            .await
            .expect("denied");
        assert_eq!(denied.status(), 404, "{path}");
    }
    std::fs::remove_dir_all(&dist).expect("remove dist dir");

    // Calls the client makes besides inference: each answers, or is switched off by settings.
    assert_eq!(
        http.get(format!("{base}/"))
            .send()
            .await
            .expect("root")
            .status(),
        200
    );
    assert_eq!(
        http.get(format!("{base}/v1/api-key"))
            .send()
            .await
            .expect("key")
            .status(),
        401
    );
    // The probe describes the API key a request was made with; a session token has none.
    // The key path itself is covered in tests/api_keys.rs.
    let key = http
        .get(format!("{base}/v1/api-key"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("key");
    assert_eq!(key.status(), 400);
    let billing: Value = http
        .get(format!("{base}/v1/billing?format=credits"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("billing")
        .json()
        .await
        .expect("billing json");
    assert_eq!(
        billing["config"]["currentPeriod"]["type"],
        "USAGE_PERIOD_TYPE_DAILY"
    );
    assert!(
        billing["config"]["creditUsagePercent"]
            .as_f64()
            .expect("percent")
            >= 0.0
    );
    let topup: Value = http
        .get(format!("{base}/v1/auto-topup-rule"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("topup")
        .json()
        .await
        .expect("topup json");
    assert_eq!(topup["rule"]["enabled"], false);
    let consent = http
        .post(format!("{base}/v1/consent/accept"))
        .bearer_auth(&access)
        .json(&json!({"noticeId": "n1", "version": 2}))
        .send()
        .await
        .expect("consent");
    assert_eq!(consent.status(), 200);
    let bad_consent = http
        .post(format!("{base}/v1/consent/accept"))
        .bearer_auth(&access)
        .json(&json!({"noticeId": "n1"}))
        .send()
        .await
        .expect("consent");
    assert_eq!(bad_consent.status(), 400);
    let heuristics: Value = http
        .get(format!("{base}/v1/feedback/config"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("heuristics")
        .json()
        .await
        .expect("heuristics json");
    assert_eq!(heuristics["enabled"], false);
    let created: Value = http
        .post(format!("{base}/v1/feedback/requests"))
        .bearer_auth(&access)
        .json(&json!({"requestId": "r-1", "sessionId": "s1", "clientType": "tui", "feedbackMode": "thumbs", "triggerType": "tier1_engagement"}))
        .send()
        .await
        .expect("feedback request")
        .json()
        .await
        .expect("feedback request json");
    assert_eq!(created["requestId"], "r-1");
    let mut delta = serde_json::Map::new();
    delta.insert("clientType".into(), json!("tui"));
    delta.insert("toolsUsedThisTurn".into(), json!([]));
    delta.insert("errorTypesThisTurn".into(), json!([]));
    delta.insert("toolOutcomes".into(), json!(""));
    for name in [
        "turnNumber",
        "deltaToolCalls",
        "deltaToolFailures",
        "deltaErrors",
        "deltaCancellations",
        "deltaRegenerations",
        "deltaCompactions",
        "deltaEditAndRetries",
        "deltaPositiveRatings",
        "deltaNegativeRatings",
        "deltaAssistantMessages",
        "deltaLongPauses",
        "deltaSuccessfulToolUses",
        "consecutiveCancellations",
        "contextWindowUsage",
        "cumulativeToolCalls",
        "cumulativeErrors",
        "sessionDurationSeconds",
        "totalTokensBeforeCompaction",
        "feedbackRequestsSent",
        "deltaAgentLinesAdded",
        "deltaAgentLinesRemoved",
        "deltaAgentLinesAddedReverted",
        "deltaAgentLinesRemovedReverted",
        "deltaHumanLinesAdded",
        "deltaHumanLinesRemoved",
    ] {
        delta.insert(name.into(), json!(0));
    }
    delta.insert("turnNumber".into(), json!(3));
    let recorded: Value = http
        .post(format!("{base}/v1/sessions/s1/turn-deltas"))
        .bearer_auth(&access)
        .json(&Value::Object(delta))
        .send()
        .await
        .expect("delta")
        .json()
        .await
        .expect("delta json");
    assert_eq!(recorded["turnNumber"], 3);
    let tokens: Value = http
        .post(format!("{base}/v1/tokenize-text"))
        .bearer_auth(&access)
        .json(&json!({"text": "hello there", "model": "m"}))
        .send()
        .await
        .expect("tokens")
        .json()
        .await
        .expect("tokens json");
    assert_eq!(tokens["token_ids"].as_array().expect("ids").len(), 3);
    let images = http
        .post(format!("{base}/v1/images/generations"))
        .bearer_auth(&access)
        .json(&json!({}))
        .send()
        .await
        .expect("images");
    assert_eq!(images.status(), 404);
    let voice = http
        .get(format!("{base}/v1/stt"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("voice");
    assert_eq!(voice.status(), 404);

    let models: Value = http
        .get(format!("{base}/v1/models"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("models")
        .json()
        .await
        .expect("models json");
    assert_eq!(models["data"][0]["context_window"], 1638);
    assert_eq!(models["data"][0]["supports_backend_search"], true);

    let missing = http
        .put(format!("{base}/sessions/s1"))
        .bearer_auth(&access)
        .json(&json!({"session": {"title": "One", "cwd": "/tmp"}, "agentId": "agent-1"}))
        .send()
        .await
        .expect("upsert");
    assert_eq!(missing.status(), 200);
    let saved = http
        .post(format!("{base}/sessions/s1/data"))
        .bearer_auth(&access)
        .json(&json!({"messages": [{"content": "hello", "timestamp": "2026-10-06T00:00:00Z"}]}))
        .send()
        .await
        .expect("save");
    assert_eq!(saved.status(), 200);
    let listed: Value = http
        .get(format!("{base}/sessions"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list json");
    assert_eq!(listed["sessions"][0]["sessionId"], "s1");
    let loaded: Value = http
        .get(format!("{base}/sessions/s1/data"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("load")
        .json()
        .await
        .expect("load json");
    assert_eq!(loaded["messages"][0]["content"], "hello");
    assert_eq!(loaded["session"]["sessionId"], "s1");
    let hidden = http
        .get(format!("{base}/sessions/s1/data"))
        .bearer_auth(&access_b)
        .send()
        .await
        .expect("hidden");
    assert_eq!(hidden.status(), 404);
    let share: Value = http
        .post(format!("{base}/sessions/s1/share"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("share")
        .json()
        .await
        .expect("share json");
    let permission = share["permissionId"].as_str().expect("permission");
    let denied = other
        .get(format!("{base}/build/share/{permission}"))
        .bearer_auth(&access_b)
        .send()
        .await
        .expect("denied share");
    assert_eq!(denied.status(), 404);
    let opened: Value = http
        .get(format!("{base}/build/share/{permission}"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("open share")
        .json()
        .await
        .expect("open json");
    assert_eq!(opened["sessionId"], "s1");

    let conv = http.put(format!("{base}/rest/app-chat/conversations/c1")).bearer_auth(&access).json(&json!({"title": "Notes", "starred": true, "workspaces": [{"workspaceId": "w1", "name": "Work"}]})).send().await.expect("conv");
    assert_eq!(conv.status(), 200);
    let convs: Value = http
        .get(format!("{base}/rest/app-chat/conversations?pageSize=10"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("convs")
        .json()
        .await
        .expect("convs json");
    assert_eq!(convs["conversations"][0]["conversationId"], "c1");
    assert_eq!(
        convs["conversations"][0]["workspaces"][0]["workspaceId"],
        "w1"
    );
    let found: Value = http
        .get(format!(
            "{base}/rest/app-chat/conversations?pageSize=10&searchQuery=Notes"
        ))
        .bearer_auth(&access)
        .send()
        .await
        .expect("search conv")
        .json()
        .await
        .expect("search conv json");
    assert_eq!(
        found["textSearchMatches"][0]["conversation"]["title"],
        "Notes"
    );
    let ws: Value = http
        .get(format!("{base}/rest/workspaces?pageSize=10"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("workspaces")
        .json()
        .await
        .expect("workspaces json");
    assert_eq!(ws["workspaces"][0]["workspaceId"], "w1");
    let foreign = other
        .get(format!("{base}/rest/workspaces?pageSize=10"))
        .bearer_auth(&access_b)
        .send()
        .await
        .expect("foreign ws")
        .json::<Value>()
        .await
        .expect("foreign json");
    assert!(foreign["workspaces"].as_array().expect("array").is_empty());
    let removed = http
        .delete(format!("{base}/rest/app-chat/conversations/soft/c1"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("soft");
    assert_eq!(removed.status(), 200);
    let again = http
        .delete(format!("{base}/rest/app-chat/conversations/soft/c1"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("soft again");
    assert_eq!(again.status(), 404);

    let skills: Value = http
        .post(format!("{base}/rest/skills"))
        .bearer_auth(&access)
        .json(&json!({"locale": "en"}))
        .send()
        .await
        .expect("skills")
        .json()
        .await
        .expect("skills json");
    assert_eq!(skills["skills"][0]["name"], "cortex");
    let user_skills: Value = http
        .get(format!("{base}/rest/user-skills"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("user skills")
        .json()
        .await
        .expect("user skills json");
    assert!(user_skills["skills"].as_array().expect("skills").is_empty());
    let modes: Value = http
        .post(format!("{base}/rest/modes"))
        .bearer_auth(&access)
        .json(&json!({"locale": "en"}))
        .send()
        .await
        .expect("modes")
        .json()
        .await
        .expect("modes json");
    assert_eq!(modes["defaultModeId"], "Qwen3.5-9B");
    assert!(modes["modes"][0]["availability"]["available"].is_object());

    let created: Value = http.post(format!("{base}/v1/sandbox/environments")).bearer_auth(&access).json(&json!({"name": "dev", "environmentVariables": [{"key": "A", "value": "1"}], "secrets": [{"key": "TOKEN", "value": "secret-value"}], "snapshotBucket": "attacker"})).send().await.expect("env").json().await.expect("env json");
    let env_id = created["environment"]["environment"]["environmentId"]
        .as_str()
        .expect("env id")
        .to_owned();
    assert_eq!(created["environment"]["environment"]["name"], "dev");
    assert_eq!(created["environment"]["secrets"][0]["value"], "");
    assert!(!created.to_string().contains("secret-value"));
    assert!(!created.to_string().contains("attacker"));
    let listed_env: Value = http
        .get(format!("{base}/v1/sandbox/environments"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("envs")
        .json()
        .await
        .expect("envs json");
    assert_eq!(
        listed_env["environments"][0]["environment"]["environmentId"],
        env_id
    );
    let forked: Value = http
        .post(format!("{base}/v1/sandbox/sessions/fork"))
        .bearer_auth(&access)
        .json(&json!({"sourceSandboxId": env_id, "copies": 1, "snapshotBucket": "attacker"}))
        .send()
        .await
        .expect("fork")
        .json()
        .await
        .expect("fork json");
    let sandbox_id = forked["sandboxIds"][0]
        .as_str()
        .expect("sandbox")
        .to_owned();
    assert!(
        forked["sessions"][0]["websocketUrl"]
            .as_str()
            .expect("ws")
            .contains("/ws/code-agent")
    );
    assert!(!forked.to_string().contains("attacker"));
    let foreign_fork = other
        .delete(format!("{base}/v1/sandbox/sessions/{sandbox_id}"))
        .bearer_auth(&access_b)
        .send()
        .await
        .expect("foreign fork");
    assert_eq!(foreign_fork.status(), 404);
    let deleted = http
        .delete(format!("{base}/v1/sandbox/sessions/{sandbox_id}"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("delete sandbox");
    assert_eq!(deleted.status(), 200);
    let renamed = http
        .put(format!("{base}/v1/sandbox/environments/{env_id}"))
        .bearer_auth(&access)
        .json(&json!({"name": "dev-2"}))
        .send()
        .await
        .expect("rename env");
    assert_eq!(renamed.status(), 200);
    let removed_env = http
        .delete(format!("{base}/v1/sandbox/environments/{env_id}"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("delete env");
    assert_eq!(removed_env.status(), 200);

    let registered = http.post(format!("{base}/v1/sessions/register")).bearer_auth(&access).json(&json!({"sessionId": "reg-1", "cwd": "/work", "gcsTracePrefix": "traces/reg-1", "modelId": "Qwen3.5-9B"})).send().await.expect("register session");
    assert_eq!(registered.status(), 200);
    let updated = http
        .post(format!("{base}/v1/sessions/reg-1/replicas/update"))
        .bearer_auth(&access)
        .json(&json!({"summary": "registry row", "lastTurnNumber": 2, "restorableTurnNumber": 1}))
        .send()
        .await
        .expect("replica");
    assert_eq!(updated.status(), 200);
    let found_reg: Value = http
        .get(format!("{base}/v1/sessions/search?limit=5&query=registry"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("registry search")
        .json()
        .await
        .expect("registry json");
    assert_eq!(found_reg["sessions"][0]["sessionId"], "reg-1");
    assert_eq!(found_reg["sessions"][0]["lastTurnNumber"], 2);
    let owned_reg = http
        .get(format!("{base}/v1/sessions/reg-1/replicas"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("owned replica");
    assert_eq!(owned_reg.status(), 200);
    let foreign_reg = other
        .get(format!("{base}/v1/sessions/reg-1/replicas"))
        .bearer_auth(&access_b)
        .send()
        .await
        .expect("foreign replica");
    assert_eq!(foreign_reg.status(), 404);
    let finalized = http
        .post(format!("{base}/v1/sessions/reg-1/replicas/finalize"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("finalize");
    assert_eq!(finalized.status(), 200);

    let limits: Value = http
        .get(format!("{base}/v1/storage/limits"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("limits")
        .json()
        .await
        .expect("limits json");
    assert_eq!(limits["enabled"], true);
    let uploaded = http
        .post(format!("{base}/v1/storage"))
        .bearer_auth(&access)
        .header("x-storage-path", "traces/reg-1/1/notes.txt")
        .header("content-type", "text/plain")
        .body("hello storage")
        .send()
        .await
        .expect("upload");
    assert_eq!(uploaded.status(), 200);
    let exists = http
        .get(format!("{base}/v1/storage/exists"))
        .bearer_auth(&access)
        .header("x-storage-path", "traces/reg-1/1/notes.txt")
        .send()
        .await
        .expect("exists");
    assert_eq!(exists.status(), 200);
    let missing_obj = other
        .get(format!("{base}/v1/storage/exists"))
        .bearer_auth(&access_b)
        .header("x-storage-path", "traces/reg-1/1/notes.txt")
        .send()
        .await
        .expect("missing object");
    assert_eq!(missing_obj.status(), 404);
    let batch: Value = http
        .post(format!("{base}/v1/storage/batch_exists"))
        .bearer_auth(&access)
        .json(&json!({"paths": ["traces/reg-1/1/notes.txt", "missing.txt"]}))
        .send()
        .await
        .expect("batch exists")
        .json()
        .await
        .expect("batch json");
    assert_eq!(batch["exists"][0], "traces/reg-1/1/notes.txt");
    assert_eq!(batch["missing"][0], "missing.txt");
    let part = reqwest::multipart::Part::bytes(b"batch".to_vec())
        .file_name("batch/a.txt")
        .mime_str("text/plain")
        .expect("mime");
    let form = reqwest::multipart::Form::new().part("batch/a.txt", part);
    let batched = http
        .post(format!("{base}/v1/storage/batch_upload"))
        .bearer_auth(&access)
        .multipart(form)
        .send()
        .await
        .expect("batch upload");
    assert_eq!(batched.status(), 200);
    let json_upload: Value = http.post(format!("{base}/v1/storage/batch_upload_json")).bearer_auth(&access).json(&json!({"files": [{"path": "batch/b.txt", "content_type": "text/plain", "data": "aGk="}]})).send().await.expect("json upload").json().await.expect("json upload body");
    assert_eq!(json_upload["results"][0]["status"], "ok");
    let signed: Value = http
        .get(format!("{base}/v1/storage/download"))
        .bearer_auth(&access)
        .header("x-storage-path", "traces/reg-1/1/notes.txt")
        .send()
        .await
        .expect("download")
        .json()
        .await
        .expect("download json");
    let signed_url = signed["signed_url"].as_str().expect("signed");
    let bytes = http
        .get(signed_url)
        .send()
        .await
        .expect("signed get")
        .text()
        .await
        .expect("signed text");
    assert_eq!(bytes, "hello storage");
    let replica_file: Value = http
        .get(format!(
            "{base}/v1/sessions/reg-1/download?file=notes.txt&turn=1"
        ))
        .bearer_auth(&access)
        .send()
        .await
        .expect("replica download")
        .json()
        .await
        .expect("replica download json");
    let replica_bytes = http
        .get(replica_file["downloadUrl"].as_str().expect("url"))
        .send()
        .await
        .expect("replica bytes")
        .text()
        .await
        .expect("replica text");
    assert_eq!(replica_bytes, "hello storage");
    let put_url: Value = http
        .post(format!("{base}/v1/storage/signed-upload-url"))
        .bearer_auth(&access)
        .header("x-storage-path", "direct/c.txt")
        .header("content-type", "text/plain")
        .send()
        .await
        .expect("signed put")
        .json()
        .await
        .expect("signed put json");
    let put = http
        .put(put_url["signedUrl"].as_str().expect("put url"))
        .body("direct")
        .send()
        .await
        .expect("put");
    assert_eq!(put.status(), 200);
    let init: Value = http
        .post(format!("{base}/v1/storage/multipart/init"))
        .bearer_auth(&access)
        .json(&json!({"totalSize": 4}))
        .send()
        .await
        .expect("init")
        .json()
        .await
        .expect("init json");
    let part_url = init["partUrls"][0]["url"].as_str().expect("part url");
    let part_put = http
        .put(part_url)
        .body("part")
        .send()
        .await
        .expect("part put");
    assert_eq!(part_put.status(), 200);
    let completed = http
        .post(format!(
            "{base}/v1/storage/multipart/{}/complete",
            init["uploadId"].as_str().expect("upload")
        ))
        .bearer_auth(&access)
        .header("x-storage-path", "multi/out.txt")
        .header("content-type", "text/plain")
        .json(&json!({"parts": [{"partNumber": 1, "path": "multi/1"}]}))
        .send()
        .await
        .expect("complete");
    assert_eq!(completed.status(), 200);

    let embedded: Value = http
        .post(format!("{base}/v1/embeddings"))
        .bearer_auth(&access)
        .json(&json!({"model": "embed", "input": ["hello"], "dimensions": 2}))
        .send()
        .await
        .expect("embed")
        .json()
        .await
        .expect("embed json");
    assert_eq!(
        embedded["data"][0]["embedding"]
            .as_array()
            .expect("vector")
            .len(),
        2
    );

    let answer: Value = http.post(format!("{base}/v1/responses")).bearer_auth(&access).json(&json!({"model": "Qwen3.5-9B", "input": "rust ownership", "tools": [{"type": "web_search"}]})).send().await.expect("search").json().await.expect("search json");
    let rendered = answer.to_string();
    assert!(
        rendered.contains("https://www.rust-lang.org/"),
        "{rendered}"
    );
    assert!(rendered.contains("Ownership is a Rust rule."), "{rendered}");
    assert!(!rendered.contains(BRAVE), "{rendered}");

    relay_pair(&base, &access).await;
    gateway_pair(&base, &access, answer["id"].as_str().unwrap_or("user")).await;

    let deleted_data = http
        .delete(format!("{base}/sessions/s1/data"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("delete data");
    assert_eq!(deleted_data.status(), 200);
    let _ = running;
}

async fn relay_pair(base: &str, access: &str) {
    let mut left = ws(base, "/ws/code-agent", access).await;
    let mut right = ws(base, "/ws/code-agent", access).await;
    let left_init = next_text(&mut left).await;
    let right_init = next_text(&mut right).await;
    assert!(left_init.contains("initialize"), "{left_init}");
    assert!(right_init.contains("initialize"), "{right_init}");
    let result = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"1"}}"#;
    left.send(tokio_tungstenite::tungstenite::Message::Text(result.into()))
        .await
        .expect("left result");
    right
        .send(tokio_tungstenite::tungstenite::Message::Text(result.into()))
        .await
        .expect("right result");
    let upsert = r#"{"jsonrpc":"2.0","method":"_cortex/session/upsert","params":{"sessionId":"s1","cwd":"/tmp"}}"#;
    left.send(tokio_tungstenite::tungstenite::Message::Text(upsert.into()))
        .await
        .expect("left upsert");
    right
        .send(tokio_tungstenite::tungstenite::Message::Text(upsert.into()))
        .await
        .expect("right upsert");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let update = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"kind":"ping"}}}"#;
    left.send(tokio_tungstenite::tungstenite::Message::Text(update.into()))
        .await
        .expect("update");
    let received = until_text(&mut right, "session/update").await;
    assert!(received.contains("ping"), "{received}");
}

async fn gateway_pair(base: &str, access: &str, user_hint: &str) {
    let _ = user_hint;
    let mut left = ws(base, "/ws/gw/", access).await;
    let mut right = ws(base, "/ws/gw/", access).await;
    let hello = r#"{"protocol_version":"1.0.0","kind":"harness"}"#;
    left.send(tokio_tungstenite::tungstenite::Message::Text(hello.into()))
        .await
        .expect("hello");
    right
        .send(tokio_tungstenite::tungstenite::Message::Text(hello.into()))
        .await
        .expect("hello");
    let left_ack = next_text(&mut left).await;
    let right_ack = next_text(&mut right).await;
    assert!(left_ack.contains("connection_id"), "{left_ack}");
    assert!(right_ack.contains("user_id"), "{right_ack}");
    let frame = r#"{"jsonrpc":"2.0","method":"hub.ping","params":{}}"#;
    left.send(tokio_tungstenite::tungstenite::Message::Text(frame.into()))
        .await
        .expect("frame");
    let received = until_text(&mut right, "hub.ping").await;
    assert!(received.contains("hub.ping"), "{received}");
}

async fn ws(
    base: &str,
    path: &str,
    access: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!("ws://{}{path}", base.trim_start_matches("http://"));
    let mut request = url.into_client_request().expect("request");
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {access}").parse().expect("auth"),
    );
    request
        .headers_mut()
        .insert("x-cortex-token-auth", "cortex-cli".parse().expect("header"));
    let (socket, _) = connect_async(request).await.expect("websocket");
    socket
}

async fn next_text(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> String {
    let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("timeout")
        .expect("closed")
        .expect("message");
    match message {
        tokio_tungstenite::tungstenite::Message::Text(text) => text.to_string(),
        other => panic!("unexpected frame {other:?}"),
    }
}

async fn until_text(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    needle: &str,
) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut last = String::new();
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let message = tokio::time::timeout(remaining, socket.next())
            .await
            .expect("timeout")
            .expect("closed")
            .expect("message");
        if let tokio_tungstenite::tungstenite::Message::Text(text) = message {
            last = text.to_string();
            if last.contains(needle) {
                return last;
            }
        }
    }
    last
}

async fn login(http: &reqwest::Client, base: &str, email: &str, state: &str) -> String {
    let (verifier, challenge) = pkce();
    let redirect = "http://127.0.0.1:1/callback";
    let authorize = format!(
        "{base}/authorize?response_type=code&client_id={client}&redirect_uri={redirect}&scope=openid&code_challenge={challenge}&code_challenge_method=S256&state={state}&nonce={state}",
        client = cortex_api::CLIENT_ID,
        redirect = urlencoding(redirect),
        challenge = urlencoding(&challenge),
    );
    let next = authorize.trim_start_matches(base);
    http.get(&authorize).send().await.expect("authorize");
    let created = http
        .post(format!("{base}/register"))
        .form(&[
            ("email", email),
            ("password", "correct-horse"),
            ("first_name", "A"),
            ("last_name", "B"),
            ("next", next),
        ])
        .send()
        .await
        .expect("register");
    assert_eq!(created.status(), 303, "{email}");
    let allow = http
        .post(format!("{base}/consent"))
        .form(&[("state", state), ("decision", "allow")])
        .send()
        .await
        .expect("allow");
    assert_eq!(allow.status(), 303, "{email}");
    let loc = allow
        .headers()
        .get(reqwest::header::LOCATION)
        .expect("location")
        .to_str()
        .expect("location str");
    let code = query_param(loc, "code").expect("code");
    let tokens: Value = http
        .post(format!("{base}/oauth2/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect),
            ("client_id", cortex_api::CLIENT_ID),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .await
        .expect("token")
        .json()
        .await
        .expect("token json");
    tokens["access_token"].as_str().expect("access").to_owned()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .expect("client")
}

fn truncate() {
    let status = std::process::Command::new("psql")
        .args(["-h", "127.0.0.1", "-d", "cortex_api_phase4", "-c", "TRUNCATE users, browser_sessions, auth_transactions, refresh_tokens, device_grants, login_attempts, rate_buckets, relay_events CASCADE"])
        .stderr(std::process::Stdio::null())
        .status();
    if let Ok(status) = status {
        let _ = status.success();
    }
}

fn pkce() -> (String, String) {
    let mut raw = [0u8; 32];
    rand::Rng::fill(&mut rand::rng(), &mut raw);
    let verifier = URL_SAFE_NO_PAD.encode(raw);
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn urlencoding(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
}

async fn mock_llama() -> String {
    let app = Router::new()
        .route("/props", get(|| async { Json(json!({"default_generation_settings": {"n_ctx": 4096}, "modalities": {"vision": true}})) }))
        .route("/v1/responses", post(mock_responses))
        .route("/v1/embeddings", post(|| async { Json(json!({"data": [{"embedding": [0.1, 0.2, 0.3, 0.4]}]})) }))
        .route("/tokenize", post(|| async { Json(json!({"tokens": [11, 22, 33]})) }));
    bind(app).await
}

async fn mock_responses(body: Bytes) -> impl IntoResponse {
    let text = String::from_utf8_lossy(&body);
    if text.contains("function_call_output") {
        Json(json!({
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Ownership is a Rust rule."}]
            }]
        }))
    } else {
        Json(json!({
            "output": [{
                "type": "function_call",
                "name": "web_search",
                "call_id": "call_1",
                "arguments": "{\"query\":\"rust ownership\"}"
            }]
        }))
    }
}

async fn mock_brave() -> String {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/web/search", get(brave_search))
        .with_state(hits);
    bind(app).await
}

async fn brave_search(State(hits): State<Arc<AtomicUsize>>, headers: HeaderMap) -> Response {
    let token = headers
        .get("x-subscription-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if token != BRAVE {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    hits.fetch_add(1, Ordering::Relaxed);
    Json(json!({
        "web": {"results": [{
            "title": "Rust",
            "url": "https://www.rust-lang.org/",
            "description": "A language empowering everyone"
        }]}
    }))
    .into_response()
}

async fn bind(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock");
    });
    format!("http://{addr}")
}
