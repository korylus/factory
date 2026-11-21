//! ディレクターが呼ぶAPI (`/api/v1/director/...`) と、APIのエラーの応答 (RFC 9457のProblem Details)。

use std::collections::HashSet;

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use jiff::Timestamp;
use jiff::civil::DateTime;
use korylusfactory_protocol::{AgentEntryResult, Settings, StatusReport};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use serde::Serialize;
use serde::de::IgnoredAny;

use crate::app::AppState;
use crate::auth::Principal;
use crate::{agents, db, settings};

/// APIのパスの接頭辞。この下の応答は、エラーもProblem Detailsで返す。
pub const PREFIX: &str = "/api/";

/// 状態の報告を受け取るパス。
pub const STATUS_PATH: &str = "/api/v1/director/status";

/// ディレクターが稼働と担当ごとのエージェントの設定を読むパス。
pub const SETTINGS_PATH: &str = "/api/v1/director/settings";

/// APIの応答に付けるヘッダー。
pub const HEADERS: [(HeaderName, &str); 3] = [
    (header::CACHE_CONTROL, "no-store"),
    (HeaderName::from_static("x-robots-tag"), "noindex"),
    (
        header::CONTENT_SECURITY_POLICY,
        "default-src 'none'; frame-ancestors 'none'",
    ),
];

const PROBLEM_CONTENT_TYPE: &str = "application/problem+json";

/// 報告を保存するSQL。プロジェクトの行が無ければ作り、あれば丸ごと置き換える。
const UPSERT_STATUS: &str =
    "INSERT INTO project_statuses (project, report, received_at) VALUES (?1, ?2, ?3)
ON CONFLICT (project) DO UPDATE SET report = excluded.report, received_at = excluded.received_at";

/// パスがAPIのものかを返す。
pub fn is_api(path: &str) -> bool {
    path.starts_with(PREFIX)
}

/// API用の保護ヘッダーを付け、ルーターや抽出器が作ったエラーをProblem Detailsにする。
///
/// 認証の拒否、パニックを受け止めた応答、ルートが無いときの応答にも適用する。
pub async fn complete_response(request: Request, next: Next) -> Response {
    let api = is_api(request.uri().path());
    let mut response = next.run(request).await;
    if !api {
        return response;
    }

    if response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_none_or(|value| value != PROBLEM_CONTENT_TYPE)
        && let Some(problem) = Problem::from_rejection(response.status())
    {
        // Allowなど、元の応答で決まったヘッダーを保ったまま本文を置き換える。
        *response.body_mut() = problem.into_response().into_body();
        response.headers_mut().remove(header::CONTENT_LENGTH);
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(PROBLEM_CONTENT_TYPE),
        );
    }
    for (name, value) in HEADERS {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    response
}

/// APIのエラーの応答 (RFC 9457のProblem Details)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    /// 問題の種類を表すURI。クライアントはこれで分岐する。
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// 問題の種類ごとに決まった短い要約。
    pub title: &'static str,
    /// 応答のステータスコード。
    pub status: u16,
    /// この発生に固有の説明。
    pub detail: String,
    /// 値の誤っている箇所 (報告の値の検証に失敗したときだけ)。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<FieldError>,
    /// JSONとして読めなかった位置 (読めなかったときだけ)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<Position>,
}

/// 報告の値の誤り1つ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldError {
    /// 誤っている値の場所 (JSON Pointer。例: `/progress/0/name`)。
    pub pointer: String,
    /// 何が誤っているか。
    pub detail: String,
}

/// 本文の中の位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Position {
    /// 行 (1始まり)。
    pub line: usize,
    /// 列 (1始まり)。
    pub column: usize,
}

impl Problem {
    fn new(status: StatusCode, kind: &'static str, title: &'static str, detail: &str) -> Self {
        Self {
            kind,
            title,
            status: status.as_u16(),
            detail: detail.to_owned(),
            errors: Vec::new(),
            position: None,
        }
    }

    fn from_rejection(status: StatusCode) -> Option<Self> {
        Some(match status {
            StatusCode::BAD_REQUEST => Self::new(
                status,
                "/api/problems/invalid-request",
                "要求を読み取れません",
                "要求の本文を確認して送り直してください。",
            ),
            StatusCode::METHOD_NOT_ALLOWED => Self::new(
                status,
                "/api/problems/method-not-allowed",
                "HTTPメソッドが許可されていません",
                "Allowヘッダーで案内しているメソッドで送り直してください。",
            ),
            StatusCode::PAYLOAD_TOO_LARGE => Self::new(
                status,
                "/api/problems/payload-too-large",
                "要求の本文が大きすぎます",
                "本文を小さくして送り直してください。",
            ),
            StatusCode::INTERNAL_SERVER_ERROR => Self::internal_error(),
            _ => return None,
        })
    }

    fn invalid_json() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "/api/problems/invalid-json",
            "JSONとして読めません",
            "本文をUTF-8のJSONとして読めません。",
        )
    }

    /// 主を示すヘッダーが無い要求に返す問題。
    pub fn unauthenticated() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "/api/problems/unauthenticated",
            "ログインしていません",
            "exe.devの認証を通して呼んでください。",
        )
    }

    /// 主は分かったが、呼べない要求に返す問題。
    pub fn forbidden() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "/api/problems/forbidden",
            "許可されていません",
            "この操作は許可されていません。",
        )
    }

    /// 存在しないAPIのパスに返す問題。
    pub fn not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "/api/problems/not-found",
            "見つかりません",
            "このパスのAPIはありません。",
        )
    }

    /// ウェブアプリの中で失敗したときに返す問題。内部のエラーの中身は出さない。
    pub fn internal_error() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "/api/problems/internal-error",
            "ウェブアプリの中で失敗しました",
            "時間をおいて送り直してください。",
        )
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        // 文字列と数だけのため、JSONにできないことは無い。
        let body = serde_json::to_vec(&self).unwrap_or_default();
        (
            status,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static(PROBLEM_CONTENT_TYPE),
            )],
            body,
        )
            .into_response()
    }
}

/// `PUT /api/v1/director/status`: ディレクターの状態の報告を受け取り、そのプロジェクトの行を置き換える。
///
/// プロジェクトは要求の本文ではなく、設定でディレクターのVM名に対応付けたものを使う。
pub async fn put_status(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Principal::Director { project, .. } = principal else {
        return Problem::forbidden().into_response();
    };
    if !is_json(&headers) {
        return Problem::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "/api/problems/unsupported-media-type",
            "JSONではありません",
            "本文はJSONにし、Content-Type: application/json を付けて送ってください。",
        )
        .into_response();
    }
    let received_at = Timestamp::now();
    let (text, report) = match parse(&body) {
        Ok(parsed) => parsed,
        Err(problem) => return problem.into_response(),
    };
    let errors = validate(&report);
    if !errors.is_empty() {
        let mut problem = Problem::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "/api/problems/invalid-report",
            "報告の値が誤っています",
            "報告の値に誤りがあります。errors の箇所を直して送り直してください。",
        );
        problem.errors = errors;
        return problem.into_response();
    }
    // 送られた本文をそのまま保存し、この版のウェブアプリが知らない項目も残す。
    let text = text.to_owned();
    let saving = project.clone();
    let saved = state
        .database
        .call(move |connection| save_status(connection, &saving, &text, received_at))
        .await;
    match saved {
        Ok(()) => {
            state.events.publish(&project);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => {
            log_save_failure(&error);
            Problem::internal_error().into_response()
        }
    }
}

/// `GET /api/v1/director/settings`: ディレクターのプロジェクトの設定を返す。
///
/// 稼働の設定を保存していなければ稼働として返す。
/// 担当ごとのエージェントは保存した既知の枠だけを載せ、載せない枠はディレクターが既定として扱う。
/// プロジェクトは、状態の報告と同じく設定でディレクターのVM名に対応付けたものを使う。
pub async fn get_settings(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Response {
    let Principal::Director { project, .. } = principal else {
        return Problem::forbidden().into_response();
    };
    let loaded = async {
        let operation = settings::load(&state.database, &project).await?;
        let agents = agents::load(&state.database, &project).await?;
        Ok::<_, db::Error>((operation, agents))
    };
    match loaded.await {
        Ok((operation, agents)) => {
            let body = Settings {
                paused: operation.is_paused(),
                agents: agents
                    .into_iter()
                    .map(|(slot, entry)| {
                        (slot.value().to_owned(), AgentEntryResult::Decoded(entry))
                    })
                    .collect(),
            };
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(error) => {
            // 原因のSQLiteのエラー文には保存した値が入ることがあるため、原因をたどらずに記録する。
            log::error!("ディレクターに返す設定を読めません: {error}");
            Problem::internal_error().into_response()
        }
    }
}

/// 報告を保存する。同じ本文の再送と、保存済みの報告より前に受け取った報告は書き込まない。
///
/// 新旧は、このVMの時計で採った受信時刻で決める。
/// 時計が戻ると、戻った幅のあいだは新しい報告も保存済みより前に受け取ったものとみなし、書き込まずに成功を返す。
/// 管制のVMの時計はNTPで少しずつ合わせるため戻ることは稀で、止まるのも戻った幅のあいだだけのため、この前提を受け入れる。
fn save_status(
    connection: &mut Connection,
    project: &str,
    report: &str,
    received_at: Timestamp,
) -> rusqlite::Result<()> {
    // 比較と書き込みの間に別の接続から更新されないよう、読み取る前に書き込みロックを取る。
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let previous = transaction
        .query_row(
            "SELECT report = ?2, received_at FROM project_statuses WHERE project = ?1",
            params![project, report],
            |row| Ok((row.get::<_, bool>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    if let Some((same_report, previous_time)) = previous {
        let previous_time = previous_time.parse::<Timestamp>().map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        if same_report || received_at <= previous_time {
            return transaction.commit();
        }
    }
    transaction.execute(
        UPSERT_STATUS,
        params![project, report, received_at.to_string()],
    )?;
    transaction.commit()
}

fn log_save_failure(error: &db::Error) {
    // SQLiteのエラー文には保存した値やSQLが入ることがあるため、種類だけを記録する。
    match error {
        db::Error::Query(rusqlite::Error::SqliteFailure(error, _)) => {
            log::error!(
                "状態の報告を保存できません: SQLiteエラー ({})",
                error.extended_code
            );
        }
        db::Error::Query(rusqlite::Error::FromSqlConversionFailure(..)) => {
            log::error!("状態の報告を保存できません: DBの受信時刻を読み取れません");
        }
        db::Error::Interrupted => {
            log::error!("状態の報告を保存できません: DBの処理が中断されました");
        }
        _ => log::error!("状態の報告を保存できません: DBの操作に失敗しました"),
    }
}

/// Content-Typeが `application/json` (引数付きを含む) かを返す。
fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
}

/// 本文を報告として読む。JSONとして読めなければ400、形が報告と合わなければ422の問題を返す。
fn parse(body: &[u8]) -> Result<(&str, StatusReport), Problem> {
    let text = std::str::from_utf8(body).map_err(|error| {
        let prefix = &body[..error.valid_up_to()];
        let mut lines = prefix.rsplit(|byte| *byte == b'\n');
        let column = lines.next().map_or(1, |line| line.len() + 1);
        let mut problem = Problem::invalid_json();
        problem.position = Some(Position {
            line: lines.count() + 1,
            column,
        });
        problem
    })?;
    let report = serde_json::from_str(text).map_err(|error| {
        let (mut problem, error) = if !error.is_data() {
            (Problem::invalid_json(), error)
        } else if let Err(syntax) = serde_json::from_str::<IgnoredAny>(text) {
            // 型エラーで読み取りが止まっていても、本文全体の構文が誤っていれば400にする。
            (Problem::invalid_json(), syntax)
        } else {
            let problem = Problem::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "/api/problems/invalid-report",
                "報告の値が誤っています",
                "必要な項目が無いか、項目の型が報告の形と合いません。",
            );
            (problem, error)
        };
        problem.position = Some(Position {
            line: error.line(),
            column: error.column(),
        });
        problem
    })?;
    Ok((text, report))
}

/// UTCのRFC 3339の構文を確かめ、日付と時刻の範囲はJiffで検証する。
fn is_utc_timestamp(text: &str) -> bool {
    const DATE_TIME: &[u8] = b"0000-00-00T00:00:00";
    let Some(local) = text
        .strip_suffix(['Z', 'z'])
        .or_else(|| text.strip_suffix("+00:00"))
    else {
        return false;
    };
    let Some((date_time, fraction)) = local.split_at_checked(DATE_TIME.len()) else {
        return false;
    };
    let valid_date_time = date_time
        .bytes()
        .zip(DATE_TIME)
        .all(|(byte, expected)| match expected {
            b'0' => byte.is_ascii_digit(),
            b'T' => byte.eq_ignore_ascii_case(expected),
            _ => byte == *expected,
        });
    let valid_fraction = match fraction.as_bytes() {
        [] => true,
        [b'.', digits @ ..] => !digits.is_empty() && digits.iter().all(u8::is_ascii_digit),
        _ => false,
    };
    valid_date_time && valid_fraction && date_time.parse::<DateTime>().is_ok()
}

/// 形は読めた報告の値を確かめ、誤りの一覧を返す。誤りが無ければ空を返す。
fn validate(report: &StatusReport) -> Vec<FieldError> {
    let mut errors = Vec::new();
    let mut error = |pointer: String, detail: &str| {
        errors.push(FieldError {
            pointer,
            detail: detail.to_owned(),
        });
    };
    if !is_utc_timestamp(&report.sent_at) {
        error(
            "/sent_at".to_owned(),
            "UTCのRFC 3339の時刻 (例: 2026-11-19T08:00:00Z) にしてください。",
        );
    }
    let mut names = HashSet::new();
    for (index, plan) in report.progress.iter().enumerate() {
        if plan.name.trim().is_empty() {
            error(format!("/progress/{index}/name"), "計画書名が空です。");
        } else if !names.insert(plan.name.as_str()) {
            error(
                format!("/progress/{index}/name"),
                "同じ計画書名が2回以上あります。",
            );
        }
        if plan.url.trim().is_empty() {
            error(format!("/progress/{index}/url"), "URLが空です。");
        }
        for (session_index, session) in plan.sessions.iter().enumerate() {
            let pointer = format!("/progress/{index}/sessions/{session_index}");
            if !is_utc_timestamp(&session.started_at) {
                error(
                    format!("{pointer}/started_at"),
                    "UTCのRFC 3339の時刻 (例: 2026-11-19T08:00:00Z) にしてください。",
                );
            }
            if session
                .ended_at
                .as_deref()
                .is_some_and(|ended_at| !is_utc_timestamp(ended_at))
            {
                error(
                    format!("{pointer}/ended_at"),
                    "UTCのRFC 3339の時刻 (例: 2026-11-19T08:00:00Z) にしてください。",
                );
            }
        }
        for (task_index, task) in plan.tasks.iter().enumerate() {
            if task.number.trim().is_empty() {
                error(
                    format!("/progress/{index}/tasks/{task_index}/number"),
                    "タスクの番号が空です。",
                );
            }
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn report() -> serde_json::Value {
        json!({
            "sent_at": "2026-11-19T08:00:00Z",
            "korylusfactory": {"version": "0.1.0", "commit": "abc1234"},
            "running": true,
            "progress": [{
                "name": "webapp-status",
                "title": "ウェブアプリの骨格と状態の表示 作業計画書",
                "url": "https://wikino.example.com/s/example/pages/1",
                "task": "1-2",
                "tasks": [{"number": "1-2", "title": "クレートを作る", "status": "in_progress"}]
            }],
            "upcoming": []
        })
    }

    fn parsed(value: &serde_json::Value) -> StatusReport {
        serde_json::from_value(value.clone()).expect("報告を読めなかった")
    }

    #[test]
    fn 正しい報告には誤りが無い() {
        assert_eq!(
            validate(&parsed(&report())),
            [],
            "正しい報告に誤りを見つけた"
        );
    }

    #[test]
    fn 誤った値の場所をjson_pointerで返す() {
        let mut value = report();
        value["sent_at"] = json!("きのう");
        value["progress"][0]["url"] = json!(" ");
        value["progress"][0]["tasks"][0]["number"] = json!("");
        let mut second = value["progress"][0].clone();
        second["url"] = json!("https://example.com/");
        second["tasks"] = json!([]);
        value["progress"]
            .as_array_mut()
            .expect("progressが配列ではない")
            .push(second);

        value["progress"][0]["sessions"] = json!([
            {
                "step": {"kind": "report", "label": "作業報告書の作成"},
                "role": {"kind": "implementer", "label": "作業者"},
                "agent": "claude",
                "started_at": "2026-11-19T08:00:00Z"
            },
            {
                "step": {"kind": "report", "label": "作業報告書の作成"},
                "role": {"kind": "implementer", "label": "作業者"},
                "agent": "claude",
                "started_at": "2026-11-19 08:00",
                "ended_at": "あとで"
            }
        ]);
        let pointers: Vec<String> = validate(&parsed(&value))
            .into_iter()
            .map(|error| error.pointer)
            .collect();

        assert_eq!(
            pointers,
            [
                "/sent_at",
                "/progress/0/url",
                "/progress/0/sessions/1/started_at",
                "/progress/0/sessions/1/ended_at",
                "/progress/0/tasks/0/number",
                "/progress/1/name"
            ],
            "誤りの場所が違う"
        );
    }

    #[test]
    fn 読めない本文は400で形の合わない本文は422にする() {
        let syntax = parse(b"{").expect_err("壊れたJSONを読めた");
        let data = parse(br#"{"sent_at": 1}"#).expect_err("形の合わない報告を読めた");

        assert_eq!(syntax.status, 400, "壊れたJSONを400にしなかった");
        assert_eq!(
            syntax.kind, "/api/problems/invalid-json",
            "壊れたJSONの種類が違う"
        );
        assert!(syntax.position.is_some(), "読めなかった位置を返さなかった");
        assert_eq!(data.status, 422, "形の合わない報告を422にしなかった");
        assert_eq!(
            data.kind, "/api/problems/invalid-report",
            "形の合わない報告の種類が違う"
        );
    }

    #[test]
    fn utcのrfc3339の日付時刻だけを受け入れる() {
        for (sent_at, valid) in [
            ("2026-11-19T08:00:00Z", true),
            ("2026-11-19t08:00:00.123456789012z", true),
            ("2026-11-19T08:00:00+00:00", true),
            ("2026-11-19T09:00:00+01:00", false),
            ("2026-11-19T08:00:00-00:00", false),
            ("20261119T080000Z", false),
            ("2026-11-19T08:00Z", false),
            ("2026-02-30T08:00:00Z", false),
            ("2026-11-19T08:00:00.Z", false),
            ("2026-11-19T08:00:00,1Z", false),
        ] {
            let mut body = report();
            body["sent_at"] = json!(sent_at);
            let errors = validate(&parsed(&body));
            if valid {
                assert!(
                    errors.is_empty(),
                    "{sent_at} をUTCの日時として受け入れなかった"
                );
            } else {
                assert_eq!(errors.len(), 1, "{sent_at} の指摘数が違う");
                assert_eq!(errors[0].pointer, "/sent_at", "{sent_at} の指摘箇所が違う");
            }
        }
    }

    #[test]
    fn 保存の順番が前後しても新しい受信時刻の報告を保持する() {
        let mut connection = Connection::open_in_memory().expect("テストのDBを開けなかった");
        for migration in db::MIGRATIONS {
            connection
                .execute_batch(migration)
                .expect("マイグレーションに失敗した");
        }
        let old = "2026-11-19T08:00:00Z"
            .parse()
            .expect("古い時刻を読めなかった");
        let new = "2026-11-19T08:00:00.1Z"
            .parse()
            .expect("新しい時刻を読めなかった");
        let retried_at = "2026-11-19T08:00:01Z"
            .parse()
            .expect("再送の時刻を読めなかった");

        save_status(&mut connection, "Annict", "古い内容", old).expect("初回の保存に失敗した");
        save_status(&mut connection, "Annict", "新しい内容", new)
            .expect("新しい報告を保存できなかった");
        save_status(&mut connection, "Annict", "保存が遅れた内容", old).expect("応答に失敗した");
        save_status(&mut connection, "Annict", "新しい内容", retried_at)
            .expect("再送への応答に失敗した");

        let (report, received_at): (String, String) = connection
            .query_row(
                "SELECT report, received_at FROM project_statuses",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("保存結果を読めなかった");
        assert_eq!(report, "新しい内容", "受信時刻が古い報告で上書きされた");
        assert_eq!(received_at, new.to_string(), "受信時刻が後退した");
    }

    #[test]
    fn content_typeがjsonかを引数と大文字小文字に関わらず見分ける() {
        for (value, expected) in [
            (Some("application/json"), true),
            (Some("Application/JSON; charset=utf-8"), true),
            (Some("text/plain"), false),
            (None, false),
        ] {
            let mut headers = HeaderMap::new();
            if let Some(value) = value {
                headers.insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_str(value).expect("ヘッダーの値を作れなかった"),
                );
            }

            assert_eq!(is_json(&headers), expected, "{value:?} の見分けが違う");
        }
    }
}
