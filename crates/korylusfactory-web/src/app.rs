//! ウェブアプリのルーティングと、すべての応答に掛ける処理 (認証・保護のヘッダー・圧縮)。

use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::middleware;
use axum::response::IntoResponse as _;
use axum::response::Response;
use axum::routing::{get, put};
use jiff::Timestamp;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::config::Config;
use crate::db::{self, Database};
use crate::events::{self, Events};
use crate::{api, assets, auth, page, projects};

/// すべての応答に付ける保護のヘッダー。HTMLとAPIで値の違うヘッダーは、それぞれの応答で付ける。
const SECURITY_HEADERS: [(HeaderName, &str); 3] = [
    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    (header::REFERRER_POLICY, "same-origin"),
    (
        HeaderName::from_static("permissions-policy"),
        "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
    ),
];

/// 要求を処理するときに使う、ウェブアプリの状態。
#[derive(Debug, Clone)]
pub struct AppState {
    /// ウェブアプリの設定。
    pub config: Arc<Config>,
    /// ウェブアプリのDB。
    pub database: Database,
    /// 開いている画面への知らせの送り口。
    pub events: Events,
}

/// ウェブアプリのルーティングを組み立てる。
///
/// すべての要求は、まず主を見分け ([`auth::authenticate`])、見分けられなければ401か403で拒否する。
/// そのうえで、人間の画面は人間だけ、ディレクターのAPIはディレクターだけが呼べる。
pub fn router(state: AppState) -> Router {
    let human = Router::new()
        .route("/", get(home))
        .route("/projects/{project}", get(project))
        .route(events::PATH, get(events::stream))
        .route(&format!("{}{{file}}", assets::PREFIX), get(assets::serve))
        .route_layer(middleware::from_fn(auth::require_human));
    let director = Router::new()
        .route(api::STATUS_PATH, put(api::put_status))
        .route_layer(middleware::from_fn(auth::require_director));
    let routes = Router::new()
        .merge(human)
        .merge(director)
        .fallback(not_found)
        .with_state(state.clone());
    wrap(routes, state.config)
}

/// ルートに、すべての要求と応答に掛ける処理 (パニックの受け止め・認証・圧縮・保護のヘッダー) を掛ける。
fn wrap(routes: Router, config: Arc<Config>) -> Router {
    let mut app = routes
        .layer(middleware::from_fn_with_state(config, auth::authenticate))
        .layer(CatchPanicLayer::custom(page::panic_response))
        .layer(middleware::from_fn(api::complete_response))
        .layer(CompressionLayer::new());
    // 拒否した応答とエラーの応答にも付けるよう、認証より外側に置く。
    for (name, value) in SECURITY_HEADERS {
        app = app.layer(SetResponseHeaderLayer::if_not_present(
            name,
            HeaderValue::from_static(value),
        ));
    }
    app
}

/// `GET /`: プロジェクトの一覧。
async fn home(State(state): State<AppState>) -> Response {
    let Ok(received) = projects::load(&state.database)
        .await
        .inspect_err(log_load_error)
    else {
        return page::internal_error();
    };
    let items = projects::list(&state.config.directors, &received, Timestamp::now());
    page::render(StatusCode::OK, &page::ProjectList::new(items))
}

/// `GET /projects/{project}`: プロジェクトの画面。設定に無いプロジェクトには404を返す。
async fn project(
    State(state): State<AppState>,
    name: Result<Path<String>, PathRejection>,
) -> Response {
    let Ok(Path(segment)) = name else {
        return page::not_found();
    };
    let name = projects::name_from_segment(&segment);
    if !state
        .config
        .directors
        .iter()
        .any(|director| director.project == name)
    {
        return page::not_found();
    }
    let Ok(received) = projects::load_one(&state.database, name)
        .await
        .inspect_err(log_load_error)
    else {
        return page::internal_error();
    };
    let detail = projects::detail(name, received.as_ref(), Timestamp::now());
    page::render(StatusCode::OK, &page::ProjectPage::new(detail))
}

fn log_load_error(error: &db::Error) {
    // 原因のSQLiteのエラー文には保存した値が入ることがあるため、原因をたどらずに記録する。
    log::error!("プロジェクトの報告を読めません: {error}");
}

/// どのルートにも当たらない要求に返す応答。APIのパスにはProblem Detailsで返す。
async fn not_found(request: Request) -> Response {
    if api::is_api(request.uri().path()) {
        return api::Problem::not_found().into_response();
    }
    page::not_found()
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use axum::body::{self, Body};
    use axum::http::{Request, Response};
    use tower::ServiceExt as _;

    use super::*;
    use crate::auth::{EMAIL_HEADER, SOURCE_VM_HEADER};

    struct Fixture {
        state: AppState,
        _dir: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れなかった");
        let config = Config::parse(
            Path::new("config.json"),
            r#"{
                "database": "factory.sqlite3",
                "owners": ["owner@example.com"],
                "directors": [{"vm": "annict-factory", "project": "Annict"}]
            }"#,
        )
        .expect("設定を読み取れなかった");
        let database =
            Database::open(&dir.path().join("factory.sqlite3")).expect("DBを開けなかった");
        Fixture {
            state: AppState {
                config: Arc::new(config),
                database,
                events: Events::new(),
            },
            _dir: dir,
        }
    }

    async fn send(app: Router, request: Request<Body>) -> Response<Body> {
        app.oneshot(request).await.expect("要求を処理できなかった")
    }

    fn get_as(path: &str, headers: &[(&HeaderName, &str)]) -> Request<Body> {
        let mut builder = Request::get(path);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).expect("要求を作れなかった")
    }

    #[tokio::test]
    async fn 人間はトップページを開ける() {
        let response = send(
            router(fixture().state),
            get_as("/", &[(&EMAIL_HEADER, "owner@example.com")]),
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "人間がトップページを開けなかった"
        );
    }

    #[tokio::test]
    async fn ディレクターは人間の画面を開けない() {
        let response = send(
            router(fixture().state),
            get_as(
                "/",
                &[
                    (&SOURCE_VM_HEADER, "annict-factory"),
                    (&EMAIL_HEADER, "owner@example.com"),
                ],
            ),
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "ディレクターが人間の画面を開けた"
        );
    }

    #[tokio::test]
    async fn 人間はディレクターのapiを呼べない() {
        let fixture = fixture();
        let app = wrap(
            Router::new()
                .route("/api", get(|| async { "" }))
                .route_layer(middleware::from_fn(auth::require_director)),
            fixture.state.config,
        );

        let human = send(
            app.clone(),
            get_as("/api", &[(&EMAIL_HEADER, "owner@example.com")]),
        )
        .await;
        let director = send(
            app,
            get_as("/api", &[(&SOURCE_VM_HEADER, "annict-factory")]),
        )
        .await;

        assert_eq!(
            human.status(),
            StatusCode::FORBIDDEN,
            "人間がディレクターのAPIを呼べた"
        );
        assert_eq!(
            director.status(),
            StatusCode::OK,
            "ディレクターがディレクターのAPIを呼べなかった"
        );
    }

    #[tokio::test]
    async fn 主の分からない要求は存在しないパスでも401で拒否する() {
        let response = send(router(fixture().state), get_as("/missing", &[])).await;

        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "パスがあるかどうかを未認証の要求に見せた"
        );
        assert_eq!(
            response.headers().get(header::WWW_AUTHENTICATE),
            Some(&HeaderValue::from_static(
                r#"Bearer realm="exe.dev", error_uri="https://exe.dev/docs/https-tokens-for-vms""#,
            )),
            "401の応答でexe.devの認証チャレンジを返さなかった"
        );
    }

    #[tokio::test]
    async fn 人間が存在しないパスを開くと404を返す() {
        let response = send(
            router(fixture().state),
            get_as("/missing", &[(&EMAIL_HEADER, "owner@example.com")]),
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "404を返さなかった"
        );
    }

    #[tokio::test]
    async fn 拒否した応答にも保護のヘッダーを付ける() {
        for headers in [vec![], vec![(&EMAIL_HEADER, "owner@example.com")]] {
            let response = send(router(fixture().state), get_as("/", &headers)).await;

            for (name, value) in SECURITY_HEADERS {
                assert_eq!(
                    response.headers().get(&name).and_then(|v| v.to_str().ok()),
                    Some(value),
                    "{} の応答に {name} が付いていない",
                    response.status()
                );
            }
        }
    }

    #[tokio::test]
    async fn 圧縮を受け付ける要求にはテキストを圧縮して返す() {
        let fixture = fixture();
        // 圧縮するのは一定の大きさを超える応答だけのため、大きな本文を返すルートで確かめる。
        let app = wrap(
            Router::new().route("/large", get(|| async { "あ".repeat(1000) })),
            fixture.state.config,
        );

        let response = send(
            app,
            Request::get("/large")
                .header(&EMAIL_HEADER, "owner@example.com")
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .expect("要求を作れなかった"),
        )
        .await;

        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("gzip"),
            "gzipで圧縮しなかった"
        );
        let compressed = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("本文を読めなかった");
        assert!(
            compressed.len() < 3000,
            "本文が小さくなっていない: {} バイト",
            compressed.len()
        );
    }

    #[tokio::test]
    async fn 処理の途中でパニックしたら中身を出さずに500の画面を返す() {
        async fn fail() -> &'static str {
            panic!("秘密の中身");
            #[expect(
                unreachable_code,
                reason = "パニックするハンドラーの戻り値の型を決めるため"
            )]
            ""
        }
        let fixture = fixture();
        let app = wrap(
            Router::new()
                .route("/panic", get(fail))
                .route("/api/panic", get(fail)),
            fixture.state.config,
        );

        let response = send(
            app.clone(),
            get_as("/panic", &[(&EMAIL_HEADER, "owner@example.com")]),
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "500を返さなかった"
        );
        assert_eq!(
            response.headers().get(header::CONTENT_SECURITY_POLICY),
            Some(&HeaderValue::from_static(page::CONTENT_SECURITY_POLICY)),
            "500の画面にCSPが付いていない"
        );
        let html = String::from_utf8(
            body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("本文を読めなかった")
                .to_vec(),
        )
        .expect("本文がUTF-8ではない");
        assert!(
            html.contains("エラーが起きました"),
            "500の画面を返さなかった"
        );
        assert!(!html.contains("秘密の中身"), "パニックの中身を画面に出した");

        let response = send(
            app,
            get_as("/api/panic", &[(&SOURCE_VM_HEADER, "annict-factory")]),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "APIで500を返さなかった"
        );
        assert_api_headers(&response);
        let json = problem(response).await;
        assert_eq!(
            json["type"], "/api/problems/internal-error",
            "APIのパニックを500のProblem Detailsにしなかった"
        );
        assert!(
            !json.to_string().contains("秘密の中身"),
            "パニックの中身をAPIに出した"
        );
    }

    #[tokio::test]
    async fn 人間に埋め込んだcssを配信する() {
        let response = send(
            router(fixture().state),
            get_as(
                assets::stylesheet_path(),
                &[(&EMAIL_HEADER, "owner@example.com")],
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK, "CSSを配信しなかった");
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE),
            Some(&HeaderValue::from_static("text/css; charset=utf-8")),
            "CSSのContent-Typeが違う"
        );
    }

    #[tokio::test]
    async fn 存在しないパスには404の画面を返す() {
        let response = send(
            router(fixture().state),
            get_as("/missing", &[(&EMAIL_HEADER, "owner@example.com")]),
        )
        .await;

        assert_eq!(
            response.headers().get(header::CONTENT_TYPE),
            Some(&HeaderValue::from_static("text/html; charset=utf-8")),
            "404の画面をHTMLで返さなかった"
        );
    }

    const REPORT: &str = r#"{
        "sent_at": "2026-11-19T08:00:00Z",
        "korylusfactory": {"version": "0.1.0", "commit": "abc1234"},
        "running": true,
        "progress": [],
        "upcoming": [{"title": "メッセージ 作業計画書"}],
        "future": "知らない項目"
    }"#;

    fn put_as(headers: &[(HeaderName, &str)], body: impl AsRef<[u8]>) -> Request<Body> {
        let mut builder = Request::put(api::STATUS_PATH);
        for (name, value) in headers {
            builder = builder.header(name, *value);
        }
        builder
            .body(Body::from(body.as_ref().to_vec()))
            .expect("要求を作れなかった")
    }

    fn director() -> Vec<(HeaderName, &'static str)> {
        vec![
            (SOURCE_VM_HEADER, "annict-factory"),
            (header::CONTENT_TYPE, "application/json"),
        ]
    }

    async fn saved(state: &AppState) -> Vec<(String, String, String)> {
        state
            .database
            .call(|connection| {
                let mut statement = connection.prepare(
                    "SELECT project, report, received_at FROM project_statuses ORDER BY project",
                )?;
                statement
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                    .collect()
            })
            .await
            .expect("保存した報告を読めなかった")
    }

    async fn problem(response: Response<Body>) -> serde_json::Value {
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE),
            Some(&HeaderValue::from_static("application/problem+json")),
            "Problem Detailsで返さなかった"
        );
        let bytes = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("本文を読めなかった");
        serde_json::from_slice(&bytes).expect("本文がJSONではない")
    }

    fn assert_api_headers(response: &Response<Body>) {
        for (name, value) in api::HEADERS {
            assert_eq!(
                response.headers().get(&name),
                Some(&HeaderValue::from_static(value)),
                "{} の応答の {name} が違う",
                response.status()
            );
        }
    }

    #[tokio::test]
    async fn ディレクターの報告を設定のプロジェクトの行に送られたまま保存する() {
        let fixture = fixture();

        let response = send(router(fixture.state.clone()), put_as(&director(), REPORT)).await;

        assert_eq!(
            response.status(),
            StatusCode::NO_CONTENT,
            "204を返さなかった"
        );
        let rows = saved(&fixture.state).await;
        assert_eq!(rows.len(), 1, "保存した行の数が違う");
        let (project, report, received_at) = &rows[0];
        assert_eq!(
            project, "Annict",
            "設定でVM名に対応付けたプロジェクトに保存しなかった"
        );
        assert_eq!(report, REPORT, "送られた本文をそのまま保存しなかった");
        assert!(
            received_at.parse::<jiff::Timestamp>().is_ok(),
            "受け取った時刻がRFC 3339ではない: {received_at}"
        );
    }

    #[tokio::test]
    async fn 同じプロジェクトの報告を送り直すと行を置き換える() {
        let fixture = fixture();
        let second = REPORT.replace("\"running\": true", "\"running\": false");

        send(router(fixture.state.clone()), put_as(&director(), REPORT)).await;
        let response = send(router(fixture.state.clone()), put_as(&director(), &second)).await;

        assert_eq!(
            response.status(),
            StatusCode::NO_CONTENT,
            "204を返さなかった"
        );
        let rows = saved(&fixture.state).await;
        assert_eq!(rows.len(), 1, "行を置き換えずに足した");
        assert_eq!(rows[0].1, second, "新しい報告に置き換えなかった");
    }

    #[tokio::test]
    async fn 報告を保存できなければ500のproblem_detailsを返す() {
        let fixture = fixture();
        fixture
            .state
            .database
            .call(|connection| connection.execute_batch("DROP TABLE project_statuses"))
            .await
            .expect("保存先のテーブルを消せなかった");

        let response = send(router(fixture.state.clone()), put_as(&director(), REPORT)).await;

        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "保存の失敗を500にしなかった"
        );
        assert_api_headers(&response);
        assert_eq!(
            problem(response).await["type"],
            "/api/problems/internal-error",
            "保存の失敗の問題の種類が違う"
        );
    }

    #[tokio::test]
    async fn apiの応答にapi用のヘッダーを付ける() {
        let app = router(fixture().state);
        let requests = [
            put_as(&director(), REPORT),
            put_as(&[], REPORT),
            put_as(&[(SOURCE_VM_HEADER, "unknown")], REPORT),
            put_as(&[(EMAIL_HEADER, "owner@example.com")], REPORT),
            get_as("/api/v1/missing", &[(&SOURCE_VM_HEADER, "annict-factory")]),
        ];

        for request in requests {
            assert_api_headers(&send(app.clone(), request).await);
        }
    }

    #[tokio::test]
    async fn 使えないメソッドと本文の上限超過をproblem_detailsで返す() {
        let app = router(fixture().state);
        let method = send(
            app.clone(),
            get_as(api::STATUS_PATH, &[(&SOURCE_VM_HEADER, "annict-factory")]),
        )
        .await;
        assert_eq!(
            method.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "405を返さなかった"
        );
        assert_eq!(
            method.headers().get(header::ALLOW),
            Some(&HeaderValue::from_static("PUT")),
            "405で許可するメソッドを返さなかった"
        );
        assert_api_headers(&method);
        assert_eq!(
            problem(method).await["type"],
            "/api/problems/method-not-allowed",
            "405の問題の種類が違う"
        );

        let oversized = vec![b' '; 2 * 1024 * 1024 + 1];
        let length = send(app, put_as(&director(), oversized)).await;
        assert_eq!(
            length.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "413を返さなかった"
        );
        assert_api_headers(&length);
        assert_eq!(
            problem(length).await["type"],
            "/api/problems/payload-too-large",
            "413の問題の種類が違う"
        );
    }

    #[tokio::test]
    async fn 壊れたjsonは400で拒否して保存しない() {
        let fixture = fixture();
        let app = router(fixture.state.clone());
        let mut non_utf8 = REPORT
            .strip_suffix('}')
            .expect("報告がオブジェクトではない")
            .as_bytes()
            .to_vec();
        non_utf8.extend_from_slice(b", \"encoding_sample\": \"\xff\"}");
        for body in [br#"{"sent_at": 1,"#.as_slice(), non_utf8.as_slice()] {
            let response = send(app.clone(), put_as(&director(), body)).await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "400を返さなかった"
            );
            let problem = problem(response).await;
            assert_eq!(
                problem["type"], "/api/problems/invalid-json",
                "問題の種類が違う"
            );
            assert!(problem["position"].is_object(), "誤りの位置を返さなかった");
        }
        assert!(
            saved(&fixture.state).await.is_empty(),
            "壊れた報告を保存した"
        );
    }

    #[tokio::test]
    async fn 同一の報告を再送しても受信時刻を更新しない() {
        let fixture = fixture();
        let app = router(fixture.state.clone());
        send(app.clone(), put_as(&director(), REPORT)).await;
        let first = saved(&fixture.state).await;
        assert_eq!(first.len(), 1, "初回の報告を保存しなかった");

        let response = send(app, put_as(&director(), REPORT)).await;
        assert_eq!(
            response.status(),
            StatusCode::NO_CONTENT,
            "再送に204を返さなかった"
        );
        assert_eq!(
            saved(&fixture.state).await,
            first,
            "再送で保存済みの報告が変わった"
        );
    }

    #[tokio::test]
    async fn 誤った報告は理由に合うステータスのproblem_detailsで拒否して保存しない() {
        let fixture = fixture();
        let invalid = REPORT.replace("2026-11-19T08:00:00Z", "きのう");
        let cases = [
            (
                vec![(SOURCE_VM_HEADER, "annict-factory")],
                REPORT,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "/api/problems/unsupported-media-type",
            ),
            (
                director(),
                "{",
                StatusCode::BAD_REQUEST,
                "/api/problems/invalid-json",
            ),
            (
                director(),
                r#"{"sent_at": 1}"#,
                StatusCode::UNPROCESSABLE_ENTITY,
                "/api/problems/invalid-report",
            ),
            (
                director(),
                invalid.as_str(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "/api/problems/invalid-report",
            ),
        ];

        for (headers, body, status, kind) in cases {
            let response = send(router(fixture.state.clone()), put_as(&headers, body)).await;

            assert_eq!(response.status(), status, "{kind} のステータスが違う");
            let problem = problem(response).await;
            assert_eq!(problem["type"], kind, "問題の種類が違う");
            assert_eq!(problem["status"], status.as_u16(), "本文のstatusが違う");
        }
        assert!(
            saved(&fixture.state).await.is_empty(),
            "誤った報告を保存した"
        );
    }

    #[tokio::test]
    async fn apiは主の分からない要求と人間をproblem_detailsで拒否する() {
        let fixture = fixture();

        let anonymous = send(
            router(fixture.state.clone()),
            put_as(&[(header::CONTENT_TYPE, "application/json")], REPORT),
        )
        .await;
        let human = send(
            router(fixture.state.clone()),
            put_as(
                &[
                    (EMAIL_HEADER, "owner@example.com"),
                    (header::CONTENT_TYPE, "application/json"),
                ],
                REPORT,
            ),
        )
        .await;

        assert_eq!(
            anonymous.status(),
            StatusCode::UNAUTHORIZED,
            "401を返さなかった"
        );
        assert!(
            anonymous.headers().contains_key(header::WWW_AUTHENTICATE),
            "401に認証の要求を付けなかった"
        );
        assert_eq!(
            problem(anonymous).await["type"],
            "/api/problems/unauthenticated",
            "401の問題の種類が違う"
        );
        assert_eq!(human.status(), StatusCode::FORBIDDEN, "人間がAPIを呼べた");
        assert_eq!(
            problem(human).await["type"],
            "/api/problems/forbidden",
            "403の問題の種類が違う"
        );
        assert!(
            saved(&fixture.state).await.is_empty(),
            "拒否した報告を保存した"
        );
    }

    #[tokio::test]
    async fn 存在しないapiのパスには404のproblem_detailsを返す() {
        let response = send(
            router(fixture().state),
            get_as("/api/v1/missing", &[(&SOURCE_VM_HEADER, "annict-factory")]),
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "404を返さなかった"
        );
        assert_eq!(
            problem(response).await["type"],
            "/api/problems/not-found",
            "404の問題の種類が違う"
        );
    }

    async fn html(response: Response<Body>) -> String {
        let bytes = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("本文を読めなかった");
        String::from_utf8(bytes.to_vec()).expect("本文がUTF-8ではない")
    }

    #[tokio::test]
    async fn 一覧に受け取った報告からプロジェクトの状態を出し_文面はエスケープする() {
        let fixture = fixture();
        let report = serde_json::json!({
            "sent_at": "2026-11-19T08:00:00Z",
            "korylusfactory": {"version": "0.1.0", "commit": "abc1234"},
            "running": false,
            "progress": [{
                "name": "withdrawal",
                "title": "<script>退会機能</script> 作業計画書",
                "url": "https://wikino.example.com/s/example/pages/1",
                "task": "1-2",
                "step": {"kind": "implement", "label": "実装"},
                "wait": {"kind": "question", "label": "セッションからの質問", "reason": "理由"}
            }]
        });
        send(
            router(fixture.state.clone()),
            put_as(&director(), report.to_string()),
        )
        .await;

        let response = send(
            router(fixture.state),
            get_as("/", &[(&EMAIL_HEADER, "owner@example.com")]),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK, "一覧を開けなかった");
        let html = html(response).await;
        for part in [
            "<h2>Annict</h2>",
            "人間待ちが1件あります。",
            r#"data-variant="warning""#,
            r#"role="list">"#,
            r#"<a class="item" data-size="sm" href="/projects/Annict" id="project-link-/projects/Annict">"#,
            r#"<p class="wrap-anywhere">&#60;script&#62;退会機能&#60;/script&#62; 作業計画書</p>"#,
            r#"<p class="line-clamp-none wrap-anywhere">タスク1-2の実装でセッションからの質問</p>"#,
        ] {
            assert!(html.contains(part), "一覧に {part} が無い");
        }
        assert!(
            !html.contains("<script>退会機能"),
            "報告の文面をエスケープせずに出した"
        );
    }

    #[tokio::test]
    async fn 一覧はsseの知らせと時間の経過で自分を取り直す() {
        let response = send(
            router(fixture().state),
            get_as("/", &[(&EMAIL_HEADER, "owner@example.com")]),
        )
        .await;

        let html = html(response).await;
        for part in [
            r#"hx-sse:connect="/events""#,
            r##"hx-select="#projects > *""##,
            r#"hx-swap="innerHTML""#,
            r#"hx-trigger="status-changed from:#events, every 60s""#,
            "まだ連絡がありません",
        ] {
            assert!(html.contains(part), "一覧に {part} が無い");
        }
    }

    #[tokio::test]
    async fn sseは圧縮せずにキャッシュさせず_報告を受け取るとプロジェクトを知らせる() {
        let fixture = fixture();
        let response = send(
            router(fixture.state.clone()),
            Request::get(events::PATH)
                .header(&EMAIL_HEADER, "owner@example.com")
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .expect("要求を作れなかった"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK, "SSEを開けなかった");
        for (name, value) in [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-store"),
            (HeaderName::from_static("x-robots-tag"), "noindex"),
        ] {
            assert_eq!(
                response.headers().get(&name).and_then(|v| v.to_str().ok()),
                Some(value),
                "SSEの {name} が違う"
            );
        }
        assert_eq!(
            response.headers().get(header::CONTENT_ENCODING),
            None,
            "SSEを圧縮した"
        );
        send(router(fixture.state.clone()), put_as(&director(), REPORT)).await;
        let mut body = response.into_body().into_data_stream();
        let chunk = tokio::time::timeout(
            Duration::from_secs(5),
            futures_util::StreamExt::next(&mut body),
        )
        .await
        .expect("知らせが届くまでの上限を過ぎた")
        .expect("知らせが届かなかった")
        .expect("本文を読めなかった");
        assert_eq!(
            chunk.as_ref(),
            b"event: status-changed\ndata: Annict\n\n",
            "知らせの中身が違う"
        );
    }

    #[tokio::test]
    async fn ディレクターはsseを開けない() {
        let response = send(
            router(fixture().state),
            get_as(events::PATH, &[(&SOURCE_VM_HEADER, "annict-factory")]),
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "ディレクターがSSEを開けた"
        );
    }

    /// 人間待ちのタスクと、これから進める作業計画書を持つ報告。
    fn detailed_report() -> serde_json::Value {
        serde_json::json!({
            "sent_at": "2026-11-19T08:00:00Z",
            "korylusfactory": {"version": "0.1.0", "commit": "abc1234"},
            "running": false,
            "progress": [{
                "name": "withdrawal",
                "title": "退会機能 作業計画書",
                "url": "https://wikino.example.com/s/example/pages/1",
                "task": "1-2",
                "step": {"kind": "implement", "label": "実装"},
                "wait": {"kind": "question", "label": "セッションからの質問", "reason": "<b>設計</b>を変えてよいか"},
                "tasks": [
                    {"number": "1-1", "title": "退会の画面を作る", "status": "done"},
                    {"number": "1-2", "title": "退会の処理を作る", "status": "in_progress"},
                    {"number": "1-3", "title": "退会を知らせるメールを送る", "status": "pending"}
                ]
            }],
            "upcoming": [
                {"title": "通知の改善 作業計画書", "url": "https://wikino.example.com/s/example/pages/2"},
                {"title": "危ないリンク 作業計画書", "url": "javascript:alert(1)"}
            ]
        })
    }

    #[tokio::test]
    async fn プロジェクトの画面に人間待ちの理由とタスクとこれから進める作業計画書を出す() {
        let fixture = fixture();
        send(
            router(fixture.state.clone()),
            put_as(&director(), detailed_report().to_string()),
        )
        .await;

        let response = send(
            router(fixture.state),
            get_as("/projects/Annict", &[(&EMAIL_HEADER, "owner@example.com")]),
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "プロジェクトの画面を開けなかった"
        );
        let html = html(response).await;
        for part in [
            "<title>Annict - Korylus Factory</title>",
            r#"<div class="alert" data-variant="warning">"#,
            "<h2>セッションからの質問</h2>",
            "&#60;b&#62;設計&#60;/b&#62;を変えてよいか",
            "3タスク中1つ済み ・ タスク1-2の実装",
            "退会の画面を作る",
            r#"<p class="line-clamp-none wrap-anywhere">済み</p>"#,
            r#"<p class="line-clamp-none wrap-anywhere">実装 ・ セッションからの質問</p>"#,
            r#"<p class="line-clamp-none wrap-anywhere">未着手</p>"#,
            r#"<a class="btn px-0 underline" data-variant="link" href="https://wikino.example.com/s/example/pages/1" id="active-plan-"#,
            r#"作業計画書を開く<span class="sr-only">（退会機能 作業計画書）</span>"#,
            r#"href="https://wikino.example.com/s/example/pages/2""#,
            "危ないリンク 作業計画書",
            r#"hx-get="/projects/Annict""#,
            r#"hx-trigger="status-changed from:#events, every 60s""#,
        ] {
            assert!(html.contains(part), "プロジェクトの画面に {part} が無い");
        }
        assert!(
            !html.contains("<b>設計</b>"),
            "人間待ちの理由をエスケープせずに出した"
        );
        assert!(
            !html.contains("javascript:"),
            "http・https以外のURLをリンクにした"
        );
    }

    #[tokio::test]
    async fn 報告をまだ受け取っていないプロジェクトの画面は不通を知らせるだけにする() {
        let response = send(
            router(fixture().state),
            get_as("/projects/Annict", &[(&EMAIL_HEADER, "owner@example.com")]),
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "プロジェクトの画面を開けなかった"
        );
        let html = html(response).await;
        assert!(
            html.contains(r#"<div class="alert" data-variant="destructive">"#)
                && html.contains("まだ状態の報告を受け取っていません"),
            "不通を知らせなかった"
        );
        assert!(
            !html.contains("これから進める作業計画書"),
            "受け取っていない報告の中身を出した"
        );
    }

    #[tokio::test]
    async fn 設定にある名前から詳細を開き_同じurlで取り直せる() {
        let mut fixture = fixture();
        let cases = [
            (".", "/projects/~."),
            ("..", "/projects/~.."),
            ("~.", "/projects/~~."),
            ("~~..", "/projects/~~~.."),
            ("~ordinary", "/projects/~ordinary"),
            ("a b/日", "/projects/a%20b%2F%E6%97%A5"),
        ];
        Arc::get_mut(&mut fixture.state.config)
            .expect("テスト用の設定を変更できなかった")
            .directors
            .extend(
                cases
                    .iter()
                    .enumerate()
                    .map(|(index, (name, _))| crate::config::Director {
                        vm: format!("director-{index}"),
                        project: (*name).to_owned(),
                    }),
            );
        let app = router(fixture.state);
        let owner = [(&EMAIL_HEADER, "owner@example.com")];
        let listing = html(send(app.clone(), get_as("/", &owner)).await).await;
        for (name, path) in cases {
            assert!(
                listing.contains(&format!("href=\"{path}\"")),
                "一覧のリンクが無い"
            );
            let response = send(app.clone(), get_as(path, &owner)).await;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{name:?} の詳細を開けなかった"
            );
            let detail = html(response).await;
            assert!(detail.contains(&format!("<title>{name} - Korylus Factory</title>")));
            assert!(
                detail.contains(&format!("hx-get=\"{path}\"")),
                "取り直すURLが違う"
            );
        }
    }

    #[tokio::test]
    async fn 設定に無いプロジェクトの画面は404にする() {
        for path in ["/projects/Unknown", "/projects/%FF"] {
            let response = send(
                router(fixture().state),
                get_as(path, &[(&EMAIL_HEADER, "owner@example.com")]),
            )
            .await;

            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{path} が404にならなかった"
            );
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE),
                Some(&HeaderValue::from_static("text/html; charset=utf-8")),
                "{path} の404を画面で返さなかった"
            );
        }
    }

    #[tokio::test]
    async fn ディレクターはプロジェクトの画面を開けない() {
        let response = send(
            router(fixture().state),
            get_as("/projects/Annict", &[(&SOURCE_VM_HEADER, "annict-factory")]),
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "ディレクターがプロジェクトの画面を開けた"
        );
    }
}
