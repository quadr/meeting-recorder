//! Read-only record synchronization using the documented public v2 API.
//! https://callabo.ai/en/developers/api/records
use super::*;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RemoteRecord {
    pub status: String,
    pub title: Option<String>,
    pub summary: String,
    pub synced_at: Option<i64>,
    pub checked_at: i64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LinkedRecord {
    pub uuid: String,
    pub workspace: String,
    pub workspace_name: String,
    pub record_id: u64,
    pub url: String,
    pub remote: Option<RemoteRecord>,
}

fn linked(receipt: &Receipt) -> Option<LinkedRecord> {
    if receipt.stage != "done" {
        return None;
    }
    let id = receipt.record_id?;
    let mut url = Url::parse("https://callabo.ai/en/workspace/").ok()?;
    url.path_segments_mut().ok()?.pop_if_empty().extend([
        &receipt.workspace,
        "record",
        &id.to_string(),
        "detail",
    ]);
    Some(LinkedRecord {
        uuid: receipt.uuid.clone(),
        workspace: receipt.workspace.clone(),
        workspace_name: receipt
            .workspace_name
            .clone()
            .unwrap_or_else(|| receipt.workspace.clone()),
        record_id: id,
        url: url.to_string(),
        remote: receipt.remote.clone(),
    })
}

pub fn completed_links(dir: &Path, base: &str) -> Vec<LinkedRecord> {
    Journal::load(&dir.join(format!("{base}.callabo.json")))
        .map(|j| j.uploads.iter().filter_map(linked).collect())
        .unwrap_or_default()
}

fn insight_text(value: &Value) -> String {
    // Only documented textual insight fields; never render remote HTML as markup.
    match value {
        Value::String(s) => s.trim().to_owned(),
        Value::Array(items) => items
            .iter()
            .map(insight_text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::Object(_) => [value.get("title"), value.get("items")]
            .into_iter()
            .flatten()
            .map(insight_text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn parse_remote(body: &Value, id: u64, now: i64) -> Result<RemoteRecord, String> {
    let record = body.get("record").ok_or("Missing Callabo record")?;
    if record.get("id").and_then(Value::as_u64) != Some(id) {
        return Err("Callabo returned a different record.".into());
    }
    let status = record
        .get("status")
        .and_then(Value::as_str)
        .ok_or("Missing Callabo record status")?;
    let empty = Vec::new();
    let insights = match body.get("insights") {
        Some(Value::Array(items)) => items,
        Some(Value::Null) => &empty,
        _ => return Err("Missing Callabo insights".into()),
    };
    let summary_types = [
        "summary",
        "custom",
        "product-meeting",
        "customer-interview",
        "demo-meeting",
        "takeaway",
    ];
    let selected = summary_types
        .iter()
        .find_map(|kind| insights.iter().find(|item| item["type"] == *kind));
    let summary = selected
        .filter(|item| item["status"] == "processed")
        .map(|item| insight_text(&item["value"]).chars().take(32_000).collect())
        .unwrap_or_default();
    let state = match status {
        "failed" | "not_supported" => "failed",
        "created" | "uploaded" | "transcribed" => "processing",
        "processed"
            if record["is_title_generating"] == true
                || insights.iter().any(|i| i["status"] == "created") =>
        {
            "processing"
        }
        "processed"
            if selected
                .is_some_and(|i| i["status"] == "failed" || i["status"] == "not_supported") =>
        {
            "summary_failed"
        }
        "processed" => "ready",
        _ => return Err("Unrecognized Callabo processing status.".into()),
    };
    let title = record
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(500).collect());
    Ok(RemoteRecord {
        status: state.into(),
        title,
        summary,
        synced_at: Some(now),
        checked_at: now,
        error: None,
    })
}

async fn fetch_remote(
    api: &str,
    token: &str,
    workspace: &str,
    id: u64,
) -> Result<RemoteRecord, String> {
    let mut url = resource_url(api, workspace, "records")?;
    url.path_segments_mut()
        .map_err(|_| "Invalid Callabo URL")?
        .push(&id.to_string());
    url.query_pairs_mut().append_pair("include", "insights");
    let (client, _) = clients()?;
    let body = json_response(
        check(
            client
                .get(url)
                .bearer_auth(validate_token(token)?)
                .send()
                .await,
            "record lookup",
        )
        .await?,
    )
    .await?;
    parse_remote(&body, id, chrono::Utc::now().timestamp())
}

fn store_result(
    path: &Path,
    uuid: &str,
    result: Result<RemoteRecord, String>,
) -> Result<LinkedRecord, String> {
    // Reload after the network request. A deleted/renamed recording must never
    // be recreated, and another upload's receipt must not be overwritten.
    let mut journal = Journal::load(path)?;
    let receipt = journal
        .uploads
        .iter_mut()
        .find(|r| r.uuid == uuid && r.stage == "done")
        .ok_or("The local Callabo link changed while refreshing.")?;
    receipt.remote = Some(match result {
        Ok(remote) => remote,
        Err(error) => {
            let mut cached = receipt.remote.clone().unwrap_or(RemoteRecord {
                status: "unknown".into(),
                title: None,
                summary: String::new(),
                synced_at: None,
                checked_at: 0,
                error: None,
            });
            cached.checked_at = chrono::Utc::now().timestamp();
            cached.error = Some(error);
            cached
        }
    });
    let result = linked(receipt).ok_or("Missing Callabo link")?;
    journal.save(path)?;
    Ok(result)
}

#[tauri::command]
pub async fn callabo_sync_record(
    folder: Option<String>,
    base: String,
    uuid: String,
    app: AppHandle,
) -> Result<LinkedRecord, String> {
    let uploads = app.state::<Uploads>();
    let dir = recording_dir(&recordings_root(), folder.as_deref(), &base)?;
    let path = dir.join(format!("{base}.callabo.json"));
    let receipt = uploads.while_idle(folder.as_deref(), &base, || {
        Journal::load(&path)?
            .uploads
            .into_iter()
            .find(|r| r.uuid == uuid && r.stage == "done")
            .ok_or_else(|| "No completed Callabo upload for this recording.".into())
    })?;
    let result = match saved_token() {
        Ok(token) => {
            fetch_remote(
                API,
                &token,
                &receipt.workspace,
                receipt.record_id.ok_or("Missing Callabo record ID")?,
            )
            .await
        }
        Err(error) => Err(error),
    };
    uploads.while_idle(folder.as_deref(), &base, || {
        store_result(&path, &uuid, result)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn payload(status: &str) -> Value {
        json!({"record":{"id":12,"title":"Meeting","status":status},"insights":[]})
    }
    #[test]
    fn stages_and_summary_are_independent_of_upload_completion() {
        for state in ["created", "uploaded", "transcribed"] {
            assert_eq!(
                parse_remote(&payload(state), 12, 1).unwrap().status,
                "processing"
            );
        }
        assert_eq!(
            parse_remote(&payload("failed"), 12, 1).unwrap().status,
            "failed"
        );
        assert!(parse_remote(&payload("new-state"), 12, 1).is_err());
        assert!(parse_remote(&payload("processed"), 13, 1).is_err());
        let mut body = payload("processed");
        body["insights"] = json!([{"type":"summary","status":"created","value":null}]);
        assert_eq!(parse_remote(&body, 12, 1).unwrap().status, "processing");
        body["insights"][0] =
            json!({"type":"summary","status":"processed","value":["First","Second"]});
        let result = parse_remote(&body, 12, 1).unwrap();
        assert_eq!(result.status, "ready");
        assert_eq!(result.summary, "First\n\nSecond");
        body["record"]["is_title_generating"] = json!(true);
        assert_eq!(parse_remote(&body, 12, 1).unwrap().status, "processing");
    }
    #[test]
    fn custom_summary_and_failed_summary_do_not_get_stuck_processing() {
        let mut body = payload("processed");
        body["insights"] = json!([{"type":"custom","status":"processed","value":[{"title":"Decision","items":["Ship"]}]}]);
        assert_eq!(
            parse_remote(&body, 12, 1).unwrap().summary,
            "Decision\nShip"
        );
        body["insights"][0]["status"] = json!("failed");
        assert_eq!(parse_remote(&body, 12, 1).unwrap().status, "summary_failed");
    }

    #[test]
    fn refresh_preserves_history_cache_and_never_resurrects_deleted_receipts() {
        let dir = std::env::temp_dir().join(format!("meetrec-links-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("record.callabo.json");
        let receipt = Receipt {
            workspace: "a/b".into(),
            workspace_name: Some("Team".into()),
            record_id: Some(12),
            uuid: "one".into(),
            filesize: 100,
            stage: "done".into(),
            options: None,
            remote: None,
        };
        receipt.save(&path).unwrap();
        let mut other = receipt.clone();
        other.uuid = "two".into();
        other.record_id = Some(13);
        other.save(&path).unwrap();
        let remote = parse_remote(&payload("processed"), 12, 42).unwrap();
        store_result(&path, "one", Ok(remote)).unwrap();
        let stale = store_result(&path, "one", Err("Offline".into())).unwrap();
        let cached = stale.remote.unwrap();
        assert_eq!(cached.title.as_deref(), Some("Meeting"));
        assert_eq!(cached.synced_at, Some(42));
        assert_eq!(cached.error.as_deref(), Some("Offline"));
        let history = Journal::load(&path).unwrap();
        assert_eq!(history.uploads.len(), 2);
        assert!(history.uploads[1].remote.is_none());
        assert!(stale.url.contains("a%2Fb/record/12/detail"));
        std::fs::remove_file(&path).unwrap();
        assert!(store_result(&path, "one", Err("Offline".into())).is_err());
        assert!(!path.exists());
        std::fs::remove_dir(&dir).unwrap();
    }

    #[tokio::test]
    async fn fetch_uses_public_record_detail_and_includes_insights() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buf[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("GET /v2/workspaces/team/records/12?include=insights "));
            assert!(request
                .to_lowercase()
                .contains("authorization: bearer pat_fake"));
            let body = payload("processed").to_string();
            let response=format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body);
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let result = fetch_remote(&api, "pat_fake", "team", 12).await.unwrap();
        assert_eq!(result.status, "ready");
        task.await.unwrap();
    }
}
