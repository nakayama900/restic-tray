//! Pure-Rust Cloudflare R2 (S3-compatible) API client for deleting objects under a prefix
//! or emptying and deleting an entire R2 bucket.
//!
//! - Uses AWS Signature Version 4 (`AWS4-HMAC-SHA256`) over HTTPS (Windows Schannel via `ureq`).
//! - Never writes credentials to disk or spawns external shells; derived signing keys are zeroized.
//! - Supports dry-run previews, progress reporting, cancellation, incomplete multipart upload
//!   cleanup, and graceful handling when `DeleteBucket` returns `403 AccessDenied` (Object R/W token).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use md5::{Digest as _, Md5};
use quick_xml::Reader;
use quick_xml::events::Event;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroize;

use crate::config::R2Config;
use crate::error::{AppError, AppResult};
use crate::secrets::StoredSecrets;

type HmacSha256 = Hmac<Sha256>;

const MAX_DELETE_BATCH: usize = 1000;
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);

/// Scope of a Cloudflare R2 deletion operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum R2DeleteScope {
    /// Delete only objects and incomplete multipart uploads under `<prefix>/`.
    /// Requires a non-empty `r2.prefix` so an entire bucket is never wiped by mistake.
    PrefixOnly,
    /// Empty the entire bucket (all objects and incomplete multipart uploads) and attempt
    /// S3 `DeleteBucket`.
    EntireBucket,
}

impl R2DeleteScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PrefixOnly => "prefixOnly",
            Self::EntireBucket => "entireBucket",
        }
    }
}

/// Real-time progress for an in-flight R2 scan or deletion.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct R2DeleteProgress {
    pub running: bool,
    pub dry_run: bool,
    pub phase: String,
    pub bucket: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    pub objects_scanned: u64,
    pub objects_deleted: u64,
    pub bytes_scanned: u64,
    pub bytes_deleted: u64,
    pub multipart_aborted: u64,
}

/// Summary returned after an R2 preview (`dry_run: true`) or deletion (`dry_run: false`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct R2DeleteReport {
    pub scope: R2DeleteScope,
    pub dry_run: bool,
    pub bucket: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    pub objects_found: u64,
    pub bytes_found: u64,
    pub objects_deleted: u64,
    pub bytes_deleted: u64,
    pub multipart_uploads_found: u64,
    pub multipart_uploads_aborted: u64,
    pub bucket_deleted: bool,
    /// True when `EntireBucket` emptied all objects/uploads, but `DeleteBucket` returned
    /// `403 AccessDenied` (expected for bucket-scoped Object Read & Write tokens).
    pub bucket_delete_access_denied: bool,
    /// True when R2 reported `404 NoSuchBucket`.
    pub bucket_not_found: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// RAII guard that ensures `progress.running` is always cleared on exit.
struct ProgressGuard<'a> {
    state: &'a Arc<Mutex<R2DeleteProgress>>,
}

impl Drop for ProgressGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.state.lock() {
            guard.running = false;
        }
    }
}

fn update_progress(state: &Arc<Mutex<R2DeleteProgress>>, f: impl FnOnce(&mut R2DeleteProgress)) {
    if let Ok(mut guard) = state.lock() {
        f(&mut guard);
    }
}

fn check_cancelled(cancel_flag: &Arc<AtomicBool>) -> AppResult<()> {
    if cancel_flag.load(Ordering::Relaxed) {
        return Err(AppError::R2("R2 の削除処理を中止しました".to_owned()));
    }
    Ok(())
}

/// Scans (`dry_run: true`) or deletes (`dry_run: false`) objects under the configured R2 prefix
/// or empties and deletes the entire configured R2 bucket.
pub fn delete_r2_data(
    r2: &R2Config,
    secrets: &StoredSecrets,
    scope: R2DeleteScope,
    dry_run: bool,
    progress_state: &Arc<Mutex<R2DeleteProgress>>,
    cancel_flag: &Arc<AtomicBool>,
) -> AppResult<R2DeleteReport> {
    if !r2.is_configured() {
        return Err(AppError::Config(
            "Cloudflare R2 のアカウント ID とバケット名が設定されていません".to_owned(),
        ));
    }

    let bucket = r2.normalized_bucket();
    let clean_prefix = r2.normalized_prefix();
    let list_prefix: Option<String> = match scope {
        R2DeleteScope::PrefixOnly => {
            if clean_prefix.is_empty() {
                return Err(AppError::InvalidArgument(
                    "プレフィックスが空のため「プレフィックス配下のみ削除」は実行できません。バケット全体を削除する場合は entireBucket を指定してください"
                        .to_owned(),
                ));
            }
            // Trailing `/` prevents matching sibling prefixes like `restic-windows-2/...`.
            Some(format!("{clean_prefix}/"))
        }
        R2DeleteScope::EntireBucket => None,
    };

    let access_key = secrets
        .r2_access_key_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::Config("R2 のアクセスキー ID が DPAPI ストアに設定されていません".to_owned())
        })?;
    let secret_key = secrets
        .r2_secret_access_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::Config(
                "R2 のシークレットアクセスキーが DPAPI ストアに設定されていません".to_owned(),
            )
        })?;

    let endpoint_str = r2.endpoint_url()?;
    let endpoint = ParsedEndpoint::parse(&endpoint_str)?;
    let region = match r2.region.trim() {
        "" => "auto",
        other => other,
    };

    let client = R2S3Client::new(endpoint, region, access_key, secret_key)?;

    update_progress(progress_state, |p| {
        *p = R2DeleteProgress {
            running: true,
            dry_run,
            phase: "オブジェクト一覧を取得中…".to_owned(),
            bucket: bucket.clone(),
            prefix: list_prefix.clone(),
            ..Default::default()
        };
    });
    let _guard = ProgressGuard {
        state: progress_state,
    };

    let mut report = R2DeleteReport {
        scope,
        dry_run,
        bucket: bucket.clone(),
        prefix: list_prefix.clone(),
        objects_found: 0,
        bytes_found: 0,
        objects_deleted: 0,
        bytes_deleted: 0,
        multipart_uploads_found: 0,
        multipart_uploads_aborted: 0,
        bucket_deleted: false,
        bucket_delete_access_denied: false,
        bucket_not_found: false,
        note: None,
    };

    // 1. List (and when !dry_run, batch-delete) objects.
    let mut continuation_token: Option<String> = None;
    loop {
        check_cancelled(cancel_flag)?;
        update_progress(progress_state, |p| {
            p.phase = if dry_run {
                "削除対象のオブジェクトを集計中…".to_owned()
            } else {
                "オブジェクト一覧を取得中…".to_owned()
            };
        });

        let page = match client.list_objects_v2(
            &bucket,
            list_prefix.as_deref(),
            continuation_token.as_deref(),
        )? {
            ListOutcome::Found(page) => page,
            ListOutcome::NoSuchBucket => {
                report.bucket_not_found = true;
                report.note = Some(
                    "指定されたバケットは見つかりませんでした（すでに削除済みの可能性があります）。"
                        .to_owned(),
                );
                return Ok(report);
            }
        };

        let page_bytes: u64 = page.objects.iter().map(|o| o.size).sum();
        report.objects_found = report
            .objects_found
            .saturating_add(page.objects.len() as u64);
        report.bytes_found = report.bytes_found.saturating_add(page_bytes);
        update_progress(progress_state, |p| {
            p.objects_scanned = report.objects_found;
            p.bytes_scanned = report.bytes_found;
        });

        if !dry_run && !page.objects.is_empty() {
            for chunk in page.objects.chunks(MAX_DELETE_BATCH) {
                check_cancelled(cancel_flag)?;
                update_progress(progress_state, |p| {
                    p.phase = "オブジェクトを削除中…".to_owned();
                });

                let size_by_key: HashMap<&str, u64> =
                    chunk.iter().map(|o| (o.key.as_str(), o.size)).collect();
                let keys: Vec<&str> = chunk.iter().map(|o| o.key.as_str()).collect();
                let deleted_keys = client.delete_objects(&bucket, &keys)?;

                let (deleted_count, deleted_bytes) = if deleted_keys.is_empty() {
                    let chunk_bytes: u64 = chunk.iter().map(|o| o.size).sum();
                    (chunk.len() as u64, chunk_bytes)
                } else {
                    let bytes: u64 = deleted_keys
                        .iter()
                        .filter_map(|k| size_by_key.get(k.as_str()).copied())
                        .sum();
                    (deleted_keys.len() as u64, bytes)
                };
                report.objects_deleted = report.objects_deleted.saturating_add(deleted_count);
                report.bytes_deleted = report.bytes_deleted.saturating_add(deleted_bytes);
                update_progress(progress_state, |p| {
                    p.objects_deleted = report.objects_deleted;
                    p.bytes_deleted = report.bytes_deleted;
                });
            }
        }

        if !page.is_truncated {
            break;
        }
        let next = page.next_continuation_token.ok_or_else(|| {
            AppError::R2(
                "ListObjectsV2 returned IsTruncated=true without NextContinuationToken".to_owned(),
            )
        })?;
        if continuation_token.as_deref() == Some(next.as_str()) {
            return Err(AppError::R2(
                "ListObjectsV2 pagination stalled with repeated NextContinuationToken".to_owned(),
            ));
        }
        continuation_token = Some(next);
    }

    // 2. List (and when !dry_run, abort) incomplete multipart uploads so DeleteBucket succeeds.
    let mut key_marker: Option<String> = None;
    let mut upload_id_marker: Option<String> = None;
    loop {
        check_cancelled(cancel_flag)?;
        update_progress(progress_state, |p| {
            p.phase = if dry_run {
                "未完了のマルチパートアップロードを確認中…".to_owned()
            } else {
                "未完了のマルチパートアップロードを整理中…".to_owned()
            };
        });

        let page = match client.list_multipart_uploads(
            &bucket,
            list_prefix.as_deref(),
            key_marker.as_deref(),
            upload_id_marker.as_deref(),
        )? {
            ListOutcome::Found(page) => page,
            ListOutcome::NoSuchBucket => {
                report.bucket_not_found = true;
                report.note = Some(
                    "指定されたバケットは見つかりませんでした（すでに削除済みの可能性があります）。"
                        .to_owned(),
                );
                return Ok(report);
            }
        };

        report.multipart_uploads_found = report
            .multipart_uploads_found
            .saturating_add(page.uploads.len() as u64);

        if !dry_run {
            for upload in &page.uploads {
                check_cancelled(cancel_flag)?;
                client.abort_multipart_upload(&bucket, &upload.key, &upload.upload_id)?;
                report.multipart_uploads_aborted =
                    report.multipart_uploads_aborted.saturating_add(1);
                update_progress(progress_state, |p| {
                    p.multipart_aborted = report.multipart_uploads_aborted;
                });
            }
        }

        if !page.is_truncated {
            break;
        }
        let next_key = page.next_key_marker.unwrap_or_default();
        let next_id = page.next_upload_id_marker.unwrap_or_default();
        if next_key.is_empty() && next_id.is_empty() {
            return Err(AppError::R2(
                "ListMultipartUploads returned IsTruncated=true without markers".to_owned(),
            ));
        }
        if key_marker.as_deref() == Some(next_key.as_str())
            && upload_id_marker.as_deref() == Some(next_id.as_str())
        {
            return Err(AppError::R2(
                "ListMultipartUploads pagination stalled with repeated markers".to_owned(),
            ));
        }
        key_marker = (!next_key.is_empty()).then_some(next_key);
        upload_id_marker = (!next_id.is_empty()).then_some(next_id);
    }

    // 3. Delete the bucket container itself when `EntireBucket` and not dry-run.
    if !dry_run && scope == R2DeleteScope::EntireBucket {
        check_cancelled(cancel_flag)?;
        update_progress(progress_state, |p| {
            p.phase = "バケット本体を削除中…".to_owned();
        });

        match client.delete_bucket(&bucket)? {
            DeleteBucketOutcome::Deleted => {
                report.bucket_deleted = true;
            }
            DeleteBucketOutcome::AccessDenied => {
                report.bucket_delete_access_denied = true;
                report.note = Some(
                    "バケット内のオブジェクトはすべて削除して空にしました。バケット本体の削除はアクセスキーの権限不足（403 AccessDenied）のためスキップされました。Cloudflare ダッシュボードから空のバケットを削除するか、Admin Read & Write 権限のトークンで再実行してください。"
                        .to_owned(),
                );
            }
            DeleteBucketOutcome::NoSuchBucket => {
                report.bucket_not_found = true;
                report.note = Some("バケットはすでに存在しませんでした。".to_owned());
            }
        }
    }

    Ok(report)
}

// ---------------------------------------------------------------------------------------------
// HTTP + S3 protocol client
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedEndpoint {
    scheme: String,
    /// `host` or `host:port` as used in the HTTP URL authority.
    authority: String,
    /// Value for the SigV4 `host` header (strips default `:80` / `:443`).
    host_header: String,
    /// Optional base path prefix without trailing slash (usually empty).
    base_path: String,
}

impl ParsedEndpoint {
    fn parse(raw: &str) -> AppResult<Self> {
        let trimmed = raw.trim().trim_end_matches('/');
        let (scheme, rest) = trimmed
            .split_once("://")
            .ok_or_else(|| AppError::Config(format!("invalid R2 endpoint URL: {raw}")))?;
        let scheme_lower = scheme.to_ascii_lowercase();
        if scheme_lower != "https" && scheme_lower != "http" {
            return Err(AppError::Config(format!(
                "R2 endpoint URL must use https:// or http:// (got {raw})"
            )));
        }
        let (authority_raw, path_raw) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, ""),
        };
        if authority_raw.is_empty()
            || authority_raw
                .chars()
                .any(|c| c.is_whitespace() || matches!(c, '?' | '#' | '@'))
        {
            return Err(AppError::Config(format!(
                "invalid host in R2 endpoint URL: {raw}"
            )));
        }
        let authority = authority_raw.to_ascii_lowercase();
        let default_port = if scheme_lower == "https" {
            ":443"
        } else {
            ":80"
        };
        let host_header = authority
            .strip_suffix(default_port)
            .unwrap_or(&authority)
            .to_owned();
        let base_path = path_raw.trim_end_matches('/').to_owned();
        Ok(Self {
            scheme: scheme_lower,
            authority,
            host_header,
            base_path,
        })
    }

    fn bucket_uri(&self, bucket: &str) -> String {
        format!("{}/{}", self.base_path, aws_uri_encode(bucket))
    }

    fn object_uri(&self, bucket: &str, key: &str) -> String {
        format!(
            "{}/{}/{}",
            self.base_path,
            aws_uri_encode(bucket),
            aws_uri_encode_path(key)
        )
    }

    fn full_url(&self, canonical_uri: &str, canonical_query: &str) -> String {
        if canonical_query.is_empty() {
            format!("{}://{}{}", self.scheme, self.authority, canonical_uri)
        } else {
            format!(
                "{}://{}{}?{}",
                self.scheme, self.authority, canonical_uri, canonical_query
            )
        }
    }
}

struct R2S3Client<'a> {
    agent: ureq::Agent,
    endpoint: ParsedEndpoint,
    region: &'a str,
    access_key: &'a str,
    secret_key: &'a str,
}

enum ListOutcome<T> {
    Found(T),
    NoSuchBucket,
}

enum DeleteBucketOutcome {
    Deleted,
    AccessDenied,
    NoSuchBucket,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct S3ObjectEntry {
    key: String,
    size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ListObjectsPage {
    objects: Vec<S3ObjectEntry>,
    is_truncated: bool,
    next_continuation_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MultipartUploadEntry {
    key: String,
    upload_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ListMultipartPage {
    uploads: Vec<MultipartUploadEntry>,
    is_truncated: bool,
    next_key_marker: Option<String>,
    next_upload_id_marker: Option<String>,
}

struct HttpReply {
    status: u16,
    body: String,
}

impl<'a> R2S3Client<'a> {
    fn new(
        endpoint: ParsedEndpoint,
        region: &'a str,
        access_key: &'a str,
        secret_key: &'a str,
    ) -> AppResult<Self> {
        let tls = ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::NativeTls)
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build();
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(HTTP_TIMEOUT))
            .tls_config(tls)
            .build();
        let agent = ureq::Agent::new_with_config(config);
        Ok(Self {
            agent,
            endpoint,
            region,
            access_key,
            secret_key,
        })
    }

    fn send(
        &self,
        method: &str,
        canonical_uri: &str,
        query: &[(&str, &str)],
        body: &[u8],
    ) -> AppResult<HttpReply> {
        let signed = sign_s3_request(
            method,
            &self.endpoint.host_header,
            canonical_uri,
            query,
            body,
            self.region,
            self.access_key,
            self.secret_key,
            Utc::now(),
        );
        let url = self
            .endpoint
            .full_url(canonical_uri, &signed.canonical_query);

        let mut response = match method {
            "GET" => {
                let mut req = self.agent.get(&url);
                for (k, v) in &signed.headers {
                    req = req.header(k.as_str(), v.as_str());
                }
                req.call()
            }
            "DELETE" => {
                let mut req = self.agent.delete(&url);
                for (k, v) in &signed.headers {
                    req = req.header(k.as_str(), v.as_str());
                }
                req.call()
            }
            "POST" => {
                let mut req = self.agent.post(&url);
                for (k, v) in &signed.headers {
                    req = req.header(k.as_str(), v.as_str());
                }
                req.send(body)
            }
            other => {
                return Err(AppError::R2(format!("unsupported HTTP method: {other}")));
            }
        }
        .map_err(|e| AppError::R2(format!("HTTP {method} failed: {e}")))?;

        let status = response.status().as_u16();
        let body_text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| AppError::R2(format!("failed reading R2 response body: {e}")))?;
        Ok(HttpReply {
            status,
            body: body_text,
        })
    }

    fn list_objects_v2(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        continuation_token: Option<&str>,
    ) -> AppResult<ListOutcome<ListObjectsPage>> {
        let mut query = vec![
            ("encoding-type", "url"),
            ("list-type", "2"),
            ("max-keys", "1000"),
        ];
        if let Some(p) = prefix {
            query.push(("prefix", p));
        }
        if let Some(tok) = continuation_token {
            query.push(("continuation-token", tok));
        }
        let uri = self.endpoint.bucket_uri(bucket);
        let reply = self.send("GET", &uri, &query, &[])?;
        if reply.status == 404 && is_error_code(&reply.body, "NoSuchBucket") {
            return Ok(ListOutcome::NoSuchBucket);
        }
        if !(200..300).contains(&reply.status) {
            return Err(format_s3_http_error(
                "ListObjectsV2",
                reply.status,
                &reply.body,
            ));
        }
        Ok(ListOutcome::Found(parse_list_objects_v2(&reply.body)?))
    }

    fn delete_objects(&self, bucket: &str, keys: &[&str]) -> AppResult<Vec<String>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let xml = build_delete_objects_xml(keys);
        let uri = self.endpoint.bucket_uri(bucket);
        let reply = self.send("POST", &uri, &[("delete", "")], xml.as_bytes())?;
        if !(200..300).contains(&reply.status) {
            return Err(format_s3_http_error(
                "DeleteObjects",
                reply.status,
                &reply.body,
            ));
        }
        let (deleted, errors) = parse_delete_objects_result(&reply.body)?;
        if !errors.is_empty() {
            let summary: Vec<String> = errors
                .iter()
                .take(5)
                .map(|(k, code, msg)| {
                    if msg.is_empty() {
                        format!("{k} ({code})")
                    } else {
                        format!("{k} ({code}: {msg})")
                    }
                })
                .collect();
            let extra = if errors.len() > 5 {
                format!(" 他 {} 件", errors.len() - 5)
            } else {
                String::new()
            };
            return Err(AppError::R2(format!(
                "一部オブジェクトの削除に失敗しました: {}{extra}",
                summary.join(", ")
            )));
        }
        Ok(deleted)
    }

    fn list_multipart_uploads(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        key_marker: Option<&str>,
        upload_id_marker: Option<&str>,
    ) -> AppResult<ListOutcome<ListMultipartPage>> {
        let mut query = vec![
            ("encoding-type", "url"),
            ("max-uploads", "1000"),
            ("uploads", ""),
        ];
        if let Some(p) = prefix {
            query.push(("prefix", p));
        }
        if let Some(km) = key_marker {
            query.push(("key-marker", km));
        }
        if let Some(um) = upload_id_marker {
            query.push(("upload-id-marker", um));
        }
        let uri = self.endpoint.bucket_uri(bucket);
        let reply = self.send("GET", &uri, &query, &[])?;
        if reply.status == 404 && is_error_code(&reply.body, "NoSuchBucket") {
            return Ok(ListOutcome::NoSuchBucket);
        }
        if !(200..300).contains(&reply.status) {
            return Err(format_s3_http_error(
                "ListMultipartUploads",
                reply.status,
                &reply.body,
            ));
        }
        Ok(ListOutcome::Found(parse_list_multipart_uploads(
            &reply.body,
        )?))
    }

    fn abort_multipart_upload(&self, bucket: &str, key: &str, upload_id: &str) -> AppResult<()> {
        let uri = self.endpoint.object_uri(bucket, key);
        let reply = self.send("DELETE", &uri, &[("uploadId", upload_id)], &[])?;
        if (200..300).contains(&reply.status)
            || (reply.status == 404 && is_error_code(&reply.body, "NoSuchUpload"))
        {
            return Ok(());
        }
        Err(format_s3_http_error(
            "AbortMultipartUpload",
            reply.status,
            &reply.body,
        ))
    }

    fn delete_bucket(&self, bucket: &str) -> AppResult<DeleteBucketOutcome> {
        let uri = self.endpoint.bucket_uri(bucket);
        let reply = self.send("DELETE", &uri, &[], &[])?;
        if (200..300).contains(&reply.status) {
            return Ok(DeleteBucketOutcome::Deleted);
        }
        if reply.status == 403 {
            return Ok(DeleteBucketOutcome::AccessDenied);
        }
        if reply.status == 404 && is_error_code(&reply.body, "NoSuchBucket") {
            return Ok(DeleteBucketOutcome::NoSuchBucket);
        }
        Err(format_s3_http_error(
            "DeleteBucket",
            reply.status,
            &reply.body,
        ))
    }
}

// ---------------------------------------------------------------------------------------------
// AWS Signature Version 4 (S3)
// ---------------------------------------------------------------------------------------------

struct SignedRequest {
    canonical_query: String,
    /// Headers to attach to the HTTP request (excluding `Host`, which the HTTP client sets).
    headers: Vec<(String, String)>,
}

#[allow(clippy::too_many_arguments)]
fn sign_s3_request(
    method: &str,
    host_header: &str,
    canonical_uri: &str,
    query: &[(&str, &str)],
    body: &[u8],
    region: &str,
    access_key: &str,
    secret_key: &str,
    now: DateTime<Utc>,
) -> SignedRequest {
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date_stamp = now.format("%Y%m%d").to_string();

    let payload_hash = hex_lower(&sha256_bytes(body));

    let mut encoded_query: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (aws_uri_encode(k), aws_uri_encode(v)))
        .collect();
    encoded_query.sort();
    let canonical_query = encoded_query
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");

    let mut canonical_header_pairs: Vec<(&str, String)> = vec![
        ("host", host_header.to_owned()),
        ("x-amz-content-sha256", payload_hash.clone()),
        ("x-amz-date", amz_date.clone()),
    ];
    let content_md5 = (!body.is_empty()).then(|| base64_encode(&md5_bytes(body)));
    if let Some(ref md5_b64) = content_md5 {
        canonical_header_pairs.push(("content-md5", md5_b64.clone()));
        canonical_header_pairs.push(("content-type", "application/xml".to_owned()));
    }
    canonical_header_pairs.sort_by(|a, b| a.0.cmp(b.0));

    let canonical_headers: String = canonical_header_pairs
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", v.trim()))
        .collect();
    let signed_headers = canonical_header_pairs
        .iter()
        .map(|(k, _)| *k)
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );

    let scope = format!("{date_stamp}/{region}/s3/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex_lower(&sha256_bytes(canonical_request.as_bytes()))
    );

    let mut prefixed_secret = Vec::with_capacity(4 + secret_key.len());
    prefixed_secret.extend_from_slice(b"AWS4");
    prefixed_secret.extend_from_slice(secret_key.as_bytes());
    let mut k_date = hmac_sha256(&prefixed_secret, date_stamp.as_bytes());
    prefixed_secret.zeroize();
    let mut k_region = hmac_sha256(&k_date, region.as_bytes());
    k_date.zeroize();
    let mut k_service = hmac_sha256(&k_region, b"s3");
    k_region.zeroize();
    let mut k_signing = hmac_sha256(&k_service, b"aws4_request");
    k_service.zeroize();
    let signature = hmac_sha256(&k_signing, string_to_sign.as_bytes());
    k_signing.zeroize();

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={}",
        hex_lower(&signature)
    );

    let mut headers = vec![
        ("x-amz-date".to_owned(), amz_date),
        ("x-amz-content-sha256".to_owned(), payload_hash),
        ("Authorization".to_owned(), authorization),
    ];
    if let Some(md5_b64) = content_md5 {
        headers.push(("Content-Type".to_owned(), "application/xml".to_owned()));
        headers.push(("Content-MD5".to_owned(), md5_b64));
    }

    SignedRequest {
        canonical_query,
        headers,
    }
}

fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

fn md5_bytes(data: &[u8]) -> [u8; 16] {
    let mut hasher = Md5::new();
    hasher.update(data);
    hasher.finalize().into()
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Standard RFC 4648 Base64 encoder (used for `Content-MD5`).
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// AWS SigV4 RFC 3986 URI encoding (encodes every byte except `A-Za-z0-9-_.~`).
fn aws_uri_encode(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

/// AWS SigV4 URI encoding for an object key path, preserving `/` separators.
fn aws_uri_encode_path(path: &str) -> String {
    path.split('/')
        .map(aws_uri_encode)
        .collect::<Vec<_>>()
        .join("/")
}

/// Decodes S3 `encoding-type=url` values (`+` -> space, `%XX` -> raw UTF-8 bytes).
fn decode_s3_url_encoding(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| input.to_owned())
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// S3 XML builders and parsers
// ---------------------------------------------------------------------------------------------

fn escape_xml_text(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
}

fn build_delete_objects_xml(keys: &[&str]) -> String {
    let mut xml = String::with_capacity(128 + keys.len() * 64);
    xml.push_str(
        r#"<?xml version="1.0" encoding="UTF-8"?><Delete xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Quiet>false</Quiet>"#,
    );
    for key in keys {
        xml.push_str("<Object><Key>");
        escape_xml_text(&mut xml, key);
        xml.push_str("</Key></Object>");
    }
    xml.push_str("</Delete>");
    xml
}

fn append_xml_event_text(target: &mut String, ev: &Event<'_>) -> AppResult<()> {
    match ev {
        Event::Text(e) => {
            let decoded = e
                .decode()
                .map_err(|err| AppError::R2(format!("invalid XML text: {err}")))?;
            target.push_str(&decoded);
        }
        Event::CData(e) => {
            let decoded = e
                .decode()
                .map_err(|err| AppError::R2(format!("invalid XML CDATA: {err}")))?;
            target.push_str(&decoded);
        }
        Event::GeneralRef(e) => {
            if let Some(ch) = e
                .resolve_char_ref()
                .map_err(|err| AppError::R2(format!("invalid XML char ref: {err}")))?
            {
                target.push(ch);
            } else {
                let name = e
                    .decode()
                    .map_err(|err| AppError::R2(format!("invalid XML entity ref: {err}")))?;
                if let Some(resolved) = quick_xml::escape::resolve_xml_entity(&name) {
                    target.push_str(resolved);
                } else {
                    target.push('&');
                    target.push_str(&name);
                    target.push(';');
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn parse_list_objects_v2(xml: &str) -> AppResult<ListObjectsPage> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut path: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut encoding_url = false;
    let mut is_truncated = false;
    let mut next_token: Option<String> = None;
    let mut objects = Vec::new();
    let mut cur_key: Option<String> = None;
    let mut cur_size: u64 = 0;

    loop {
        match reader
            .read_event()
            .map_err(|e| AppError::R2(format!("failed to parse ListObjectsV2 XML: {e}")))?
        {
            Event::Start(e) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if tag == "Contents" {
                    cur_key = None;
                    cur_size = 0;
                }
                path.push(tag);
                text.clear();
            }
            Event::End(e) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                let parent = path
                    .len()
                    .checked_sub(2)
                    .and_then(|i| path.get(i))
                    .map(String::as_str);
                match (parent, tag.as_str()) {
                    (Some("ListBucketResult"), "EncodingType") => {
                        encoding_url = text.trim().eq_ignore_ascii_case("url");
                    }
                    (Some("ListBucketResult"), "IsTruncated") => {
                        is_truncated = text.trim().eq_ignore_ascii_case("true");
                    }
                    (Some("ListBucketResult"), "NextContinuationToken") => {
                        let tok = text.trim();
                        if !tok.is_empty() {
                            next_token = Some(tok.to_owned());
                        }
                    }
                    (Some("Contents"), "Key") => {
                        cur_key = Some(text.clone());
                    }
                    (Some("Contents"), "Size") => {
                        cur_size = text.trim().parse::<u64>().unwrap_or(0);
                    }
                    (_, "Contents") => {
                        if let Some(key) = cur_key.take() {
                            objects.push(S3ObjectEntry {
                                key,
                                size: cur_size,
                            });
                        }
                    }
                    _ => {}
                }
                path.pop();
                text.clear();
            }
            Event::Eof => break,
            ev => append_xml_event_text(&mut text, &ev)?,
        }
    }

    if encoding_url {
        for obj in &mut objects {
            obj.key = decode_s3_url_encoding(&obj.key);
        }
    }

    Ok(ListObjectsPage {
        objects,
        is_truncated,
        next_continuation_token: next_token,
    })
}

#[allow(clippy::type_complexity)]
fn parse_delete_objects_result(
    xml: &str,
) -> AppResult<(Vec<String>, Vec<(String, String, String)>)> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut path: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut deleted = Vec::new();
    let mut errors = Vec::new();

    let mut err_key = String::new();
    let mut err_code = String::new();
    let mut err_msg = String::new();

    loop {
        match reader
            .read_event()
            .map_err(|e| AppError::R2(format!("failed to parse DeleteObjects XML: {e}")))?
        {
            Event::Start(e) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if tag == "Error" {
                    err_key.clear();
                    err_code.clear();
                    err_msg.clear();
                }
                path.push(tag);
                text.clear();
            }
            Event::End(e) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                let parent = path
                    .len()
                    .checked_sub(2)
                    .and_then(|i| path.get(i))
                    .map(String::as_str);
                match (parent, tag.as_str()) {
                    (Some("Deleted"), "Key") => deleted.push(text.clone()),
                    (Some("Error"), "Key") => err_key = text.clone(),
                    (Some("Error"), "Code") => err_code = text.trim().to_owned(),
                    (Some("Error"), "Message") => err_msg = text.trim().to_owned(),
                    (Some("DeleteResult"), "Error") => {
                        errors.push((
                            std::mem::take(&mut err_key),
                            std::mem::take(&mut err_code),
                            std::mem::take(&mut err_msg),
                        ));
                    }
                    _ => {}
                }
                path.pop();
                text.clear();
            }
            Event::Eof => break,
            ev => append_xml_event_text(&mut text, &ev)?,
        }
    }

    Ok((deleted, errors))
}

fn parse_list_multipart_uploads(xml: &str) -> AppResult<ListMultipartPage> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut path: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut encoding_url = false;
    let mut is_truncated = false;
    let mut next_key_marker: Option<String> = None;
    let mut next_upload_id_marker: Option<String> = None;
    let mut uploads = Vec::new();
    let mut cur_key: Option<String> = None;
    let mut cur_id: Option<String> = None;

    loop {
        match reader
            .read_event()
            .map_err(|e| AppError::R2(format!("failed to parse ListMultipartUploads XML: {e}")))?
        {
            Event::Start(e) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if tag == "Upload" {
                    cur_key = None;
                    cur_id = None;
                }
                path.push(tag);
                text.clear();
            }
            Event::End(e) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                let parent = path
                    .len()
                    .checked_sub(2)
                    .and_then(|i| path.get(i))
                    .map(String::as_str);
                match (parent, tag.as_str()) {
                    (Some("ListMultipartUploadsResult"), "EncodingType") => {
                        encoding_url = text.trim().eq_ignore_ascii_case("url");
                    }
                    (Some("ListMultipartUploadsResult"), "IsTruncated") => {
                        is_truncated = text.trim().eq_ignore_ascii_case("true");
                    }
                    (Some("ListMultipartUploadsResult"), "NextKeyMarker") => {
                        if !text.is_empty() {
                            next_key_marker = Some(text.clone());
                        }
                    }
                    (Some("ListMultipartUploadsResult"), "NextUploadIdMarker") => {
                        let id = text.trim();
                        if !id.is_empty() {
                            next_upload_id_marker = Some(id.to_owned());
                        }
                    }
                    (Some("Upload"), "Key") => cur_key = Some(text.clone()),
                    (Some("Upload"), "UploadId") => cur_id = Some(text.trim().to_owned()),
                    (_, "Upload") => {
                        if let (Some(key), Some(upload_id)) = (cur_key.take(), cur_id.take()) {
                            uploads.push(MultipartUploadEntry { key, upload_id });
                        }
                    }
                    _ => {}
                }
                path.pop();
                text.clear();
            }
            Event::Eof => break,
            ev => append_xml_event_text(&mut text, &ev)?,
        }
    }

    if encoding_url {
        for u in &mut uploads {
            u.key = decode_s3_url_encoding(&u.key);
        }
        if let Some(ref mut km) = next_key_marker {
            *km = decode_s3_url_encoding(km);
        }
    }

    Ok(ListMultipartPage {
        uploads,
        is_truncated,
        next_key_marker,
        next_upload_id_marker,
    })
}

fn parse_s3_error(xml: &str) -> Option<(String, String)> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut path: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut code = String::new();
    let mut message = String::new();

    loop {
        match reader.read_event().ok()? {
            Event::Start(e) => {
                path.push(String::from_utf8_lossy(e.local_name().as_ref()).into_owned());
                text.clear();
            }
            Event::End(e) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                let parent = path
                    .len()
                    .checked_sub(2)
                    .and_then(|i| path.get(i))
                    .map(String::as_str);
                if parent == Some("Error") {
                    match tag.as_str() {
                        "Code" => code = text.trim().to_owned(),
                        "Message" => message = text.trim().to_owned(),
                        _ => {}
                    }
                }
                path.pop();
                text.clear();
            }
            Event::Eof => break,
            ev => {
                let _ = append_xml_event_text(&mut text, &ev);
            }
        }
    }

    if code.is_empty() && message.is_empty() {
        None
    } else {
        Some((code, message))
    }
}

fn is_error_code(body: &str, expected: &str) -> bool {
    parse_s3_error(body).is_some_and(|(code, _)| code.eq_ignore_ascii_case(expected))
}

fn format_s3_http_error(op: &str, status: u16, body: &str) -> AppError {
    if let Some((code, msg)) = parse_s3_error(body) {
        if msg.is_empty() {
            AppError::R2(format!("{op} failed (HTTP {status} {code})"))
        } else {
            AppError::R2(format!("{op} failed (HTTP {status} {code}): {msg}"))
        }
    } else {
        let snippet = body.trim();
        if snippet.is_empty() {
            AppError::R2(format!("{op} failed (HTTP {status})"))
        } else {
            let short: String = snippet.chars().take(240).collect();
            AppError::R2(format!("{op} failed (HTTP {status}): {short}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn sigv4_matches_aws_s3_list_objects_test_vector() {
        // Official AWS SigV4 S3 Example: GET Bucket (List Objects)
        // https://docs.aws.amazon.com/AmazonS3/latest/API/sig-v4-header-based-auth.html
        let now = Utc.with_ymd_and_hms(2013, 5, 24, 0, 0, 0).unwrap();
        let signed = sign_s3_request(
            "GET",
            "examplebucket.s3.amazonaws.com",
            "/",
            &[("max-keys", "2"), ("prefix", "J")],
            &[],
            "us-east-1",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            now,
        );
        assert_eq!(signed.canonical_query, "max-keys=2&prefix=J");
        let auth = signed
            .headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        );
    }

    #[test]
    fn base64_uri_encode_and_xml_round_trip() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");

        assert_eq!(aws_uri_encode("a/b c+d~e"), "a%2Fb%20c%2Bd~e");
        assert_eq!(
            aws_uri_encode_path("dir/my file+1.txt"),
            "dir/my%20file%2B1.txt"
        );
        assert_eq!(
            decode_s3_url_encoding("dir/my+file%2B1+%E6%97%A5%E6%9C%AC%E8%AA%9E.txt"),
            "dir/my file+1 日本語.txt"
        );

        let xml = build_delete_objects_xml(&["a/b&c<d>\"'"]);
        assert!(xml.contains("<Key>a/b&amp;c&lt;d&gt;&quot;&apos;</Key>"));
    }

    #[test]
    fn parse_list_objects_v2_decodes_entities_and_url_encoding() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
            <EncodingType>url</EncodingType>
            <IsTruncated>true</IsTruncated>
            <NextContinuationToken>tok+123==</NextContinuationToken>
            <Contents>
                <Key>prefix/a+%26+b%2B1/%E3%83%86%E3%82%B9%E3%83%88.bin</Key>
                <Size>4096</Size>
            </Contents>
        </ListBucketResult>"#;
        let page = parse_list_objects_v2(xml).unwrap();
        assert!(page.is_truncated);
        assert_eq!(page.next_continuation_token.as_deref(), Some("tok+123=="));
        assert_eq!(page.objects.len(), 1);
        assert_eq!(page.objects[0].key, "prefix/a & b+1/テスト.bin");
        assert_eq!(page.objects[0].size, 4096);
    }

    #[test]
    fn parse_delete_objects_result_extracts_deleted_and_errors() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <DeleteResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
            <Deleted><Key>ok/1</Key></Deleted>
            <Error>
                <Key>locked/file&amp;1</Key>
                <Code>ObjectLockedByBucketPolicy</Code>
                <Message>Object is locked</Message>
            </Error>
        </DeleteResult>"#;
        let (deleted, errors) = parse_delete_objects_result(xml).unwrap();
        assert_eq!(deleted, vec!["ok/1"]);
        assert_eq!(
            errors,
            vec![(
                "locked/file&1".to_owned(),
                "ObjectLockedByBucketPolicy".to_owned(),
                "Object is locked".to_owned()
            )]
        );
    }

    // -----------------------------------------------------------------------------------------
    // Local HTTP mock S3 server for end-to-end tests
    // -----------------------------------------------------------------------------------------

    #[derive(Clone, Default)]
    struct MockS3State {
        bucket_exists: bool,
        delete_bucket_403: bool,
        locked_keys: Vec<String>,
        objects: BTreeMap<String, u64>,
        uploads: Vec<(String, String)>,
        page_size: usize,
        delete_bucket_called: bool,
        seen_hosts: Vec<String>,
    }

    fn spawn_mock_s3(state: Arc<Mutex<MockS3State>>) -> (String, Arc<AtomicBool>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock s3");
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);

        thread::spawn(move || {
            while !stop_bg.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                        handle_mock_connection(&mut stream, &state);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        (format!("http://127.0.0.1:{}", addr.port()), stop)
    }

    fn handle_mock_connection(stream: &mut std::net::TcpStream, state: &Arc<Mutex<MockS3State>>) {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let header_end = loop {
            match stream.read(&mut tmp) {
                Ok(0) => return,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                }
                Err(_) => return,
            }
        };
        let header_str = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let mut lines = header_str.lines();
        let req_line = lines.next().unwrap_or_default();
        let mut parts = req_line.split_whitespace();
        let method = parts.next().unwrap_or_default();
        let target = parts.next().unwrap_or_default();

        let mut content_length = 0usize;
        let mut host_val = String::new();
        let mut has_auth = false;
        let mut has_md5 = false;
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                let kl = k.trim().to_ascii_lowercase();
                let vt = v.trim();
                if kl == "content-length" {
                    content_length = vt.parse().unwrap_or(0);
                } else if kl == "host" {
                    host_val = vt.to_owned();
                } else if kl == "authorization" && vt.starts_with("AWS4-HMAC-SHA256 ") {
                    has_auth = true;
                } else if kl == "content-md5" && !vt.is_empty() {
                    has_md5 = true;
                }
            }
        }
        while buf.len() < header_end + content_length {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(_) => break,
            }
        }
        let body =
            String::from_utf8_lossy(&buf[header_end..header_end + content_length]).into_owned();

        let (path, query_str) = target.split_once('?').unwrap_or((target, ""));
        let query: HashMap<String, String> = query_str
            .split('&')
            .filter(|s| !s.is_empty())
            .map(|pair| {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                (decode_s3_url_encoding(k), decode_s3_url_encoding(v))
            })
            .collect();

        let mut st = state.lock().unwrap();
        st.seen_hosts.push(host_val);

        let (status, resp_body) = if !has_auth {
            (
                403,
                "<Error><Code>AccessDenied</Code><Message>Missing SigV4</Message></Error>"
                    .to_owned(),
            )
        } else if !st.bucket_exists {
            (
                404,
                "<Error><Code>NoSuchBucket</Code><Message>The specified bucket does not exist</Message></Error>"
                    .to_owned(),
            )
        } else if method == "GET" && query.contains_key("list-type") {
            let prefix = query.get("prefix").map(String::as_str).unwrap_or("");
            let start_after = query.get("continuation-token").map(String::as_str);
            let matching: Vec<(&String, &u64)> = st
                .objects
                .iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .filter(|(k, _)| start_after.is_none_or(|sa| k.as_str() > sa))
                .collect();
            let page_size = st.page_size.max(1);
            let truncated = matching.len() > page_size;
            let page = &matching[..matching.len().min(page_size)];
            let next_tok = truncated.then(|| page.last().unwrap().0.clone());

            let mut xml = String::from(
                r#"<?xml version="1.0" encoding="UTF-8"?><ListBucketResult><EncodingType>url</EncodingType>"#,
            );
            xml.push_str(&format!("<IsTruncated>{truncated}</IsTruncated>"));
            if let Some(tok) = next_tok {
                xml.push_str("<NextContinuationToken>");
                escape_xml_text(&mut xml, &tok);
                xml.push_str("</NextContinuationToken>");
            }
            for (k, sz) in page {
                xml.push_str("<Contents><Key>");
                escape_xml_text(&mut xml, &aws_uri_encode_path(k));
                xml.push_str(&format!("</Key><Size>{sz}</Size></Contents>"));
            }
            xml.push_str("</ListBucketResult>");
            (200, xml)
        } else if method == "POST" && query.contains_key("delete") {
            if !has_md5 {
                (
                    400,
                    "<Error><Code>InvalidRequest</Code><Message>Missing Content-MD5</Message></Error>"
                        .to_owned(),
                )
            } else {
                // Parse <Key> elements from request body
                let mut reader = Reader::from_str(&body);
                let mut in_key = false;
                let mut cur = String::new();
                let mut keys = Vec::new();
                loop {
                    match reader.read_event() {
                        Ok(Event::Start(e)) if e.local_name().as_ref() == b"Key" => {
                            in_key = true;
                            cur.clear();
                        }
                        Ok(Event::End(e)) if e.local_name().as_ref() == b"Key" => {
                            in_key = false;
                            keys.push(cur.clone());
                        }
                        Ok(Event::Eof) | Err(_) => break,
                        Ok(ev) if in_key => {
                            let _ = append_xml_event_text(&mut cur, &ev);
                        }
                        _ => {}
                    }
                }
                let mut xml =
                    String::from(r#"<?xml version="1.0" encoding="UTF-8"?><DeleteResult>"#);
                for k in keys {
                    if st.locked_keys.contains(&k) {
                        xml.push_str("<Error><Key>");
                        escape_xml_text(&mut xml, &k);
                        xml.push_str(
                            "</Key><Code>ObjectLockedByBucketPolicy</Code><Message>Locked</Message></Error>",
                        );
                    } else {
                        st.objects.remove(&k);
                        xml.push_str("<Deleted><Key>");
                        escape_xml_text(&mut xml, &k);
                        xml.push_str("</Key></Deleted>");
                    }
                }
                xml.push_str("</DeleteResult>");
                (200, xml)
            }
        } else if method == "GET" && query.contains_key("uploads") {
            let prefix = query.get("prefix").map(String::as_str).unwrap_or("");
            let matching: Vec<(String, String)> = st
                .uploads
                .iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .cloned()
                .collect();
            let mut xml = String::from(
                r#"<?xml version="1.0" encoding="UTF-8"?><ListMultipartUploadsResult><EncodingType>url</EncodingType><IsTruncated>false</IsTruncated>"#,
            );
            for (k, uid) in matching {
                xml.push_str("<Upload><Key>");
                escape_xml_text(&mut xml, &aws_uri_encode_path(&k));
                xml.push_str("</Key><UploadId>");
                escape_xml_text(&mut xml, &uid);
                xml.push_str("</UploadId></Upload>");
            }
            xml.push_str("</ListMultipartUploadsResult>");
            (200, xml)
        } else if method == "DELETE" && query.contains_key("uploadId") {
            let uid = query.get("uploadId").cloned().unwrap_or_default();
            let key_part = path
                .trim_start_matches('/')
                .split_once('/')
                .map_or("", |(_, k)| k);
            let decoded_key = decode_s3_url_encoding(key_part);
            st.uploads
                .retain(|(k, id)| !(k == &decoded_key && id == &uid));
            (204, String::new())
        } else if method == "DELETE" && path.trim_matches('/').split('/').count() == 1 {
            st.delete_bucket_called = true;
            if st.delete_bucket_403 {
                (
                    403,
                    "<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>"
                        .to_owned(),
                )
            } else if !st.objects.is_empty() || !st.uploads.is_empty() {
                (
                    409,
                    "<Error><Code>BucketNotEmpty</Code><Message>The bucket you tried to delete is not empty</Message></Error>"
                        .to_owned(),
                )
            } else {
                st.bucket_exists = false;
                (204, String::new())
            }
        } else {
            (
                400,
                format!(
                    "<Error><Code>BadRequest</Code><Message>{method} {target}</Message></Error>"
                ),
            )
        };
        drop(st);

        let reason = match status {
            200 => "OK",
            204 => "No Content",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            409 => "Conflict",
            _ => "Error",
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            resp_body.len(),
            resp_body
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }

    fn test_secrets() -> StoredSecrets {
        StoredSecrets {
            r2_access_key_id: Some("test-ak".to_owned()),
            r2_secret_access_key: Some("test-sk".to_owned()),
            restic_password: None, // Not needed for S3 API deletion!
        }
    }

    #[test]
    fn prefix_only_deletes_only_matching_prefix_and_keeps_siblings() {
        let state = Arc::new(Mutex::new(MockS3State {
            bucket_exists: true,
            page_size: 2,
            objects: BTreeMap::from([
                ("restic-windows/config".to_owned(), 100),
                ("restic-windows/data/aa/bb".to_owned(), 200),
                ("restic-windows/keys/k1".to_owned(), 50),
                ("restic-windows-2/config".to_owned(), 999),
                ("other/file.txt".to_owned(), 500),
            ]),
            uploads: vec![
                ("restic-windows/data/partial".to_owned(), "u1".to_owned()),
                ("restic-windows-2/data/partial".to_owned(), "u2".to_owned()),
            ],
            ..Default::default()
        }));
        let (endpoint, stop) = spawn_mock_s3(Arc::clone(&state));
        let r2 = R2Config {
            account_id: "acc".to_owned(),
            bucket: "my-backups".to_owned(),
            prefix: "restic-windows".to_owned(),
            endpoint_override: Some(endpoint),
            ..Default::default()
        };
        let progress = Arc::new(Mutex::new(R2DeleteProgress::default()));
        let cancel = Arc::new(AtomicBool::new(false));

        // Dry run first: counts only `restic-windows/` items, deletes nothing.
        let preview = delete_r2_data(
            &r2,
            &test_secrets(),
            R2DeleteScope::PrefixOnly,
            true,
            &progress,
            &cancel,
        )
        .unwrap();
        assert!(preview.dry_run);
        assert_eq!(preview.objects_found, 3);
        assert_eq!(preview.bytes_found, 350);
        assert_eq!(preview.objects_deleted, 0);
        assert_eq!(preview.multipart_uploads_found, 1);
        assert_eq!(preview.multipart_uploads_aborted, 0);
        assert_eq!(state.lock().unwrap().objects.len(), 5);

        // Real delete: removes only `restic-windows/`, keeps `restic-windows-2/` & `other/`, never calls DeleteBucket.
        let report = delete_r2_data(
            &r2,
            &test_secrets(),
            R2DeleteScope::PrefixOnly,
            false,
            &progress,
            &cancel,
        )
        .unwrap();
        assert!(!report.dry_run);
        assert_eq!(report.objects_deleted, 3);
        assert_eq!(report.bytes_deleted, 350);
        assert_eq!(report.multipart_uploads_aborted, 1);
        assert!(!report.bucket_deleted);
        assert!(!progress.lock().unwrap().running);

        let st = state.lock().unwrap();
        assert!(!st.delete_bucket_called);
        assert!(st.bucket_exists);
        let remaining: Vec<&String> = st.objects.keys().collect();
        assert_eq!(remaining, vec!["other/file.txt", "restic-windows-2/config"]);
        assert_eq!(
            st.uploads,
            vec![("restic-windows-2/data/partial".to_owned(), "u2".to_owned())]
        );

        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn prefix_only_rejects_empty_prefix() {
        let r2 = R2Config {
            account_id: "acc".to_owned(),
            bucket: "my-backups".to_owned(),
            prefix: "   ".to_owned(),
            ..Default::default()
        };
        let progress = Arc::new(Mutex::new(R2DeleteProgress::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let err = delete_r2_data(
            &r2,
            &test_secrets(),
            R2DeleteScope::PrefixOnly,
            true,
            &progress,
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(err, AppError::InvalidArgument(_)));
    }

    #[test]
    fn entire_bucket_empties_paginated_objects_aborts_uploads_and_deletes_bucket() {
        let state = Arc::new(Mutex::new(MockS3State {
            bucket_exists: true,
            page_size: 2,
            objects: BTreeMap::from([
                ("a/1".to_owned(), 10),
                ("b/2 & 3".to_owned(), 20),
                ("c/日本語.dat".to_owned(), 30),
            ]),
            uploads: vec![("in/progress file".to_owned(), "upload-99".to_owned())],
            ..Default::default()
        }));
        let (endpoint, stop) = spawn_mock_s3(Arc::clone(&state));
        let r2 = R2Config {
            account_id: "acc".to_owned(),
            bucket: "my-backups".to_owned(),
            prefix: "a".to_owned(),
            endpoint_override: Some(endpoint),
            ..Default::default()
        };
        let progress = Arc::new(Mutex::new(R2DeleteProgress::default()));
        let cancel = Arc::new(AtomicBool::new(false));

        let report = delete_r2_data(
            &r2,
            &test_secrets(),
            R2DeleteScope::EntireBucket,
            false,
            &progress,
            &cancel,
        )
        .unwrap();
        assert_eq!(report.objects_deleted, 3);
        assert_eq!(report.bytes_deleted, 60);
        assert_eq!(report.multipart_uploads_aborted, 1);
        assert!(report.bucket_deleted);
        assert!(!report.bucket_delete_access_denied);
        assert!(!state.lock().unwrap().bucket_exists);

        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn entire_bucket_handles_403_access_denied_after_emptying() {
        let state = Arc::new(Mutex::new(MockS3State {
            bucket_exists: true,
            delete_bucket_403: true,
            page_size: 10,
            objects: BTreeMap::from([("config".to_owned(), 123)]),
            ..Default::default()
        }));
        let (endpoint, stop) = spawn_mock_s3(Arc::clone(&state));
        let r2 = R2Config {
            account_id: "acc".to_owned(),
            bucket: "my-backups".to_owned(),
            endpoint_override: Some(endpoint),
            ..Default::default()
        };
        let progress = Arc::new(Mutex::new(R2DeleteProgress::default()));
        let cancel = Arc::new(AtomicBool::new(false));

        let report = delete_r2_data(
            &r2,
            &test_secrets(),
            R2DeleteScope::EntireBucket,
            false,
            &progress,
            &cancel,
        )
        .unwrap();
        assert_eq!(report.objects_deleted, 1);
        assert!(!report.bucket_deleted);
        assert!(report.bucket_delete_access_denied);
        assert!(report.note.as_deref().is_some_and(|n| n.contains("403")));
        assert!(state.lock().unwrap().objects.is_empty());

        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn delete_objects_partial_lock_error_fails_loudly() {
        let state = Arc::new(Mutex::new(MockS3State {
            bucket_exists: true,
            page_size: 10,
            objects: BTreeMap::from([("locked/obj".to_owned(), 50)]),
            locked_keys: vec!["locked/obj".to_owned()],
            ..Default::default()
        }));
        let (endpoint, stop) = spawn_mock_s3(Arc::clone(&state));
        let r2 = R2Config {
            account_id: "acc".to_owned(),
            bucket: "my-backups".to_owned(),
            endpoint_override: Some(endpoint),
            ..Default::default()
        };
        let progress = Arc::new(Mutex::new(R2DeleteProgress::default()));
        let cancel = Arc::new(AtomicBool::new(false));

        let err = delete_r2_data(
            &r2,
            &test_secrets(),
            R2DeleteScope::EntireBucket,
            false,
            &progress,
            &cancel,
        )
        .unwrap_err();
        assert!(err.to_string().contains("ObjectLockedByBucketPolicy"));
        assert!(!progress.lock().unwrap().running);

        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn missing_bucket_returns_bucket_not_found() {
        let state = Arc::new(Mutex::new(MockS3State {
            bucket_exists: false,
            ..Default::default()
        }));
        let (endpoint, stop) = spawn_mock_s3(Arc::clone(&state));
        let r2 = R2Config {
            account_id: "acc".to_owned(),
            bucket: "gone-bucket".to_owned(),
            endpoint_override: Some(endpoint),
            ..Default::default()
        };
        let progress = Arc::new(Mutex::new(R2DeleteProgress::default()));
        let cancel = Arc::new(AtomicBool::new(false));

        let report = delete_r2_data(
            &r2,
            &test_secrets(),
            R2DeleteScope::EntireBucket,
            false,
            &progress,
            &cancel,
        )
        .unwrap();
        assert!(report.bucket_not_found);
        assert!(!report.bucket_deleted);

        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn cancel_flag_aborts_before_network_calls() {
        let state = Arc::new(Mutex::new(MockS3State {
            bucket_exists: true,
            objects: BTreeMap::from([("a".to_owned(), 1)]),
            ..Default::default()
        }));
        let (endpoint, stop) = spawn_mock_s3(Arc::clone(&state));
        let r2 = R2Config {
            account_id: "acc".to_owned(),
            bucket: "my-backups".to_owned(),
            endpoint_override: Some(endpoint),
            ..Default::default()
        };
        let progress = Arc::new(Mutex::new(R2DeleteProgress::default()));
        let cancel = Arc::new(AtomicBool::new(true));

        let err = delete_r2_data(
            &r2,
            &test_secrets(),
            R2DeleteScope::EntireBucket,
            false,
            &progress,
            &cancel,
        )
        .unwrap_err();
        assert!(err.to_string().contains("中止"));
        assert_eq!(state.lock().unwrap().objects.len(), 1);
        assert!(!progress.lock().unwrap().running);

        stop.store(true, Ordering::Relaxed);
    }
}
