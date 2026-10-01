//! Manual uploads using the protocol of the official Callabo CLI.
//! PATs are kept in the Windows credential vault, never config/journal/logs.
//! See https://github.com/rtzr/callabo-cli (v0.1.13); public v2 has no upload API.
use crate::{
    config::Config, recording_busy, recording_dir, recordings_root, Status, TranscribeQueue,
};
use reqwest::{Client, Response, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};
use tauri::{AppHandle, Emitter, Manager};

const API: &str = "https://api.callabo.ai";
type Key = (Option<String>, String);

#[derive(Default)]
pub struct Uploads(Mutex<HashSet<Key>>);

impl Uploads {
    /// Keep the admission lock for short filesystem mutations, so rename/delete
    /// cannot race between checking the upload set and touching the files.
    pub fn while_idle<T>(
        &self,
        folder: Option<&str>,
        base: &str,
        action: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let set = self
            .0
            .lock()
            .map_err(|_| "Callabo upload state unavailable")?;
        if set.contains(&(folder.map(str::to_owned), base.to_owned())) {
            return Err(
                "Wait until the Callabo upload finishes before changing this recording.".into(),
            );
        }
        action()
    }

    pub fn busy(&self, folder: Option<&str>, base: &str) -> bool {
        self.0
            .lock()
            .map(|s| s.contains(&(folder.map(str::to_owned), base.to_owned())))
            .unwrap_or(true)
    }

    fn start(&self, key: Key) -> Result<UploadGuard<'_>, String> {
        if !self
            .0
            .lock()
            .map_err(|_| "Callabo upload state unavailable")?
            .insert(key.clone())
        {
            return Err("This recording is already uploading to Callabo.".into());
        }
        Ok(UploadGuard { uploads: self, key })
    }
}

struct UploadGuard<'a> {
    uploads: &'a Uploads,
    key: Key,
}
impl Drop for UploadGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut s) = self.uploads.0.lock() {
            s.remove(&self.key);
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Workspace {
    pub slug: String,
    pub name: String,
}

fn default_language() -> String {
    "default".into()
}
fn default_scope() -> String {
    "workspace".into()
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(default)]
pub struct UploadPreferences {
    pub team_ids: Vec<u64>,
    pub label_ids: Vec<u64>,
    pub accessible_team_ids: Vec<u64>,
    pub accessible_user_ids: Vec<u64>,
    pub transcribe_language: String,
}
impl Default for UploadPreferences {
    fn default() -> Self {
        Self {
            team_ids: vec![],
            label_ids: vec![],
            accessible_team_ids: vec![],
            accessible_user_ids: vec![],
            transcribe_language: default_language(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct UploadOptions {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(flatten)]
    pub preferences: UploadPreferences,
}
impl Default for UploadOptions {
    fn default() -> Self {
        Self {
            title: None,
            scope: default_scope(),
            preferences: UploadPreferences::default(),
        }
    }
}
impl UploadOptions {
    fn normalized(mut self) -> Result<Self, String> {
        if !["private", "team", "workspace"].contains(&self.scope.as_str()) {
            return Err("Invalid Callabo visibility scope.".into());
        }
        let p = &mut self.preferences;
        for ids in [
            &mut p.team_ids,
            &mut p.label_ids,
            &mut p.accessible_team_ids,
            &mut p.accessible_user_ids,
        ] {
            if ids.len() > 50 || ids.iter().any(|id| *id == 0 || *id > i64::MAX as u64) {
                return Err("Callabo selections require up to 50 positive IDs.".into());
            }
            ids.sort_unstable();
            ids.dedup();
        }
        if self.scope == "team" && p.team_ids.is_empty() {
            return Err("Select at least one team for team visibility.".into());
        }
        if ![
            "default", "detect", "multi", "ko", "en", "ja", "zh", "es", "fr", "de", "ru", "pt",
            "it", "ar", "hi", "id", "vi", "th", "nl",
        ]
        .contains(&p.transcribe_language.as_str())
        {
            return Err("Invalid Callabo transcription language.".into());
        }
        self.title = self
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if self
            .title
            .as_ref()
            .is_some_and(|s| s.chars().count() > 500 || s.chars().any(char::is_control))
        {
            return Err(
                "Callabo title must be at most 500 characters without control characters.".into(),
            );
        }
        Ok(self)
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Team {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub request_insight_extract_type: Option<String>,
    #[serde(default)]
    pub default_custom_insight_template_id: Option<u64>,
}
#[derive(Serialize, Deserialize, Debug)]
pub struct Label {
    pub id: u64,
    pub name: String,
}
#[derive(Serialize)]
pub struct DialogData {
    teams: Vec<Team>,
    labels: Vec<Label>,
    preferences: UploadPreferences,
    teams_loaded: bool,
    labels_loaded: bool,
    warnings: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Receipt {
    pub workspace: String,
    #[serde(default)]
    pub workspace_name: Option<String>,
    pub record_id: Option<u64>,
    uuid: String,
    filesize: u64,
    // "creating" / "uploading" / "completing" / "done". Ambiguous remote
    // writes are not retried automatically; known incomplete records are reused.
    stage: String,
    #[serde(default)]
    options: Option<UploadOptions>,
}

impl Receipt {
    fn save(&self, path: &Path) -> Result<(), String> {
        let mut journal = Journal::load(path)?;
        match journal.uploads.iter_mut().find(|r| r.uuid == self.uuid) {
            Some(existing) => *existing = self.clone(),
            None => journal.uploads.push(self.clone()),
        }
        journal.save(path)
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Journal {
    uploads: Vec<Receipt>,
}

impl Journal {
    fn load(path: &Path) -> Result<Self, String> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Stored {
            History(Journal),
            Legacy(Receipt),
        }
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(Stored::History(journal)) => Ok(journal),
                Ok(Stored::Legacy(receipt)) => Ok(Self {
                    uploads: vec![receipt],
                }),
                Err(_) => {
                    Err("Cannot read existing Callabo receipt; check it before retrying.".into())
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(_) => Err("Cannot read Callabo upload receipt.".into()),
        }
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        let data = serde_json::to_vec_pretty(self)
            .map_err(|_| "Cannot serialize Callabo upload history")?;
        std::fs::write(path, data)
            .map_err(|_| "Cannot save Callabo upload history. Check folder permissions.".into())
    }

    fn remove_attempt(path: &Path, uuid: &str) -> Result<(), String> {
        let mut journal = Self::load(path)?;
        journal.uploads.retain(|r| r.uuid != uuid);
        if journal.uploads.is_empty() {
            std::fs::remove_file(path).map_err(|_| "Cannot clear rejected upload receipt".into())
        } else {
            journal.save(path)
        }
    }
}

#[derive(Serialize, PartialEq, Eq, Debug)]
pub struct UploadedWorkspace {
    slug: String,
    name: String,
}

pub fn completed_workspaces(dir: &Path, base: &str) -> Vec<UploadedWorkspace> {
    let Ok(journal) = Journal::load(&dir.join(format!("{base}.callabo.json"))) else {
        return vec![];
    };
    let mut workspaces: Vec<UploadedWorkspace> = vec![];
    for receipt in journal.uploads {
        if receipt.stage != "done" || receipt.record_id.is_none() {
            continue;
        }
        let name = receipt
            .workspace_name
            .unwrap_or_else(|| receipt.workspace.clone());
        if let Some(existing) = workspaces.iter_mut().find(|w| w.slug == receipt.workspace) {
            existing.name = name;
        } else {
            workspaces.push(UploadedWorkspace {
                slug: receipt.workspace,
                name,
            });
        }
    }
    workspaces
}

fn clients() -> Result<(Client, Client), String> {
    let build = |timeout| {
        Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "Cannot initialize Callabo HTTP client".to_string())
    };
    Ok((
        build(Duration::from_secs(60))?,
        build(Duration::from_secs(1800))?,
    ))
}

fn validate_token(token: &str) -> Result<&str, String> {
    let token = token.trim();
    if token.is_empty()
        || token.chars().any(|c| c.is_whitespace() || c.is_control())
        || !token.is_ascii()
    {
        return Err("Enter a Callabo Personal Access Token in Settings.".into());
    }
    Ok(token)
}

fn endpoint(
    api: &str,
    workspace: &str,
    record: Option<u64>,
    suffix: &[&str],
) -> Result<Url, String> {
    if workspace.is_empty()
        || workspace == "."
        || workspace == ".."
        || workspace
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
    {
        return Err("Select a Callabo workspace in Settings.".into());
    }
    let mut url = Url::parse(api).map_err(|_| "Invalid Callabo API URL")?;
    {
        let mut parts = url
            .path_segments_mut()
            .map_err(|_| "Invalid Callabo API URL")?;
        parts
            .clear()
            .extend(["v1", "workspace", workspace, "record"]);
        if let Some(id) = record {
            parts.push(&id.to_string());
        }
        parts.extend(suffix);
    }
    Ok(url)
}

async fn check(
    result: Result<Response, reqwest::Error>,
    operation: &str,
) -> Result<Response, String> {
    // Do not include URLs, reflected response bodies, or reqwest Debug strings:
    // they can expose the PAT or signed storage URL in persistent application logs.
    let response = result.map_err(|_| {
        format!("Callabo {operation}: network error or timeout. Check your connection.")
    })?;
    if !response.status().is_success() {
        let hint = match response.status().as_u16() {
            401 => " Check your Personal Access Token.",
            403 => " Check workspace access and whether PAT API access is enabled.",
            413 => " The recording exceeds the server's upload limit.",
            429 => " Rate limited; try again later.",
            _ => "",
        };
        return Err(format!(
            "Callabo {operation}: HTTP {}.{hint}",
            response.status().as_u16()
        ));
    }
    Ok(response)
}

async fn json_response(response: Response) -> Result<Value, String> {
    response
        .json()
        .await
        .map_err(|_| "Callabo returned an unexpected response.".into())
}

async fn workspaces(api: &str, token: &str) -> Result<Vec<Workspace>, String> {
    let token = validate_token(token)?;
    let (client, _) = clients()?;
    let body = json_response(
        check(
            client
                .get(format!("{api}/v1/workspace"))
                .bearer_auth(token)
                .send()
                .await,
            "workspace lookup",
        )
        .await?,
    )
    .await?;
    let items = body
        .as_array()
        .ok_or("Callabo workspace response is not a list.")?;
    let result: Vec<_> = items
        .iter()
        .filter_map(|item| {
            // Same eligibility check as the official CLI's PAT login.
            if let Some(pat) = item.pointer("/option/additional_options/personal_access_token") {
                if !pat.is_null()
                    && (!pat.is_object()
                        || pat
                            .get("enabled")
                            .is_some_and(|enabled| !enabled.is_null() && enabled != true))
                {
                    return None;
                }
            }
            let slug = item
                .get("slug")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .or_else(|| item.get("temp_slug").and_then(Value::as_str))?
                .to_owned();
            Some(Workspace {
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(&slug)
                    .to_owned(),
                slug,
            })
        })
        .collect();
    if result.is_empty() {
        return Err(
            "No accessible Callabo workspaces. Check PAT API access in workspace settings.".into(),
        );
    }
    Ok(result)
}

fn saved_token() -> Result<String, String> {
    crate::callabo_secret::load()?
        .ok_or_else(|| "Save a Callabo Personal Access Token in Settings first.".into())
}

#[tauri::command]
pub fn callabo_auth_status() -> Result<bool, String> {
    Ok(crate::callabo_secret::load()?.is_some())
}

#[tauri::command]
pub fn callabo_forget_token() -> Result<(), String> {
    crate::callabo_secret::forget()
}

#[tauri::command]
pub async fn callabo_workspaces(token: Option<String>) -> Result<Vec<Workspace>, String> {
    let candidate = match &token {
        Some(t) => validate_token(t)?.to_owned(),
        None => saved_token()?,
    };
    // Verify first: an invalid replacement must not destroy a working saved PAT.
    let result = workspaces(API, &candidate).await?;
    if token.is_some() {
        crate::callabo_secret::save(&candidate)?;
    }
    Ok(result)
}

fn resource_url(api: &str, workspace: &str, resource: &str) -> Result<Url, String> {
    endpoint(api, workspace, None, &[])?; // shared slug validation
    let mut url = Url::parse(api).map_err(|_| "Invalid Callabo API URL")?;
    url.path_segments_mut()
        .map_err(|_| "Invalid Callabo API URL")?
        .clear()
        .extend(["v2", "workspaces", workspace, resource]);
    Ok(url)
}

async fn teams(api: &str, token: &str, workspace: &str) -> Result<Vec<Team>, String> {
    let (client, _) = clients()?;
    let mut result = Vec::new();
    for page in 0..100u32 {
        let mut url = resource_url(api, workspace, "teams")?;
        url.query_pairs_mut()
            .append_pair("limit", "100")
            .append_pair("offset", &(page * 100).to_string());
        let body = json_response(
            check(
                client.get(url).bearer_auth(token).send().await,
                "team lookup",
            )
            .await?,
        )
        .await?;
        let items = body
            .get("items")
            .and_then(Value::as_array)
            .ok_or("Unexpected Callabo team list")?;
        for item in items {
            result.push(
                serde_json::from_value(item.clone()).map_err(|_| "Unexpected Callabo team data")?,
            );
        }
        if items.len() < 100
            || body
                .pointer("/page/total")
                .and_then(Value::as_u64)
                .is_some_and(|total| result.len() as u64 >= total)
        {
            return Ok(result);
        }
    }
    Err("Callabo team list is too large; narrow your account's workspace access.".into())
}

async fn labels(api: &str, token: &str, workspace: &str) -> Result<Vec<Label>, String> {
    let (client, _) = clients()?;
    let body = json_response(
        check(
            client
                .get(resource_url(api, workspace, "labels")?)
                .bearer_auth(token)
                .send()
                .await,
            "label lookup",
        )
        .await?,
    )
    .await?;
    serde_json::from_value(
        body.get("items")
            .ok_or("Unexpected Callabo label list")?
            .clone(),
    )
    .map_err(|_| "Unexpected Callabo label data".into())
}

#[tauri::command]
pub async fn callabo_dialog_data(workspace: String, app: AppHandle) -> Result<DialogData, String> {
    endpoint(API, &workspace, None, &[])?;
    let token = saved_token()?;
    let (team_result, label_result) = tokio::join!(
        teams(API, &token, &workspace),
        labels(API, &token, &workspace)
    );
    let mut warnings = Vec::new();
    let teams_loaded = team_result.is_ok();
    let labels_loaded = label_result.is_ok();
    let teams = team_result.unwrap_or_else(|e| {
        warnings.push(e);
        vec![]
    });
    let labels = label_result.unwrap_or_else(|e| {
        warnings.push(e);
        vec![]
    });
    let preferences = Config::load(&app)
        .callabo_upload_settings
        .get(&workspace)
        .cloned()
        .unwrap_or_default();
    Ok(DialogData {
        teams,
        labels,
        preferences,
        teams_loaded,
        labels_loaded,
        warnings,
    })
}

#[tauri::command]
pub fn set_callabo_workspace(workspace: Option<String>, app: AppHandle) -> Result<(), String> {
    if let Some(slug) = &workspace {
        endpoint(API, slug, None, &[])?;
    }
    let mut config = Config::load(&app);
    config.callabo_workspace = workspace;
    config.save(&app)
}

async fn upload(
    api: &str,
    token: &str,
    workspace: &str,
    path: &Path,
    journal: &Path,
    options: UploadOptions,
    progress: impl Fn(&str),
) -> Result<Receipt, String> {
    let token = validate_token(token)?;
    let options = options.normalized()?;
    let create_url = endpoint(api, workspace, None, &[])?;
    // Open once before any external write; stream from this same handle.
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| "A combined WAV is required. Merge legacy microphone/system files first.")?;
    let metadata = file
        .metadata()
        .await
        .map_err(|_| "Cannot read recording metadata.")?;
    if !metadata.is_file() || metadata.len() <= 44 {
        return Err("The recording is empty or unavailable.".into());
    }
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("Invalid recording filename")?;
    let (client, storage) = clients()?;
    // Completed uploads never lock a recording. Only an unfinished attempt in
    // this workspace is resumed; attempts in other workspaces stay untouched.
    let history = Journal::load(journal)?;
    let mut receipt = match history
        .uploads
        .into_iter()
        .rev()
        .find(|r| r.workspace == workspace && r.stage != "done")
    {
        Some(existing) => {
            if existing.filesize != metadata.len() {
                return Err("An earlier upload exists for a different file. Review the .callabo.json receipt before retrying.".into());
            }
            if existing.options.as_ref() != Some(&options) {
                return Err("An unfinished upload exists with different options. Reuse the original choices or review the .callabo.json receipt before retrying.".into());
            }
            if existing.record_id.is_none() || existing.stage == "completing" {
                return Err(format!("The previous Callabo request had an uncertain result (record {:?}). Check Callabo and the .callabo.json receipt before retrying; no duplicate was created.", existing.record_id));
            }
            existing
        }
        None => Receipt {
            workspace: workspace.to_owned(),
            workspace_name: None,
            record_id: None,
            uuid: uuid::Uuid::new_v4().to_string(),
            filesize: metadata.len(),
            stage: "creating".into(),
            options: Some(options.clone()),
        },
    };
    if receipt.record_id.is_none() {
        // Save before POST, so a timeout/crash cannot silently create a duplicate.
        receipt.save(journal)?;
        progress("creating");
        let rec_date: chrono::DateTime<chrono::Utc> = metadata
            .modified()
            .map_err(|_| "Cannot read recording date")?
            .into();
        let mut payload = json!({
            "uuid": receipt.uuid, "source": "manual_upload", "upload_source": "cli",
            "filesize": metadata.len(), "duration": null, "rec_date": rec_date.to_rfc3339(),
            "scope": options.scope, "transcribe_language": options.preferences.transcribe_language, "save_media": "default",
            "save_dialog": "default", "filename": filename,
        });
        if let Some(title) = &options.title {
            payload["title"] = json!(title);
        }
        for (key, ids) in [
            ("team_ids", &options.preferences.team_ids),
            ("label_ids", &options.preferences.label_ids),
            (
                "accessible_team_ids",
                &options.preferences.accessible_team_ids,
            ),
            (
                "accessible_user_ids",
                &options.preferences.accessible_user_ids,
            ),
        ] {
            if !ids.is_empty() {
                payload[key] = json!(ids);
            }
        }
        let created = check(
            client
                .post(create_url)
                .bearer_auth(token)
                .json(&payload)
                .send()
                .await,
            "record creation",
        )
        .await;
        // Definitive client rejection cannot have created a record. Allow a
        // corrected token/permission retry, but retain uncertain network/5xx results.
        if let Err(error) = &created {
            if [
                "HTTP 400.",
                "HTTP 401.",
                "HTTP 403.",
                "HTTP 404.",
                "HTTP 413.",
                "HTTP 422.",
                "HTTP 429.",
            ]
            .iter()
            .any(|code| error.contains(code))
            {
                Journal::remove_attempt(journal, &receipt.uuid)?;
            }
        }
        let body = json_response(created?).await?;
        receipt.record_id = Some(
            body.get("id")
                .and_then(Value::as_u64)
                .filter(|id| *id > 0)
                .ok_or("Callabo did not return a record ID. Check Callabo before retrying.")?,
        );
        receipt.stage = "uploading".into();
        receipt.save(journal)?;
    }
    let id = receipt.record_id.ok_or("Missing Callabo record ID")?;
    progress("uploading");
    let url_value = json_response(
        check(
            client
                .post(endpoint(api, workspace, Some(id), &["upload"])?)
                .bearer_auth(token)
                .send()
                .await,
            "upload URL",
        )
        .await?,
    )
    .await?;
    let signed = Url::parse(
        url_value
            .as_str()
            .ok_or("Callabo did not return an upload URL")?,
    )
    .map_err(|_| "Callabo returned an invalid upload URL")?;
    // HTTP is permitted only for the loopback test server, never in production.
    let secure = signed.scheme() == "https"
        || (cfg!(test) && signed.scheme() == "http" && signed.host_str() == Some("127.0.0.1"));
    if !secure || !signed.username().is_empty() || signed.password().is_some() {
        return Err("Callabo returned an unsafe upload URL.".into());
    }
    let filename_encoded: String = filename
        .as_bytes()
        .iter()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(b) {
                (*b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    check(
        storage
            .put(signed)
            .header("Content-Type", "audio/wav")
            .header("Content-Length", metadata.len())
            .header(
                "Content-Disposition",
                format!("attachment; filename*=UTF-8''{filename_encoded}"),
            )
            // Deliberately no PAT header on the storage request.
            .body(reqwest::Body::from(file))
            .send()
            .await,
        "file transfer",
    )
    .await?;
    receipt.stage = "completing".into();
    receipt.save(journal)?;
    progress("completing");
    check(
        client
            .post(endpoint(api, workspace, Some(id), &["upload", "complete"])?)
            .bearer_auth(token)
            .json(&json!({"uuid": receipt.uuid}))
            .send()
            .await,
        "upload completion",
    )
    .await?;
    receipt.stage = "done".into();
    receipt.save(journal)?;
    progress("done");
    Ok(receipt)
}

#[tauri::command]
pub async fn callabo_upload(
    folder: Option<String>,
    base: String,
    workspace: String,
    workspace_name: Option<String>,
    options: UploadOptions,
    app: AppHandle,
) -> Result<Receipt, String> {
    let state = app.state::<Uploads>();
    let token = saved_token()?;
    let options = options.normalized()?;
    let _guard = state.start((folder.clone(), base.clone()))?;
    if recording_busy(
        &app.state::<Status>(),
        &app.state::<TranscribeQueue>(),
        folder.as_deref(),
        &base,
    ) {
        return Err(
            "Wait until recording and transcription finish before uploading to Callabo.".into(),
        );
    }
    let dir = recording_dir(&recordings_root(), folder.as_deref(), &base, false)?;
    let path: PathBuf = dir.join(format!("{base}.wav"));
    let journal = dir.join(format!("{base}.callabo.json"));
    let mut receipt = upload(
        API,
        &token,
        &workspace,
        &path,
        &journal,
        options.clone(),
        |stage| {
            let _ = app.emit(
                "callabo-progress",
                json!({"folder": folder, "base": base, "stage": stage}),
            );
        },
    )
    .await?;
    receipt.workspace_name = workspace_name
        .map(|name| {
            name.trim()
                .chars()
                .filter(|c| !c.is_control())
                .take(200)
                .collect::<String>()
        })
        .filter(|name| !name.is_empty());
    // A naming/history failure must not turn an already completed remote upload
    // into an apparent failure that invites a duplicate retry.
    if receipt.save(&journal).is_err() {
        let _ = app.emit(
            "callabo-settings-warning",
            "Upload completed, but the workspace display name could not be saved.",
        );
    }
    let mut config = Config::load(&app);
    config
        .callabo_upload_settings
        .insert(workspace, options.preferences);
    // Remote upload is already complete. Do not report a false upload failure
    // or invite duplicate retries if only saving local defaults fails.
    if config.save(&app).is_err() {
        let _ = app.emit(
            "callabo-settings-warning",
            "Upload completed, but workspace upload preferences could not be saved.",
        );
    }
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn upload_options_validate_and_default_to_workspace_auto_title() {
        let defaults: UploadOptions = serde_json::from_value(json!({})).unwrap();
        let normalized = defaults.normalized().unwrap();
        assert_eq!(normalized.scope, "workspace");
        assert_eq!(normalized.title, None);
        assert_eq!(normalized.preferences.transcribe_language, "default");
        assert!(UploadOptions {
            scope: "team".into(),
            ..Default::default()
        }
        .normalized()
        .is_err());
        assert!(UploadOptions {
            scope: "public".into(),
            ..Default::default()
        }
        .normalized()
        .is_err());
        let options: UploadOptions = serde_json::from_value(
            json!({"title":"  ","scope":"team","team_ids":[7,7,3],"transcribe_language":"ko"}),
        )
        .unwrap();
        let options = options.normalized().unwrap();
        assert_eq!(options.title, None);
        assert_eq!(options.preferences.team_ids, [3, 7]);
        for raw in [
            json!({"team_ids":[0]}),
            json!({"transcribe_language":"fake"}),
            json!({"title":"bad\nname"}),
        ] {
            assert!(serde_json::from_value::<UploadOptions>(raw)
                .unwrap()
                .normalized()
                .is_err());
        }
    }

    #[tokio::test]
    async fn custom_dialog_choices_are_sent_and_changed_retry_options_are_rejected() {
        let scratch = Scratch::new();
        let audio = scratch.audio();
        let options: UploadOptions = serde_json::from_value(json!({
            "title":"  Custom meeting  ","scope":"team","team_ids":[7],"label_ids":[8],
            "accessible_team_ids":[9],"accessible_user_ids":[10],"transcribe_language":"ko"
        }))
        .unwrap();
        let (base, requests, task) = server(
            vec![201, 200, 503],
            vec![
                r#"{"id":43}"#.into(),
                r#""$BASE/storage""#.into(),
                "failure".into(),
            ],
        )
        .await;
        assert!(upload(
            &base,
            "pat_fake",
            "a",
            &audio,
            &scratch.journal(),
            options,
            |_| {}
        )
        .await
        .is_err());
        task.await.unwrap();
        let payload: Value = serde_json::from_slice(&requests.lock().unwrap()[0].body).unwrap();
        assert_eq!(payload["title"], "Custom meeting");
        assert_eq!(payload["scope"], "team");
        assert_eq!(payload["team_ids"], json!([7]));
        assert_eq!(payload["label_ids"], json!([8]));
        assert_eq!(payload["accessible_team_ids"], json!([9]));
        assert_eq!(payload["accessible_user_ids"], json!([10]));
        assert_eq!(payload["transcribe_language"], "ko");
        let err = upload(
            &base,
            "pat_fake",
            "a",
            &audio,
            &scratch.journal(),
            UploadOptions::default(),
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(err.contains("different options"));
    }

    #[tokio::test]
    async fn team_lookup_paginates_and_preserves_template_information() {
        let page1 = json!({"items":(1..=100).map(|id|json!({"id":id,"name":format!("Team {id}")})).collect::<Vec<_>>(),"page":{"total":101}}).to_string();
        let page2 = json!({"items":[{"id":101,"name":"Custom","request_insight_extract_type":"custom","default_custom_insight_template_id":99}],"page":{"total":101}}).to_string();
        let (base, requests, task) = server(vec![200, 200], vec![page1, page2]).await;
        let teams = teams(&base, "pat_fake", "a").await.unwrap();
        task.await.unwrap();
        assert_eq!(teams.len(), 101);
        assert_eq!(teams[100].default_custom_insight_template_id, Some(99));
        assert!(requests.lock().unwrap()[1]
            .head
            .starts_with("GET /v2/workspaces/a/teams?limit=100&offset=100 "));
    }

    #[tokio::test]
    async fn label_lookup_uses_workspace_scoped_public_api() {
        let (base, requests, task) = server(
            vec![200],
            vec![r#"{"items":[{"id":5,"name":"Work"}]}"#.into()],
        )
        .await;
        let labels = labels(&base, "pat_fake", "b").await.unwrap();
        task.await.unwrap();
        assert_eq!(labels[0].id, 5);
        assert!(requests.lock().unwrap()[0]
            .head
            .starts_with("GET /v2/workspaces/b/labels "));
    }

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("meetrec-callabo-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }
        fn audio(&self) -> PathBuf {
            let path = self.0.join("2026-10-01_11-13_회의.wav");
            let spec = hound::WavSpec {
                channels: 2,
                sample_rate: 16000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let mut writer = hound::WavWriter::create(&path, spec).unwrap();
            for n in 0..32 {
                writer.write_sample(n as i16).unwrap();
            }
            writer.finalize().unwrap();
            path
        }
        fn journal(&self) -> PathBuf {
            self.0.join("record.callabo.json")
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Debug)]
    struct Request {
        head: String,
        body: Vec<u8>,
    }

    async fn server(
        statuses: Vec<u16>,
        bodies: Vec<String>,
    ) -> (
        String,
        Arc<Mutex<Vec<Request>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let address = base.clone();
        let task = tokio::spawn(async move {
            for (status, body) in statuses.into_iter().zip(bodies) {
                let (mut socket, _) =
                    tokio::time::timeout(Duration::from_secs(15), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let mut data = Vec::new();
                let mut chunk = [0u8; 2048];
                let header_end = loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    data.extend_from_slice(&chunk[..n]);
                    if let Some(index) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        break index + 4;
                    }
                };
                let head = String::from_utf8(data[..header_end].to_vec()).unwrap();
                let length = head
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|s| s.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while data.len() < header_end + length {
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    data.extend_from_slice(&chunk[..n]);
                }
                captured.lock().unwrap().push(Request {
                    head,
                    body: data[header_end..header_end + length].to_vec(),
                });
                let body = body.replace("$BASE", &address);
                let response = format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (base, requests, task)
    }

    #[test]
    fn upload_guard_rejects_duplicates_and_releases_on_drop() {
        let state = Uploads::default();
        let key = (Some("2026-10".into()), "meeting".into());
        let guard = state.start(key.clone()).unwrap();
        assert!(state.busy(Some("2026-10"), "meeting"));
        assert!(state.start(key.clone()).is_err());
        assert!(state
            .while_idle(Some("2026-10"), "meeting", || -> Result<(), String> {
                panic!("busy recording must not be mutated")
            })
            .is_err());
        assert!(!state.busy(Some("2026-09"), "meeting"));
        drop(guard);
        assert!(state.start(key).is_ok());
    }

    #[test]
    fn validates_token_and_encodes_workspace_without_path_injection() {
        for token in ["", "pat_secret\r\nX: abc", "\u{7f}", "비밀"] {
            assert!(validate_token(token).is_err());
        }
        assert_eq!(validate_token(" pat_fake ").unwrap(), "pat_fake");
        for slug in ["", "..", "../other", "a\\b"] {
            assert!(endpoint(API, slug, None, &[]).is_err());
        }
        assert_eq!(
            endpoint(API, "a?b#c", Some(9), &["upload"]).unwrap().path(),
            "/v1/workspace/a%3Fb%23c/record/9/upload"
        );
    }

    #[tokio::test]
    async fn uploads_exact_wav_with_automatic_title_without_leaking_token_to_storage() {
        let scratch = Scratch::new();
        let audio = scratch.audio();
        let (base, requests, task) = server(
            vec![201, 200, 200, 200],
            vec![
                r#"{"id":42}"#.into(),
                r#""$BASE/storage?signature=secret""#.into(),
                "".into(),
                r#"{"id":42}"#.into(),
            ],
        )
        .await;
        let stages = Mutex::new(Vec::new());
        let receipt = upload(
            &base,
            "pat_test_secret",
            "workspace",
            &audio,
            &scratch.journal(),
            UploadOptions::default(),
            |s| stages.lock().unwrap().push(s.to_owned()),
        )
        .await
        .unwrap();
        task.await.unwrap();
        assert_eq!(receipt.record_id, Some(42));
        assert_eq!(
            stages.into_inner().unwrap(),
            ["creating", "uploading", "completing", "done"]
        );
        let reqs = requests.lock().unwrap();
        assert!(reqs[0]
            .head
            .starts_with("POST /v1/workspace/workspace/record "));
        let payload: Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(payload["scope"], "workspace");
        assert!(payload.get("title").is_none());
        assert_eq!(payload["filename"], "2026-10-01_11-13_회의.wav");
        assert_eq!(payload["transcribe_language"], "default");
        assert_eq!(
            payload["filesize"],
            std::fs::metadata(&audio).unwrap().len()
        );
        assert!(uuid::Uuid::parse_str(payload["uuid"].as_str().unwrap()).is_ok());
        assert!(reqs[1]
            .head
            .starts_with("POST /v1/workspace/workspace/record/42/upload "));
        assert!(reqs[2].head.starts_with("PUT /storage?signature=secret "));
        assert!(!reqs[2].head.to_lowercase().contains("authorization:"));
        assert!(!reqs[2].head.contains("pat_test_secret"));
        assert!(reqs[2]
            .head
            .contains("filename*=UTF-8''2026-10-01_11-13_%ED%9A%8C%EC%9D%98.wav"));
        assert_eq!(reqs[2].body, std::fs::read(&audio).unwrap());
        assert!(reqs[3]
            .head
            .starts_with("POST /v1/workspace/workspace/record/42/upload/complete "));
        let completed: Value = serde_json::from_slice(&reqs[3].body).unwrap();
        assert_eq!(completed["uuid"], payload["uuid"]);
        for index in [0, 1, 3] {
            assert!(reqs[index]
                .head
                .to_lowercase()
                .contains("authorization: bearer pat_test_secret"));
        }
        let journal = std::fs::read_to_string(scratch.journal()).unwrap();
        assert!(!journal.contains("pat_test_secret") && !journal.contains("signature"));
        assert_eq!(Journal::load(&scratch.journal()).unwrap().uploads.len(), 1);
    }

    #[tokio::test]
    async fn failed_transfer_reuses_record_and_hides_signed_url_and_response_body() {
        let scratch = Scratch::new();
        let audio = scratch.audio();
        let (base, _, task) = server(
            vec![201, 200, 503],
            vec![
                r#"{"id":7}"#.into(),
                r#""$BASE/storage?signature=do-not-log""#.into(),
                "pat_test_secret do-not-log".into(),
            ],
        )
        .await;
        let err = upload(
            &base,
            "pat_test_secret",
            "workspace",
            &audio,
            &scratch.journal(),
            UploadOptions::default(),
            |_| {},
        )
        .await
        .unwrap_err();
        task.await.unwrap();
        assert!(err.contains("HTTP 503"));
        assert!(!err.contains("pat_test_secret") && !err.contains("do-not-log"));
        let (base, requests, task) = server(
            vec![200, 200, 200],
            vec![r#""$BASE/storage""#.into(), "".into(), r#"{"id":7}"#.into()],
        )
        .await;
        let receipt = upload(
            &base,
            "pat_test_secret",
            "workspace",
            &audio,
            &scratch.journal(),
            UploadOptions::default(),
            |_| {},
        )
        .await
        .unwrap();
        task.await.unwrap();
        assert_eq!(receipt.record_id, Some(7));
        assert!(requests.lock().unwrap()[0]
            .head
            .starts_with("POST /v1/workspace/workspace/record/7/upload "));
    }

    #[tokio::test]
    async fn rejected_creation_can_retry_but_uncertain_creation_cannot_duplicate() {
        let scratch = Scratch::new();
        let audio = scratch.audio();
        for (status, journal_expected) in [(401, false), (503, true)] {
            let (base, _, task) = server(vec![status], vec!["secret reflected token".into()]).await;
            let err = upload(
                &base,
                "pat_test_secret",
                "workspace",
                &audio,
                &scratch.journal(),
                UploadOptions::default(),
                |_| {},
            )
            .await
            .unwrap_err();
            task.await.unwrap();
            assert!(err.contains(&format!("HTTP {status}")));
            assert_eq!(scratch.journal().exists(), journal_expected);
        }
        let err = upload(
            "http://127.0.0.1:1",
            "pat_test_secret",
            "workspace",
            &audio,
            &scratch.journal(),
            UploadOptions::default(),
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(err.contains("uncertain result"));
    }

    #[tokio::test]
    async fn workspaces_respect_pat_access_and_support_legacy_slugs() {
        let (base, _, task) = server(vec![200], vec![r#"[
            {"slug":"allowed","name":"Team"},
            {"slug":null,"temp_slug":"legacy"},
            {"slug":"disabled","option":{"additional_options":{"personal_access_token":{"enabled":false}}}},
            {"slug":"invalid","option":{"additional_options":{"personal_access_token":"no"}}}
        ]"#.into()]).await;
        let spaces = workspaces(&base, "pat_test_secret").await.unwrap();
        task.await.unwrap();
        assert_eq!(
            spaces.iter().map(|w| w.slug.as_str()).collect::<Vec<_>>(),
            ["allowed", "legacy"]
        );
    }

    #[tokio::test]
    async fn incomplete_or_unsafe_upload_does_not_send_file() {
        let scratch = Scratch::new();
        let audio = scratch.audio();
        let (base, requests, task) = server(
            vec![201, 200],
            vec![
                r#"{"id":9}"#.into(),
                r#""http://remote.invalid/file""#.into(),
            ],
        )
        .await;
        let err = upload(
            &base,
            "pat_test_secret",
            "workspace",
            &audio,
            &scratch.journal(),
            UploadOptions::default(),
            |_| {},
        )
        .await
        .unwrap_err();
        task.await.unwrap();
        assert!(err.contains("unsafe upload URL"));
        assert_eq!(requests.lock().unwrap().len(), 2);
        let mut receipt = Journal::load(&scratch.journal()).unwrap().uploads.remove(0);
        receipt.stage = "completing".into();
        receipt.save(&scratch.journal()).unwrap();
        let err = upload(
            &base,
            "pat_test_secret",
            "workspace",
            &audio,
            &scratch.journal(),
            UploadOptions::default(),
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(err.contains("uncertain result"));
    }

    fn previous_receipt(workspace: &str, stage: &str) -> Receipt {
        Receipt {
            workspace: workspace.into(),
            workspace_name: Some(format!("Workspace {workspace}")),
            record_id: Some(123),
            uuid: uuid::Uuid::new_v4().to_string(),
            filesize: 108,
            stage: stage.into(),
            options: Some(UploadOptions::default()),
        }
    }

    #[test]
    fn legacy_receipt_migrates_without_losing_ids_and_history_deduplicates_workspaces() {
        let scratch = Scratch::new();
        let mut legacy = previous_receipt("a", "done");
        legacy.workspace_name = None;
        let mut legacy_json = serde_json::to_value(&legacy).unwrap();
        legacy_json
            .as_object_mut()
            .unwrap()
            .remove("workspace_name");
        std::fs::write(scratch.journal(), serde_json::to_vec(&legacy_json).unwrap()).unwrap();
        assert_eq!(
            completed_workspaces(&scratch.0, "record"),
            vec![UploadedWorkspace {
                slug: "a".into(),
                name: "a".into()
            }]
        );
        previous_receipt("b", "done")
            .save(&scratch.journal())
            .unwrap();
        previous_receipt("a", "done")
            .save(&scratch.journal())
            .unwrap();
        previous_receipt("c", "uploading")
            .save(&scratch.journal())
            .unwrap();
        let history = Journal::load(&scratch.journal()).unwrap();
        assert_eq!(history.uploads.len(), 4);
        assert_eq!(history.uploads[0].uuid, legacy.uuid);
        assert_eq!(history.uploads[0].record_id, legacy.record_id);
        assert_eq!(
            completed_workspaces(&scratch.0, "record"),
            vec![
                UploadedWorkspace {
                    slug: "a".into(),
                    name: "Workspace a".into()
                },
                UploadedWorkspace {
                    slug: "b".into(),
                    name: "Workspace b".into()
                },
            ]
        );
    }

    #[tokio::test]
    async fn multiple_uploads_to_other_and_same_workspaces_preserve_history() {
        let scratch = Scratch::new();
        let audio = scratch.audio();
        for (workspace, id) in [("a", 1), ("b", 2), ("a", 3)] {
            let (base, requests, task) = server(
                vec![201, 200, 200, 200],
                vec![
                    format!("{{\"id\":{id}}}"),
                    r#""$BASE/storage""#.into(),
                    "".into(),
                    "".into(),
                ],
            )
            .await;
            let receipt = upload(
                &base,
                "pat_test_secret",
                workspace,
                &audio,
                &scratch.journal(),
                UploadOptions::default(),
                |_| {},
            )
            .await
            .unwrap();
            task.await.unwrap();
            assert_eq!(receipt.record_id, Some(id));
            assert!(requests.lock().unwrap()[0]
                .head
                .starts_with(&format!("POST /v1/workspace/{workspace}/record ")));
        }
        let history = Journal::load(&scratch.journal()).unwrap();
        assert_eq!(
            history
                .uploads
                .iter()
                .map(|r| r.record_id.unwrap())
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(
            history
                .uploads
                .iter()
                .map(|r| &r.uuid)
                .collect::<HashSet<_>>()
                .len(),
            3
        );
        assert_eq!(
            completed_workspaces(&scratch.0, "record")
                .iter()
                .map(|w| w.slug.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        // Rejection of a new upload cannot delete any completed history.
        let (base, _, task) = server(vec![403], vec!["".into()]).await;
        assert!(upload(
            &base,
            "pat_test_secret",
            "a",
            &audio,
            &scratch.journal(),
            UploadOptions::default(),
            |_| {}
        )
        .await
        .unwrap_err()
        .contains("HTTP 403"));
        task.await.unwrap();
        assert_eq!(Journal::load(&scratch.journal()).unwrap().uploads.len(), 3);
    }

    #[tokio::test]
    async fn uncertain_attempt_only_blocks_its_own_workspace() {
        let scratch = Scratch::new();
        let audio = scratch.audio();
        let mut uncertain = previous_receipt("a", "creating");
        uncertain.record_id = None;
        uncertain.save(&scratch.journal()).unwrap();
        let (base, _, task) = server(
            vec![201, 200, 200, 200],
            vec![
                r#"{"id":2}"#.into(),
                r#""$BASE/storage""#.into(),
                "".into(),
                "".into(),
            ],
        )
        .await;
        upload(
            &base,
            "pat_test_secret",
            "b",
            &audio,
            &scratch.journal(),
            UploadOptions::default(),
            |_| {},
        )
        .await
        .unwrap();
        task.await.unwrap();
        let err = upload(
            &base,
            "pat_test_secret",
            "a",
            &audio,
            &scratch.journal(),
            UploadOptions::default(),
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(err.contains("uncertain result"));
        let history = Journal::load(&scratch.journal()).unwrap();
        assert_eq!(history.uploads.len(), 2);
        assert_eq!(history.uploads[0].uuid, uncertain.uuid);
        assert_eq!(completed_workspaces(&scratch.0, "record").len(), 1);
    }

    #[test]
    fn malformed_history_is_never_overwritten() {
        let scratch = Scratch::new();
        let bytes = b"{ damaged history";
        std::fs::write(scratch.journal(), bytes).unwrap();
        assert!(previous_receipt("a", "done")
            .save(&scratch.journal())
            .is_err());
        assert_eq!(std::fs::read(scratch.journal()).unwrap(), bytes);
    }
}
