//! ウェブアプリのルーティングと、すべての応答に掛ける処理 (認証・保護のヘッダー・圧縮)。

use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::middleware;
use axum::response::IntoResponse as _;
use axum::response::Response;
use axum::routing::{get, post, put};
use jiff::Timestamp;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::config::Config;
use crate::db::{self, Database};
use crate::events::{self, Events};
use crate::{agents, api, assets, auth, page, projects, settings};

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
        .route("/projects/{project}/settings", get(settings::show))
        .route(
            "/projects/{project}/settings/operation",
            post(settings::update_operation),
        )
        .route("/projects/{project}/settings/agents", post(agents::update))
        .route(events::PATH, get(events::stream))
        .route(&format!("{}{{file}}", assets::PREFIX), get(assets::serve))
        // 後に足した層ほど外側で動くため、人間かどうかを確かめてから送り元を確かめる。
        .route_layer(middleware::from_fn(auth::require_same_origin))
        .route_layer(middleware::from_fn(auth::require_human));
    let director = Router::new()
        .route(api::STATUS_PATH, put(api::put_status))
        .route(api::SETTINGS_PATH, get(api::get_settings))
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
    let Some(name) = projects::configured(&state.config.directors, &segment) else {
        return page::not_found();
    };
    let Ok(received) = projects::load_one(&state.database, name)
        .await
        .inspect_err(log_load_error)
    else {
        return page::internal_error();
    };
    let Ok(operation) = settings::load(&state.database, name)
        .await
        .inspect_err(|error| log::error!("プロジェクトの設定を読めません: {error}"))
    else {
        return page::internal_error();
    };
    let detail = projects::detail(name, received.as_ref(), operation, Timestamp::now());
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
    use axum::http::{Method, Request, Response};
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
        json(response).await
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
    async fn プロジェクトの画面に最近のセッションを出し_文面はエスケープする() {
        let fixture = fixture();
        let mut report = detailed_report();
        report["progress"][0]["sessions"] = serde_json::json!([
            {
                "task": "1-2",
                "step": {"kind": "implement", "label": "実装"},
                "role": {"kind": "implementer", "label": "作業者"},
                "agent": "claude",
                "model": "<i>opus</i>",
                "effort": "high",
                "started_at": "2026-11-19T07:50:00Z"
            },
            {
                "task": "1-1",
                "step": {"kind": "review", "label": "レビュー"},
                "review": 1,
                "role": {"kind": "reviewer", "label": "レビュアー"},
                "agent": "codex",
                "started_at": "2026-11-19T07:30:00Z",
                "ended_at": "2026-11-19T07:42:00Z",
                "result": {"kind": "recorded", "label": "<s>Request Changes</s>"}
            }
        ]);
        send(
            router(fixture.state.clone()),
            put_as(&director(), report.to_string()),
        )
        .await;

        let html = html(
            send(
                router(fixture.state),
                get_as("/projects/Annict", &[(&EMAIL_HEADER, "owner@example.com")]),
            )
            .await,
        )
        .await;

        for part in [
            "<h2>最近のセッション</h2>",
            r#"<h3 class="wrap-anywhere">1-2の実装</h3>"#,
            r#"作業者 ・ Claude Code ・ <span class="font-mono text-xs">&#60;i&#62;opus&#60;/i&#62;</span> ・ high ・ "#,
            " ・ 実行中",
            r#"<h3 class="wrap-anywhere">1-1のレビュー (1回目)</h3>"#,
            "レビュアー ・ Codex ・ 既定のモデル ・ 12分 ・ &#60;s&#62;Request Changes&#60;/s&#62;",
        ] {
            assert!(html.contains(part), "最近のセッションに {part} が無い");
        }
        assert!(
            html.find("1-2の実装") < html.find("1-1のレビュー (1回目)"),
            "新しいセッションを先に並べなかった"
        );
        assert!(
            !html.contains("<i>opus</i>") && !html.contains("<s>Request"),
            "セッションの文面をエスケープせずに出した"
        );
    }

    #[tokio::test]
    async fn セッションの記録が無ければ最近のセッションのカードを出さない() {
        let fixture = fixture();
        send(
            router(fixture.state.clone()),
            put_as(&director(), detailed_report().to_string()),
        )
        .await;

        let html = html(
            send(
                router(fixture.state),
                get_as("/projects/Annict", &[(&EMAIL_HEADER, "owner@example.com")]),
            )
            .await,
        )
        .await;

        assert!(
            !html.contains("最近のセッション"),
            "セッションの記録が無いのにカードを出した"
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

    static OWNER_EMAIL_HEADER: HeaderName = EMAIL_HEADER;

    fn owner() -> (&'static HeaderName, &'static str) {
        (&OWNER_EMAIL_HEADER, "owner@example.com")
    }

    fn post_form(path: &str, headers: &[(&HeaderName, &str)], body: &str) -> Request<Body> {
        let mut builder =
            Request::post(path).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder
            .body(Body::from(body.to_owned()))
            .expect("要求を作れなかった")
    }

    /// 人間が設定の画面から送るのと同じ、同じオリジンからの要求。
    fn from_settings(path: &str, body: &str) -> Request<Body> {
        post_form(
            path,
            &[owner(), (&auth::SEC_FETCH_SITE_HEADER, "same-origin")],
            body,
        )
    }

    const OPERATION_PATH: &str = "/projects/Annict/settings/operation";
    const PAUSE: &str = "operation=paused";
    const RESUME: &str = "operation=paused&operation=running";

    async fn saved_settings(state: &AppState) -> Vec<(String, i64)> {
        state
            .database
            .call(|connection| {
                let mut statement = connection
                    .prepare("SELECT project, paused FROM project_settings ORDER BY project")?;
                statement
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect()
            })
            .await
            .expect("保存した設定を読めなかった")
    }

    #[tokio::test]
    async fn プロジェクトの画面に設定の画面へのボタンを置く() {
        let response = send(
            router(fixture().state),
            get_as("/projects/Annict", &[owner()]),
        )
        .await;

        let html = html(response).await;
        assert!(
            html.contains(r#"href="/projects/Annict/settings""#)
                && html.contains(r#"aria-label="プロジェクトの設定""#),
            "設定の画面へのボタンが無い"
        );
    }

    #[tokio::test]
    async fn 設定を保存していないプロジェクトの設定の画面は稼働のスイッチを入れて出す() {
        let response = send(
            router(fixture().state),
            get_as("/projects/Annict/settings", &[owner()]),
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "設定の画面を開けなかった"
        );
        let html = html(response).await;
        for part in [
            "<title>Annictの設定 - Korylus Factory</title>",
            r#"href="/projects/Annict""#,
            r#"<label for="operation">このプロジェクトを稼働させる</label>"#,
            "オフにすると、実行中のセッションが終わったところで停止します。依頼への回答には応じます。",
            r#"method="post""#,
            r#"action="/projects/Annict/settings/operation""#,
            r#"hx-post="/projects/Annict/settings/operation""#,
            r#"hx-trigger="change, submit""#,
            r#"hx-sync="drop""#,
            assets::settings_path(),
            "data-operation-error",
            "data-operation-fallback",
            r#"<input type="hidden" name="operation" value="paused" />"#,
            r#"role="switch""#,
            r#"value="running""#,
        ] {
            assert!(html.contains(part), "設定の画面に {part} が無い");
        }
        let submit = r#"type="submit">保存する</button>"#;
        assert_eq!(html.matches(submit).count(), 1, "保存のボタンが1つではない");
        assert!(
            html.split_once("data-operation-fallback")
                .is_some_and(|(_, rest)| rest.contains(submit)),
            "htmxが動かないときに出す枠に、保存のボタンを置いていない"
        );
        assert!(html.contains(" checked"), "稼働のスイッチを入れていない");
        assert!(!html.contains("人間キュー"), "画面に人間キューと書いた");
    }

    #[tokio::test]
    async fn 停止に切り替えると保存して設定の画面へ戻し_スイッチを切って出す() {
        let fixture = fixture();
        let app = router(fixture.state.clone());

        let response = send(app.clone(), from_settings(OPERATION_PATH, PAUSE)).await;

        assert_eq!(
            response.status(),
            StatusCode::SEE_OTHER,
            "303で戻さなかった"
        );
        assert_eq!(
            response.headers().get(header::LOCATION),
            Some(&HeaderValue::from_static("/projects/Annict/settings")),
            "設定の画面へ戻さなかった"
        );
        assert_eq!(
            saved_settings(&fixture.state).await,
            [("Annict".to_owned(), 1)],
            "停止を保存しなかった"
        );
        let html = html(send(app, get_as("/projects/Annict/settings", &[owner()])).await).await;
        assert!(
            !html.contains(" checked"),
            "停止なのにスイッチを入れて出した"
        );
    }

    #[tokio::test]
    async fn 稼働に戻すとスイッチの値を保存する() {
        let fixture = fixture();
        let app = router(fixture.state.clone());
        send(app.clone(), from_settings(OPERATION_PATH, PAUSE)).await;

        let response = send(app, from_settings(OPERATION_PATH, RESUME)).await;

        assert_eq!(
            response.status(),
            StatusCode::SEE_OTHER,
            "303で戻さなかった"
        );
        assert_eq!(
            saved_settings(&fixture.state).await,
            [("Annict".to_owned(), 0)],
            "稼働に戻さなかった"
        );
    }

    #[tokio::test]
    async fn 設定に無いプロジェクトの設定の画面と保存は404にする() {
        let fixture = fixture();
        let app = router(fixture.state.clone());

        let show = send(
            app.clone(),
            get_as("/projects/Unknown/settings", &[owner()]),
        )
        .await;
        let save = send(
            app,
            from_settings("/projects/Unknown/settings/operation", PAUSE),
        )
        .await;

        assert_eq!(
            show.status(),
            StatusCode::NOT_FOUND,
            "画面が404にならなかった"
        );
        assert_eq!(
            save.status(),
            StatusCode::NOT_FOUND,
            "保存が404にならなかった"
        );
        assert!(
            saved_settings(&fixture.state).await.is_empty(),
            "設定に無いプロジェクトの設定を保存した"
        );
    }

    #[tokio::test]
    async fn 稼働か停止のどちらでもない値は400の画面で拒否して保存しない() {
        let fixture = fixture();
        let app = router(fixture.state.clone());

        for body in [
            "",
            "operation=",
            "operation=on",
            "other=paused",
            "operation=%FF",
        ] {
            let response = send(app.clone(), from_settings(OPERATION_PATH, body)).await;

            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{body:?} を400にしなかった"
            );
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE),
                Some(&HeaderValue::from_static("text/html; charset=utf-8")),
                "{body:?} の400を画面で返さなかった"
            );
        }
        let json = send(
            app,
            Request::post(OPERATION_PATH)
                .header(&EMAIL_HEADER, "owner@example.com")
                .header(&auth::SEC_FETCH_SITE_HEADER, "same-origin")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"operation": "paused"}"#))
                .expect("要求を作れなかった"),
        )
        .await;
        assert_eq!(
            json.status(),
            StatusCode::BAD_REQUEST,
            "フォームではない本文を400にしなかった"
        );
        assert!(
            saved_settings(&fixture.state).await.is_empty(),
            "受け付けられない値で設定を保存した"
        );
    }

    #[tokio::test]
    async fn 同じオリジンから送られていない状態を変える要求は403の画面で拒否して保存しない() {
        let fixture = fixture();
        let app = router(fixture.state.clone());
        let cases: [&[(&HeaderName, &str)]; 4] = [
            &[owner()],
            &[owner(), (&auth::SEC_FETCH_SITE_HEADER, "cross-site")],
            &[owner(), (&auth::SEC_FETCH_SITE_HEADER, "same-site")],
            &[owner(), (&auth::SEC_FETCH_SITE_HEADER, "none")],
        ];

        for headers in cases {
            let response = send(app.clone(), post_form(OPERATION_PATH, headers, PAUSE)).await;

            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{headers:?} の要求を403にしなかった"
            );
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE),
                Some(&HeaderValue::from_static("text/html; charset=utf-8")),
                "{headers:?} の403を画面で返さなかった"
            );
        }
        let mut doubled = from_settings(OPERATION_PATH, PAUSE);
        doubled.headers_mut().append(
            auth::SEC_FETCH_SITE_HEADER,
            HeaderValue::from_static("cross-site"),
        );
        assert_eq!(
            send(app, doubled).await.status(),
            StatusCode::FORBIDDEN,
            "重なった Sec-Fetch-Site を受け付けた"
        );
        assert!(
            saved_settings(&fixture.state).await.is_empty(),
            "拒否した要求で設定を保存した"
        );
    }

    #[tokio::test]
    async fn 開発用の身元で動かすときも送り元を確かめる() {
        let mut fixture = fixture();
        Arc::get_mut(&mut fixture.state.config)
            .expect("テスト用の設定を変更できなかった")
            .dev_identity = Some(crate::config::DevIdentity::Email(
            "owner@example.com".to_owned(),
        ));
        let app = router(fixture.state.clone());

        let cross = send(
            app.clone(),
            post_form(
                OPERATION_PATH,
                &[(&auth::SEC_FETCH_SITE_HEADER, "cross-site")],
                PAUSE,
            ),
        )
        .await;
        let same = send(
            app,
            post_form(
                OPERATION_PATH,
                &[(&auth::SEC_FETCH_SITE_HEADER, "same-origin")],
                PAUSE,
            ),
        )
        .await;

        assert_eq!(
            cross.status(),
            StatusCode::FORBIDDEN,
            "開発用の身元でほかのサイトからの要求を受け付けた"
        );
        assert_eq!(
            same.status(),
            StatusCode::SEE_OTHER,
            "開発用の身元で同じオリジンからの要求を受け付けなかった"
        );
    }

    #[tokio::test]
    async fn 設定を読み書きできなければ500の画面を返す() {
        let fixture = fixture();
        fixture
            .state
            .database
            .call(|connection| connection.execute_batch("DROP TABLE project_settings"))
            .await
            .expect("設定のテーブルを消せなかった");
        let app = router(fixture.state.clone());

        let project = send(app.clone(), get_as("/projects/Annict", &[owner()])).await;
        let show = send(app.clone(), get_as("/projects/Annict/settings", &[owner()])).await;
        let save = send(app, from_settings(OPERATION_PATH, PAUSE)).await;

        assert_eq!(
            project.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "設定を読めないときにプロジェクトの画面を500にしなかった"
        );

        assert_eq!(
            show.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "読めないときに500にしなかった"
        );
        assert_eq!(
            save.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "保存できないときに500にしなかった"
        );
    }

    #[tokio::test]
    async fn ディレクターは設定の画面を開けず設定も変えられない() {
        let fixture = fixture();
        let app = router(fixture.state.clone());
        let director = (&SOURCE_VM_HEADER, "annict-factory");

        let show = send(
            app.clone(),
            get_as("/projects/Annict/settings", &[director]),
        )
        .await;
        let save = send(
            app,
            post_form(
                OPERATION_PATH,
                &[director, (&auth::SEC_FETCH_SITE_HEADER, "same-origin")],
                PAUSE,
            ),
        )
        .await;

        assert_eq!(
            show.status(),
            StatusCode::FORBIDDEN,
            "ディレクターが設定の画面を開けた"
        );
        assert_eq!(
            save.status(),
            StatusCode::FORBIDDEN,
            "ディレクターが設定を変えられた"
        );
        assert!(
            saved_settings(&fixture.state).await.is_empty(),
            "ディレクターの要求で設定を保存した"
        );
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

    const AGENTS_PATH: &str = "/projects/Annict/settings/agents";

    /// 作業者をモデルとeffortを指定したClaude Codeに、偶数回目のレビュアーをCodexに変える送信。奇数回目は既定のまま。
    const AGENTS: &str = "implementer_agent=claude&implementer_effort=high&implementer_model=+claude-opus-5-5%5B1m%5D+\
        &reviewer_odd_agent=codex&reviewer_odd_effort=&reviewer_odd_model=\
        &reviewer_even_agent=codex&reviewer_even_effort=max&reviewer_even_model=";

    async fn saved_agents(
        state: &AppState,
    ) -> Vec<(String, String, Option<String>, Option<String>, String)> {
        state
            .database
            .call(|connection| {
                let mut statement = connection.prepare(
                    "SELECT slot, agent, model, effort, updated_at FROM agent_settings ORDER BY slot",
                )?;
                statement
                    .query_map([], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
                    })?
                    .collect()
            })
            .await
            .expect("保存した担当ごとのエージェントを読めなかった")
    }

    /// htmlの中で、nameの項目の要素 (`<select ...>...</select>` か `<input ... />`) を取り出す。
    fn control<'a>(html: &'a str, name: &str) -> &'a str {
        let marker = format!(r#"name="{name}""#);
        let at = html.find(&marker).expect("項目が画面に無い");
        let start = html[..at].rfind('<').expect("項目の要素の始まりが無い");
        let end = if html[start..].starts_with("<select") {
            html[start..].find("</select>").expect("選択の終わりが無い") + "</select>".len()
        } else {
            html[start..].find("/>").expect("入力欄の終わりが無い") + "/>".len()
        };
        &html[start..start + end]
    }

    /// 選択の要素の中で、選ばれている選択肢の値。
    fn selected(select: &str) -> &str {
        let at = select.find(" selected").expect("選ばれている選択肢が無い");
        let option = &select[..at];
        let value = option.rfind(r#"value=""#).expect("選択肢の値が無い") + r#"value=""#.len();
        let rest = &option[value..];
        &rest[..rest.find('"').expect("選択肢の値の終わりが無い")]
    }

    #[tokio::test]
    async fn 担当ごとのエージェントを保存していなければ既定を選んだカードを出す() {
        let html = html(
            send(
                router(fixture().state),
                get_as("/projects/Annict/settings", &[owner()]),
            )
            .await,
        )
        .await;

        for part in [
            "<h2>担当ごとのエージェント</h2>",
            "モデルとeffortを空にすると、各CLIの既定値で動きます。変えた設定は、次に起動するセッションから使います。",
            r#"action="/projects/Annict/settings/agents""#,
            r#"<h3 id="implementer-heading">作業者</h3>"#,
            r#"aria-labelledby="implementer-heading""#,
            r#"<h3 id="reviewer_odd-heading">レビュアー (奇数回目)</h3>"#,
            r#"<h3 id="reviewer_even-heading">レビュアー (偶数回目)</h3>"#,
            "1回目・3回目のレビューと、その指摘の修正",
            "2つの枠を同じにすると、修正したモデルが同じ変更を再レビューします。",
            r#"aria-label="作業者のエージェント""#,
            r#"aria-label="レビュアー (奇数回目)のeffort""#,
            r#"aria-label="レビュアー (偶数回目)のモデル""#,
            r#"placeholder="既定のモデル""#,
            ">既定のeffort<",
            r#"type="submit">設定を保存する</button>"#,
        ] {
            assert!(html.contains(part), "設定の画面に {part} が無い");
        }
        let agents: Vec<&str> = ["implementer", "reviewer_odd", "reviewer_even"]
            .into_iter()
            .map(|slot| selected(control(&html, &format!("{slot}_agent"))))
            .collect();
        assert_eq!(
            agents,
            ["claude", "codex", "claude"],
            "既定のエージェントを選んでいない"
        );
        assert_eq!(
            selected(control(&html, "implementer_effort")),
            "",
            "既定のeffortを選んでいない"
        );
        let model = control(&html, "implementer_model");
        for part in [
            r#"value="""#,
            r#"autocapitalize="off""#,
            r#"autocorrect="off""#,
            r#"spellcheck="false""#,
            "font-mono",
            "text-base",
        ] {
            assert!(
                model.contains(part),
                "モデルの入力欄に {part} が無い: {model}"
            );
        }
        assert!(
            !model.contains("aria-invalid"),
            "エラーの無い入力欄を誤りとした"
        );
        assert!(
            !html.contains("に保存</p>"),
            "保存していないのに保存した時刻を出した"
        );
    }

    #[tokio::test]
    async fn 担当ごとのエージェントを保存して設定の画面へ戻し_保存した値と時刻を出す() {
        let fixture = fixture();
        let app = router(fixture.state.clone());

        let response = send(app.clone(), from_settings(AGENTS_PATH, AGENTS)).await;

        assert_eq!(
            response.status(),
            StatusCode::SEE_OTHER,
            "303で戻さなかった"
        );
        assert_eq!(
            response.headers().get(header::LOCATION),
            Some(&HeaderValue::from_static("/projects/Annict/settings")),
            "設定の画面へ戻さなかった"
        );
        let saved: Vec<_> = saved_agents(&fixture.state)
            .await
            .into_iter()
            .map(|(slot, agent, model, effort, _)| (slot, agent, model, effort))
            .collect();
        assert_eq!(
            saved,
            [
                (
                    "implementer".to_owned(),
                    "claude".to_owned(),
                    Some("claude-opus-5-5[1m]".to_owned()),
                    Some("high".to_owned())
                ),
                (
                    "reviewer_even".to_owned(),
                    "codex".to_owned(),
                    None,
                    Some("max".to_owned())
                ),
            ],
            "変えた枠だけを、前後の空白を除いて保存しなかった"
        );

        let html = html(send(app, get_as("/projects/Annict/settings", &[owner()])).await).await;
        assert_eq!(
            selected(control(&html, "implementer_effort")),
            "high",
            "保存したeffortを選んでいない"
        );
        assert_eq!(
            selected(control(&html, "reviewer_even_agent")),
            "codex",
            "保存したエージェントを選んでいない"
        );
        assert!(
            control(&html, "implementer_model").contains(r#"value="claude-opus-5-5[1m]""#),
            "保存したモデルを出していない"
        );
        assert!(
            html.contains("1分以内</time>に保存</p>"),
            "最後に保存した時刻を出していない"
        );
    }

    #[tokio::test]
    async fn 同じ値の枠は書き込まず_保存した時刻を変えない() {
        let fixture = fixture();
        let app = router(fixture.state.clone());
        send(app.clone(), from_settings(AGENTS_PATH, AGENTS)).await;
        let first = saved_agents(&fixture.state).await;

        let response = send(app, from_settings(AGENTS_PATH, AGENTS)).await;

        assert_eq!(
            response.status(),
            StatusCode::SEE_OTHER,
            "303で戻さなかった"
        );
        assert_eq!(
            saved_agents(&fixture.state).await,
            first,
            "同じ値の送り直しで行を書き換えた"
        );
    }

    #[tokio::test]
    async fn 規則に合わないモデルは保存せず_入力を残して422で描き直し_エラーを入力欄に結び付ける() {
        let fixture = fixture();
        let app = router(fixture.state.clone());
        let body = AGENTS.replace("reviewer_odd_model=", "reviewer_odd_model=+--model%3Cb%3E+");

        let response = send(app, from_settings(AGENTS_PATH, &body)).await;

        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "422にしなかった"
        );
        let html = html(response).await;
        let model = control(&html, "reviewer_odd_model");
        for part in [
            r#"value=" --model&#60;b&#62; ""#,
            r#"aria-invalid="true""#,
            r#"aria-describedby="reviewer_odd-model-error""#,
            "autofocus",
        ] {
            assert!(
                model.contains(part),
                "誤ったモデルの入力欄に {part} が無い: {model}"
            );
        }
        assert!(
            html.contains(r#"<p id="reviewer_odd-model-error" role="alert">モデルは英数字で始め"#),
            "エラーを入力欄の下に出していない"
        );
        assert!(
            control(&html, "implementer_model").contains(r#"value=" claude-opus-5-5[1m] ""#),
            "ほかの枠の入力を残していない"
        );
        assert_eq!(
            selected(control(&html, "reviewer_even_effort")),
            "max",
            "ほかの枠の選択を残していない"
        );
        assert!(
            !control(&html, "implementer_model").contains("aria-invalid"),
            "正しい入力欄を誤りとした"
        );
        assert!(
            html.contains(r#"action="/projects/Annict/settings/operation""#),
            "描き直した画面に稼働のカードが無い"
        );
        assert!(
            saved_agents(&fixture.state).await.is_empty(),
            "規則に合わないモデルがあるのにほかの枠を保存した"
        );
    }

    #[tokio::test]
    async fn 二つ目の枠の書き込みに失敗したら先に更新した枠の値と保存時刻を戻す() {
        let fixture = fixture();
        let app = router(fixture.state.clone());
        let initial = send(app.clone(), from_settings(AGENTS_PATH, AGENTS)).await;
        assert_eq!(
            initial.status(),
            StatusCode::SEE_OTHER,
            "最初の設定を保存できなかった"
        );
        let before = saved_agents(&fixture.state).await;
        fixture
            .state
            .database
            .call(|connection| {
                connection.execute_batch(
                    "CREATE TRIGGER reject_reviewer_odd BEFORE INSERT ON agent_settings
                     WHEN NEW.slot = 'reviewer_odd'
                     BEGIN
                         SELECT RAISE(ABORT, 'reviewer_odd');
                     END;",
                )
            })
            .await
            .expect("2枠目の書き込みを失敗させる準備ができなかった");
        let body = AGENTS
            .replace("implementer_agent=claude", "implementer_agent=codex")
            .replace("reviewer_odd_agent=codex", "reviewer_odd_agent=claude")
            .replace("reviewer_even_effort=max", "reviewer_even_effort=high");

        let response = send(app, from_settings(AGENTS_PATH, &body)).await;

        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "2枠目の書き込みに失敗しても500を返さなかった"
        );
        assert_eq!(
            saved_agents(&fixture.state).await,
            before,
            "途中の書き込み失敗で保存値か保存時刻が変わった"
        );
    }

    #[tokio::test]
    async fn フォームからは送れない値は400の画面で拒否して保存しない() {
        let fixture = fixture();
        let app = router(fixture.state.clone());

        for body in [
            String::new(),
            AGENTS.replace("implementer_agent=claude", "implementer_agent=gemini"),
            AGENTS.replace("reviewer_even_effort=max", "reviewer_even_effort=minimal"),
            AGENTS.replace("&reviewer_odd_model=", ""),
            format!("{AGENTS}&implementer_agent=codex"),
        ] {
            let response = send(app.clone(), from_settings(AGENTS_PATH, &body)).await;

            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{body:?} を400にしなかった"
            );
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE),
                Some(&HeaderValue::from_static("text/html; charset=utf-8")),
                "{body:?} の400を画面で返さなかった"
            );
        }
        assert!(
            saved_agents(&fixture.state).await.is_empty(),
            "受け付けられない値で設定を保存した"
        );
    }

    #[tokio::test]
    async fn 担当ごとのエージェントの保存は_設定に無いプロジェクトとほかのサイトとディレクターの要求を拒む()
     {
        let fixture = fixture();
        let app = router(fixture.state.clone());

        let unknown = send(
            app.clone(),
            from_settings("/projects/Unknown/settings/agents", AGENTS),
        )
        .await;
        let cross = send(
            app.clone(),
            post_form(
                AGENTS_PATH,
                &[owner(), (&auth::SEC_FETCH_SITE_HEADER, "cross-site")],
                AGENTS,
            ),
        )
        .await;
        let director = send(
            app,
            post_form(
                AGENTS_PATH,
                &[
                    (&SOURCE_VM_HEADER, "annict-factory"),
                    (&auth::SEC_FETCH_SITE_HEADER, "same-origin"),
                ],
                AGENTS,
            ),
        )
        .await;

        assert_eq!(
            unknown.status(),
            StatusCode::NOT_FOUND,
            "設定に無いプロジェクトを404にしなかった"
        );
        assert_eq!(
            cross.status(),
            StatusCode::FORBIDDEN,
            "ほかのサイトからの要求を403にしなかった"
        );
        assert_eq!(
            director.status(),
            StatusCode::FORBIDDEN,
            "ディレクターが担当ごとのエージェントを変えられた"
        );
        assert!(
            saved_agents(&fixture.state).await.is_empty(),
            "拒否した要求で設定を保存した"
        );
    }

    #[tokio::test]
    async fn 担当ごとのエージェントを読み書きできなければ500の画面を返す() {
        let fixture = fixture();
        fixture
            .state
            .database
            .call(|connection| connection.execute_batch("DROP TABLE agent_settings"))
            .await
            .expect("担当ごとのエージェントのテーブルを消せなかった");
        let app = router(fixture.state.clone());

        let show = send(app.clone(), get_as("/projects/Annict/settings", &[owner()])).await;
        let save = send(app, from_settings(AGENTS_PATH, AGENTS)).await;

        assert_eq!(
            show.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "読めないときに500にしなかった"
        );
        assert_eq!(
            save.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "保存できないときに500にしなかった"
        );
    }

    #[tokio::test]
    async fn 画面で保存した担当ごとのエージェントをディレクターが読む() {
        let fixture = fixture();
        let app = router(fixture.state.clone());
        send(app.clone(), from_settings(AGENTS_PATH, AGENTS)).await;

        let response = send(app, read_settings(&[(&SOURCE_VM_HEADER, "annict-factory")])).await;

        assert_eq!(
            json(response).await["agents"],
            serde_json::json!({
                "implementer": {"agent": "claude", "model": "claude-opus-5-5[1m]", "effort": "high"},
                "reviewer_even": {"agent": "codex", "effort": "max"}
            }),
            "画面で保存した設定をディレクターに返さなかった"
        );
    }

    fn read_settings(headers: &[(&HeaderName, &str)]) -> Request<Body> {
        get_as(api::SETTINGS_PATH, headers)
    }

    async fn json(response: Response<Body>) -> serde_json::Value {
        let bytes = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("本文を読めなかった");
        serde_json::from_slice(&bytes).expect("本文がJSONではない")
    }

    #[tokio::test]
    async fn ディレクターは設定を保存していなければ稼働を読み_停止に切り替えると停止を読む() {
        let fixture = fixture();
        let director = (&SOURCE_VM_HEADER, "annict-factory");

        let running = send(router(fixture.state.clone()), read_settings(&[director])).await;
        assert_eq!(running.status(), StatusCode::OK, "200を返さなかった");
        assert_eq!(
            running.headers().get(header::CONTENT_TYPE),
            Some(&HeaderValue::from_static("application/json")),
            "JSONで返さなかった"
        );
        assert_api_headers(&running);
        assert_eq!(
            json(running).await,
            serde_json::json!({"paused": false, "agents": {}}),
            "保存していない設定を稼働として返さなかった"
        );

        send(
            router(fixture.state.clone()),
            from_settings(OPERATION_PATH, PAUSE),
        )
        .await;
        let paused = send(router(fixture.state.clone()), read_settings(&[director])).await;

        assert_eq!(
            json(paused).await,
            serde_json::json!({"paused": true, "agents": {}}),
            "保存した停止を返さなかった"
        );
    }

    #[tokio::test]
    async fn ディレクターは自分のプロジェクトの設定だけを読む() {
        let mut fixture = fixture();
        Arc::get_mut(&mut fixture.state.config)
            .expect("テスト用の設定を変更できなかった")
            .directors
            .push(crate::config::Director {
                vm: "mewst-factory".to_owned(),
                project: "Mewst".to_owned(),
            });
        fixture
            .state
            .database
            .call(|connection| {
                connection.execute(
                    "INSERT INTO project_settings (project, paused, updated_at) VALUES
                     ('Annict', 0, '2026-11-19T08:00:00Z'),
                     ('Mewst', 1, '2026-11-19T08:00:00Z')",
                    [],
                )
            })
            .await
            .expect("プロジェクトの設定を保存できなかった");

        let app = router(fixture.state);
        for (vm, paused) in [("annict-factory", false), ("mewst-factory", true)] {
            let response = send(app.clone(), read_settings(&[(&SOURCE_VM_HEADER, vm)])).await;

            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{vm} に設定を返さなかった"
            );
            assert_eq!(
                json(response).await,
                serde_json::json!({"paused": paused, "agents": {}}),
                "{vm} に自分のプロジェクトの設定を返さなかった"
            );
        }
    }

    #[tokio::test]
    async fn ディレクターは保存した既知の枠のエージェントを読む() {
        let mut fixture = fixture();
        Arc::get_mut(&mut fixture.state.config)
            .expect("テスト用の設定を変更できなかった")
            .directors
            .push(crate::config::Director {
                vm: "mewst-factory".to_owned(),
                project: "Mewst".to_owned(),
            });
        fixture
            .state
            .database
            .call(|connection| {
                connection.execute_batch(
                    "INSERT INTO agent_settings (project, slot, agent, model, effort, updated_at) VALUES
                     ('Annict', 'implementer', 'claude', 'claude-opus-5-5[1m]', 'high', '2026-11-20T08:00:00Z'),
                     ('Annict', 'reviewer_odd', 'codex', NULL, NULL, '2026-11-20T08:00:00Z'),
                     ('Annict', 'reviewer_even', 'claude', '-c', NULL, '2026-11-20T08:00:00Z'),
                     ('Annict', 'watcher', 'claude', NULL, NULL, '2026-11-20T08:00:00Z'),
                     ('Mewst', 'reviewer_even', 'codex', NULL, 'max', '2026-11-20T08:00:00Z');",
                )
            })
            .await
            .expect("担当ごとのエージェントの設定を保存できなかった");

        let app = router(fixture.state);
        let annict = send(
            app.clone(),
            read_settings(&[(&SOURCE_VM_HEADER, "annict-factory")]),
        )
        .await;
        let mewst = send(app, read_settings(&[(&SOURCE_VM_HEADER, "mewst-factory")])).await;

        assert_eq!(annict.status(), StatusCode::OK, "200を返さなかった");
        assert_api_headers(&annict);
        assert_eq!(
            json(annict).await,
            serde_json::json!({
                "paused": false,
                "agents": {
                    "implementer": {"agent": "claude", "model": "claude-opus-5-5[1m]", "effort": "high"},
                    "reviewer_odd": {"agent": "codex"},
                    "reviewer_even": {"agent": "claude", "model": "-c"}
                }
            }),
            "既知の枠の保存値を、指定しない値のキーを省いて返さなかった"
        );
        assert_eq!(
            json(mewst).await,
            serde_json::json!({
                "paused": false,
                "agents": {"reviewer_even": {"agent": "codex", "effort": "max"}}
            }),
            "ほかのプロジェクトの担当ごとのエージェントを返した"
        );
    }

    #[tokio::test]
    async fn 担当ごとのエージェントの設定を読めなければ500のproblem_detailsを返す() {
        let fixture = fixture();
        fixture
            .state
            .database
            .call(|connection| connection.execute_batch("DROP TABLE agent_settings"))
            .await
            .expect("担当ごとのエージェントのテーブルを消せなかった");

        let response = send(
            router(fixture.state.clone()),
            read_settings(&[(&SOURCE_VM_HEADER, "annict-factory")]),
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "読めないときに500にしなかった"
        );
        assert_eq!(
            problem(response).await["type"],
            "/api/problems/internal-error",
            "500の問題の種類が違う"
        );
    }

    #[tokio::test]
    async fn 設定のapiは主の分からない要求と人間をproblem_detailsで拒否する() {
        let app = router(fixture().state);

        let anonymous = send(app.clone(), read_settings(&[])).await;
        let human = send(app, read_settings(&[owner()])).await;

        assert_eq!(
            anonymous.status(),
            StatusCode::UNAUTHORIZED,
            "主の分からない要求に401を返さなかった"
        );
        assert_eq!(
            human.status(),
            StatusCode::FORBIDDEN,
            "人間が設定のapiを呼べた"
        );
        assert_api_headers(&human);
        assert_eq!(
            problem(human).await["type"],
            "/api/problems/forbidden",
            "403の問題の種類が違う"
        );
    }

    #[tokio::test]
    async fn ディレクターは設定のapiで設定を変えられない() {
        let fixture = fixture();

        for method in [Method::PUT, Method::POST, Method::PATCH, Method::DELETE] {
            let request = Request::builder()
                .method(method.clone())
                .uri(api::SETTINGS_PATH)
                .header(&SOURCE_VM_HEADER, "annict-factory")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"paused": true}"#))
                .expect("要求を作れなかった");

            let response = send(router(fixture.state.clone()), request).await;

            assert_eq!(
                response.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} で405を返さなかった"
            );
            assert_eq!(
                problem(response).await["type"],
                "/api/problems/method-not-allowed",
                "{method} の問題の種類が違う"
            );
        }
        assert!(
            saved_settings(&fixture.state).await.is_empty(),
            "ディレクターの要求で設定を保存した"
        );
    }

    #[tokio::test]
    async fn 設定を読めなければ500のproblem_detailsを返す() {
        let fixture = fixture();
        fixture
            .state
            .database
            .call(|connection| connection.execute_batch("DROP TABLE project_settings"))
            .await
            .expect("設定のテーブルを消せなかった");

        let response = send(
            router(fixture.state.clone()),
            read_settings(&[(&SOURCE_VM_HEADER, "annict-factory")]),
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "読めないときに500にしなかった"
        );
        assert_api_headers(&response);
        assert_eq!(
            problem(response).await["type"],
            "/api/problems/internal-error",
            "500の問題の種類が違う"
        );
    }

    /// 停止を反映して待っているディレクターの報告。
    const PAUSED_REPORT: &str = r#"{
        "sent_at": "2026-11-19T08:00:00Z",
        "korylusfactory": {"version": "0.1.0", "commit": "abc1234"},
        "running": false,
        "paused": true,
        "progress": [],
        "upcoming": [{"title": "メッセージ 作業計画書"}]
    }"#;

    #[tokio::test]
    async fn 停止を反映したプロジェクトは一覧に停止中として出す() {
        let fixture = fixture();
        send(
            router(fixture.state.clone()),
            put_as(&director(), PAUSED_REPORT),
        )
        .await;

        let response = send(router(fixture.state), get_as("/", &[owner()])).await;

        let html = html(response).await;
        for part in [
            "data-variant=\"secondary\"\n  >停止中</span",
            r#"<p class="wrap-anywhere">メッセージ 作業計画書</p>"#,
            "次に進める作業計画書 ・ 停止しています",
            "人間待ちと不通のプロジェクトはありません。",
        ] {
            assert!(html.contains(part), "一覧に {part} が無い");
        }
    }

    #[tokio::test]
    async fn プロジェクトの画面に稼働の設定と報告の食い違いを添える() {
        let fixture = fixture();
        let app = router(fixture.state.clone());
        send(app.clone(), put_as(&director(), REPORT)).await;
        send(app.clone(), from_settings(OPERATION_PATH, PAUSE)).await;

        let stopping = html(send(app.clone(), get_as("/projects/Annict", &[owner()])).await).await;
        send(app.clone(), put_as(&director(), PAUSED_REPORT)).await;
        let stopped = html(send(app.clone(), get_as("/projects/Annict", &[owner()])).await).await;
        send(app.clone(), from_settings(OPERATION_PATH, RESUME)).await;
        let resuming = html(send(app, get_as("/projects/Annict", &[owner()])).await).await;

        assert!(
            stopping
                .contains("停止に切り替えました。実行中のセッションが終わったところで停止します"),
            "停止をまだ反映していないことを添えなかった"
        );
        assert!(
            stopped.contains("data-variant=\"secondary\"\n  >停止中</span")
                && !stopped.contains("切り替えました"),
            "停止を反映したら停止中のBadgeだけを出すはず"
        );
        assert!(
            resuming.contains("稼働に切り替えました。ディレクターが設定を読むと再開します"),
            "稼働をまだ反映していないことを添えなかった"
        );
    }
}
