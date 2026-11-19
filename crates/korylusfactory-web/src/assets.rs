//! 実行ファイルに埋め込んだ静的ファイル (CSS・JavaScript) の配信。
//!
//! ファイルは `static/` から埋め込み、内容のハッシュを入れた名前 (`app.0123456789abcdef.css`) で配信する。
//! 名前が内容ごとに変わるため、ブラウザには変わらないものとして長くキャッシュさせる。

use std::hash::{DefaultHasher, Hasher as _};
use std::sync::LazyLock;

use axum::extract::Path;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};

use crate::page;

/// 静的ファイルを配信するパスの接頭辞。
pub const PREFIX: &str = "/static/";

static STYLESHEET: LazyLock<Asset> = LazyLock::new(|| {
    Asset::new(
        "app.css",
        include_bytes!("../static/app.css"),
        "text/css; charset=utf-8",
    )
});
static HTMX: LazyLock<Asset> = LazyLock::new(|| {
    Asset::new(
        "htmx.min.js",
        include_bytes!("../static/htmx.min.js"),
        "text/javascript; charset=utf-8",
    )
});
static SSE: LazyLock<Asset> = LazyLock::new(|| {
    Asset::new(
        "hx-sse.min.js",
        include_bytes!("../static/hx-sse.min.js"),
        "text/javascript; charset=utf-8",
    )
});
static THEME: LazyLock<Asset> = LazyLock::new(|| {
    Asset::new(
        "theme.js",
        include_bytes!("../static/theme.js"),
        "text/javascript; charset=utf-8",
    )
});

/// ハッシュを入れた名前で配信するファイル。
#[derive(Debug)]
struct Asset {
    path: String,
    body: &'static [u8],
    content_type: &'static str,
}

impl Asset {
    fn new(name: &str, body: &'static [u8], content_type: &'static str) -> Self {
        Self {
            path: format!("{PREFIX}{}", hashed_name(name, body)),
            body,
            content_type,
        }
    }
}

/// 共通のスタイルシートの配信パス。テンプレートから呼ぶ。
pub(crate) fn stylesheet_path() -> &'static str {
    &STYLESHEET.path
}

/// htmxの配信パス。テンプレートから呼ぶ。
pub(crate) fn htmx_path() -> &'static str {
    &HTMX.path
}

/// htmxでSSEを受け取る拡張の配信パス。テンプレートから呼ぶ。
pub(crate) fn sse_path() -> &'static str {
    &SSE.path
}

/// 配色を切り替えるスクリプトの配信パス。テンプレートから呼ぶ。
pub(crate) fn theme_path() -> &'static str {
    &THEME.path
}

/// 内容のハッシュを、拡張子の前に入れた名前を返す (`app.css` → `app.0123456789abcdef.css`)。
fn hashed_name(name: &str, body: &[u8]) -> String {
    // 名前はこの実行ファイルの中だけで使うため、版によってハッシュの値が変わる標準のハッシュで足りる。
    let mut hasher = DefaultHasher::new();
    hasher.write(body);
    let hash = format!("{:016x}", hasher.finish());
    match name.rsplit_once('.') {
        Some((stem, extension)) => format!("{stem}.{hash}.{extension}"),
        None => format!("{name}.{hash}"),
    }
}

/// ハッシュを入れた名前のファイルを返す。知らない名前には404の画面を返す。
pub async fn serve(Path(file): Path<String>) -> Response {
    let asset = [&*STYLESHEET, &*HTMX, &*SSE, &*THEME]
        .into_iter()
        .find(|asset| asset.path.strip_prefix(PREFIX) == Some(file.as_str()));
    let Some(asset) = asset else {
        return page::not_found();
    };
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static(asset.content_type),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            ),
        ],
        asset.body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 配信するパスは内容のハッシュを拡張子の前に入れる() {
        for (path, prefix, suffix) in [
            (stylesheet_path(), "/static/app.", ".css"),
            (htmx_path(), "/static/htmx.min.", ".js"),
            (sse_path(), "/static/hx-sse.min.", ".js"),
            (theme_path(), "/static/theme.", ".js"),
        ] {
            let hash = path
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix(suffix));

            assert!(
                hash.is_some_and(
                    |hash| hash.len() == 16 && hash.chars().all(|c| c.is_ascii_hexdigit())
                ),
                "配信パスの形が違う: {path}"
            );
        }
    }

    #[test]
    fn 中身が変わるとハッシュを入れた名前も変わる() {
        assert_ne!(
            hashed_name("app.css", b"a"),
            hashed_name("app.css", b"b"),
            "中身が違うのに同じ名前になった"
        );
        assert_eq!(
            hashed_name("app.css", b"a"),
            hashed_name("app.css", b"a"),
            "同じ中身で名前が変わった"
        );
    }

    #[tokio::test]
    async fn ハッシュを入れた名前で中身と長いキャッシュの指定を返す() {
        let file = theme_path()
            .strip_prefix(PREFIX)
            .expect("接頭辞が付いていない")
            .to_owned();

        let response = serve(Path(file)).await;

        assert_eq!(response.status(), 200, "ファイルを返さなかった");
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE),
            Some(&HeaderValue::from_static("text/javascript; charset=utf-8")),
            "Content-Typeが違う"
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL),
            Some(&HeaderValue::from_static(
                "public, max-age=31536000, immutable"
            )),
            "長いキャッシュの指定が無い"
        );
    }

    #[tokio::test]
    async fn ハッシュの無い名前や古いハッシュの名前は404にする() {
        for file in ["theme.js", "theme.0000000000000000.js"] {
            let response = serve(Path(file.to_owned())).await;

            assert_eq!(response.status(), 404, "{file} を配信した");
        }
    }
}
