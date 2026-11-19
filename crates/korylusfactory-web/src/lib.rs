//! Korylus Factoryの管制のウェブアプリ。
//!
//! 各プロジェクトのディレクターが送る状態を受け取り、人間に一覧で見せる。
//! exe.devの認証付きのプロキシの内側で動き、プロキシが付けるヘッダーで要求の主を見分ける ([`auth`])。

pub mod api;
pub mod app;
pub mod assets;
pub mod auth;
pub mod config;
pub mod db;
pub mod events;
pub mod page;
pub mod projects;

/// エラーを、原因をたどって1行にまとめる。
pub fn describe(err: &dyn std::error::Error) -> String {
    let mut message = err.to_string();
    let mut source = err.source();
    while let Some(err) = source {
        message.push_str(": ");
        message.push_str(&err.to_string());
        source = err.source();
    }
    message
}
