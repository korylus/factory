//! 画面 (HTMLの応答) の描画と、HTMLの応答だけに付ける保護のヘッダー。

use std::any::{Any, type_name};

use askama::Template;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use jiff::Timestamp;
use korylusfactory_protocol::{Agent, Effort};

use crate::agents::{self, AgentsForm, SlotInput};
use crate::projects::{self, Detail, Item, Part, SessionPart, State};
use crate::settings::{self, OPERATION_FIELD, Operation};

/// HTMLの応答に付けるContent-Security-Policy。
///
/// スクリプトとスタイルは自分のオリジンのファイルだけを許し、インラインのものは許さない。
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// HTMLの応答に付けるCache-Control。状態が変わるたびに中身が変わるため、毎回確かめさせる。
pub const CACHE_CONTROL: &str = "private, no-cache";

/// プロジェクトの一覧の画面。
#[derive(Debug, Template)]
#[template(path = "home.html")]
pub struct ProjectList {
    summary: String,
    projects: Vec<Item>,
}

impl ProjectList {
    /// 並べ終えた一覧から画面を作る。
    pub fn new(projects: Vec<Item>) -> Self {
        Self {
            summary: projects::summary(&projects),
            projects,
        }
    }
}

/// プロジェクトの画面。
#[derive(Debug, Template)]
#[template(path = "project.html")]
pub struct ProjectPage {
    detail: Detail,
    path: String,
    settings_path: String,
}

impl ProjectPage {
    /// プロジェクトの画面の中身から画面を作る。
    pub fn new(detail: Detail) -> Self {
        Self {
            path: projects::path(&detail.name),
            settings_path: settings::path(&detail.name),
            detail,
        }
    }
}

/// プロジェクトの設定の画面。
#[derive(Debug, Template)]
#[template(path = "settings.html")]
pub struct SettingsPage {
    name: String,
    project_path: String,
    operation_path: String,
    running: bool,
    agents_path: String,
    slots: Vec<SlotInput>,
    /// 最後に保存した時刻 (`<time datetime>` に入れる値と、相対の表現)。
    saved_at: Option<(String, String)>,
}

impl SettingsPage {
    /// プロジェクトの名前と、稼働の設定と、担当ごとのエージェントのフォームの値から画面を作る。nowは今の時刻。
    pub fn new(name: &str, operation: Operation, agents: AgentsForm, now: Timestamp) -> Self {
        Self {
            name: name.to_owned(),
            project_path: projects::path(name),
            operation_path: settings::operation_path(name),
            running: !operation.is_paused(),
            agents_path: agents::path(name),
            slots: agents.slots,
            saved_at: agents
                .saved_at
                .map(|time| (projects::datetime(time), projects::ago(time, now))),
        }
    }

    /// inputが最初にエラーのある枠かを返す。読み込んだときに、その入力欄へフォーカスを移す。
    fn is_first_error(&self, input: &SlotInput) -> bool {
        self.slots
            .iter()
            .find(|slot| slot.error.is_some())
            .is_some_and(|slot| slot.slot == input.slot)
    }
}

/// 画面の要求が失敗したときに、見出しと案内を共通の枠で描く。
#[derive(Debug, Template)]
#[template(path = "error.html")]
struct ErrorPage {
    heading: &'static str,
    message: &'static str,
}

/// 画面を描き、HTMLの応答にする。
///
/// 描けなかったときは500の画面を返す。
pub fn render(status: StatusCode, template: &impl Template) -> Response {
    match render_template(template) {
        Ok(body) => html(status, body),
        Err(_) => internal_error(),
    }
}

/// 404の画面を返す。
pub fn not_found() -> Response {
    render(
        StatusCode::NOT_FOUND,
        &ErrorPage {
            heading: "ページが見つかりません",
            message: "URLが正しいかを確かめてください。",
        },
    )
}

/// 送られた値を受け付けられないときの400の画面を返す。
pub fn bad_request() -> Response {
    render(
        StatusCode::BAD_REQUEST,
        &ErrorPage {
            heading: "送られた内容を受け付けられません",
            message: "画面を開き直してから、もう一度操作してください。",
        },
    )
}

/// 操作を許可しないときの403の画面を返す。
pub fn forbidden() -> Response {
    render(
        StatusCode::FORBIDDEN,
        &ErrorPage {
            heading: "この操作は許可されていません",
            message: "Korylus Factoryの画面を開き直してから、もう一度操作してください。",
        },
    )
}

/// 500の画面を返す。内部のエラーの中身は出さない。
pub fn internal_error() -> Response {
    let template = ErrorPage {
        heading: "エラーが起きました",
        message: "時間をおいて、開き直してください。",
    };
    // 500の画面を描けないときは、テンプレートを使わない文面で返す。
    let body = render_template(&template).unwrap_or_else(|_| {
        "<!doctype html><html lang=\"ja\"><title>エラーが起きました</title><p>エラーが起きました。</p></html>"
            .to_owned()
    });
    html(StatusCode::INTERNAL_SERVER_ERROR, body)
}

/// 処理の途中でパニックしたときに返す応答。パニックの中身は出さない。
pub fn panic_response(_: Box<dyn Any + Send + 'static>) -> Response {
    internal_error()
}

fn render_template<T: Template>(template: &T) -> askama::Result<String> {
    template.render().inspect_err(|error| {
        // カスタムエラーの文面には表示用のデータが含まれることがあるため、失敗の種類だけを残す。
        let reason = match error {
            askama::Error::Fmt => "書式化に失敗しました",
            askama::Error::ValueMissing => "必要な値がありません",
            askama::Error::ValueType => "値の型が違います",
            askama::Error::Custom(_) => "テンプレート内の処理が失敗しました",
            _ => "テンプレートエンジンで処理に失敗しました",
        };
        log::error!("画面 {} を描けません: {reason}", type_name::<T>());
    })
}

fn html(status: StatusCode, body: String) -> Response {
    (
        status,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(CONTENT_SECURITY_POLICY),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static(CACHE_CONTROL),
            ),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::body;
    use korylusfactory_protocol::{InvalidAgentSetting, Slot};

    use super::*;
    use crate::assets;

    async fn body_text(response: Response) -> String {
        let bytes = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("本文を読めなかった");
        String::from_utf8(bytes.to_vec()).expect("本文がUTF-8ではない")
    }

    #[tokio::test]
    async fn 画面は共通の枠を持つ() {
        let html = body_text(render(StatusCode::OK, &ProjectList::new(Vec::new()))).await;

        for part in [
            "<!doctype html>",
            r#"<html lang="ja">"#,
            r#"<meta charset="utf-8" />"#,
            r#"<meta name="viewport" content="width=device-width, initial-scale=1" />"#,
            "<title>Korylus Factory</title>",
            r#"<meta name="color-scheme" content="light dark" />"#,
            r#"<meta name="robots" content="noindex" />"#,
            r##"href="#main""##,
            "<header",
            r#"<main id="main""#,
        ] {
            assert!(html.contains(part), "画面に {part} が無い");
        }
    }

    #[tokio::test]
    async fn 画面は埋め込んだcssとjavascriptを参照する() {
        let html = body_text(render(StatusCode::OK, &ProjectList::new(Vec::new()))).await;

        for path in [
            assets::stylesheet_path(),
            assets::htmx_path(),
            assets::sse_path(),
            assets::theme_path(),
            assets::project_status_path(),
        ] {
            assert!(
                html.contains(&format!(r#""{path}""#)),
                "画面が {path} を参照していない"
            );
        }
        let color_scheme = html
            .find(r#"<meta name="color-scheme" content="light dark""#)
            .expect("配色の宣言が無い");
        let theme = html.find(assets::theme_path()).expect("theme.js が無い");
        let stylesheet = html
            .find(assets::stylesheet_path())
            .expect("app.css が無い");
        assert!(
            color_scheme < theme,
            "同期スクリプトを待つ前に配色を宣言していない"
        );
        assert!(
            theme < stylesheet,
            "ダークモードのスクリプトが描画の前に走るよう、CSSより先に置いていない"
        );
    }

    #[tokio::test]
    async fn テンプレートが描画エラーを返したら中身を出さずに500の画面に切り替える() {
        #[derive(Template)]
        #[template(source = "{{ self.value()? }}", ext = "html")]
        struct FailingTemplate {
            error_detail: &'static str,
        }

        impl FailingTemplate {
            fn value(&self) -> askama::Result<&'static str> {
                Err(askama::Error::custom(std::io::Error::other(
                    self.error_detail,
                )))
            }
        }

        let template = FailingTemplate {
            error_detail: "表示してはいけない内容",
        };
        let response = render(StatusCode::OK, &template);

        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "描画の失敗を500にしなかった"
        );
        let html = body_text(response).await;
        assert!(
            html.contains("エラーが起きました"),
            "500の画面を返さなかった"
        );
        assert!(
            !html.contains("表示してはいけない内容"),
            "描画エラーの中身を画面に出した"
        );
    }

    #[tokio::test]
    async fn htmlの応答に保護のヘッダーを付ける() {
        let response = render(StatusCode::OK, &ProjectList::new(Vec::new()));

        assert_eq!(
            response.headers().get(header::CONTENT_TYPE),
            Some(&HeaderValue::from_static("text/html; charset=utf-8")),
            "Content-Typeが違う"
        );
        assert_eq!(
            response.headers().get(header::CONTENT_SECURITY_POLICY),
            Some(&HeaderValue::from_static(CONTENT_SECURITY_POLICY)),
            "CSPが付いていない"
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL),
            Some(&HeaderValue::from_static(CACHE_CONTROL)),
            "Cache-Controlが付いていない"
        );
    }

    #[tokio::test]
    async fn 画面にインラインのスクリプトとスタイルを置かない() {
        for response in [
            render(StatusCode::OK, &ProjectList::new(Vec::new())),
            render(
                StatusCode::OK,
                &ProjectPage::new(projects::detail(
                    "Annict",
                    None,
                    Operation::Running,
                    jiff::Timestamp::now(),
                )),
            ),
            render(
                StatusCode::OK,
                &SettingsPage::new(
                    "Annict",
                    Operation::Paused,
                    AgentsForm {
                        slots: Slot::ALL
                            .into_iter()
                            .map(|slot| SlotInput {
                                slot,
                                agent: "codex".to_owned(),
                                effort: String::new(),
                                model: "-c".to_owned(),
                                error: Some(InvalidAgentSetting::Model),
                            })
                            .collect(),
                        saved_at: Some(jiff::Timestamp::now()),
                    },
                    jiff::Timestamp::now(),
                ),
            ),
            not_found(),
            bad_request(),
            forbidden(),
            internal_error(),
        ] {
            let html = body_text(response).await;

            assert!(
                !html.contains("<script>"),
                "CSPで止まるインラインのスクリプトがある"
            );
            assert!(
                !html.contains("<style"),
                "CSPで止まるインラインのスタイルがある"
            );
            assert!(!html.contains("style=\""), "CSPで止まるstyle属性がある");
        }
    }

    #[tokio::test]
    async fn 見つからないときとエラーのときの画面は正しいステータスと見出しを返す() {
        let not_found = not_found();
        let internal_error = internal_error();

        assert_eq!(
            not_found.status(),
            StatusCode::NOT_FOUND,
            "404にならなかった"
        );
        assert_eq!(
            internal_error.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "500にならなかった"
        );
        assert!(
            body_text(not_found)
                .await
                .contains("<title>ページが見つかりません - Korylus Factory</title>"),
            "404の画面のタイトルが違う"
        );
        assert!(
            body_text(internal_error)
                .await
                .contains("tabindex=\"-1\" autofocus>エラーが起きました</h1>"),
            "500の画面の見出しが違う"
        );
    }
}
