//! 人間が決めるプロジェクトの設定 (稼働か停止か) の読み書きと、設定の画面。
//!
//! 担当ごとのエージェントの読み書きと保存は [`crate::agents`] が持つ。
//!
//! 設定の正本はウェブアプリのDBの `project_settings` で、行が無いプロジェクトは稼働として扱う。

use axum::Form;
use axum::extract::rejection::{FormRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Redirect, Response};
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};

use crate::app::AppState;
use crate::db::{self, Database};
use crate::{agents, page, projects};

/// 稼働か停止かを送るフォームの項目の名前。
pub const OPERATION_FIELD: &str = "operation";

/// 稼働の設定を保存するSQL。プロジェクトの行が無ければ作り、あれば置き換える。
const UPSERT_SETTING: &str =
    "INSERT INTO project_settings (project, paused, updated_at) VALUES (?1, ?2, ?3)
ON CONFLICT (project) DO UPDATE SET paused = excluded.paused, updated_at = excluded.updated_at";

/// プロジェクトを稼働させるか止めるか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// 作業を進める。
    Running,
    /// 実行中のセッションが終わったところで、次の作業を始めずに待つ。
    Paused,
}

impl Operation {
    /// フォームで送られた値から読み取る。稼働か停止のどちらでもなければ `None` を返す。
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "running" => Some(Self::Running),
            "paused" => Some(Self::Paused),
            _ => None,
        }
    }

    /// フォームで送る値。
    pub fn value(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
        }
    }

    /// 停止しているかを返す。
    pub fn is_paused(self) -> bool {
        self == Self::Paused
    }
}

/// プロジェクトの設定の画面のパス (`/projects/<名前>/settings`)。
pub fn path(name: &str) -> String {
    format!("{}/settings", projects::path(name))
}

/// 稼働か停止かを保存するパス (`/projects/<名前>/settings/operation`)。
pub fn operation_path(name: &str) -> String {
    format!("{}/operation", path(name))
}

/// プロジェクトの稼働の設定を読む。行が無ければ稼働とする。
pub async fn load(database: &Database, project: &str) -> Result<Operation, db::Error> {
    let project = project.to_owned();
    let paused = database
        .call(move |connection| {
            connection
                .query_row(
                    "SELECT paused FROM project_settings WHERE project = ?1",
                    [project],
                    |row| row.get::<_, bool>(0),
                )
                .optional()
        })
        .await?;
    Ok(if paused == Some(true) {
        Operation::Paused
    } else {
        Operation::Running
    })
}

/// プロジェクトの稼働の設定を保存する。保存済みの設定と同じなら書き込まない。
pub async fn save(
    database: &Database,
    project: &str,
    operation: Operation,
    updated_at: Timestamp,
) -> Result<(), db::Error> {
    let project = project.to_owned();
    database
        .call(move |connection| {
            save_operation(connection, &project, operation.is_paused(), updated_at)
        })
        .await
}

fn save_operation(
    connection: &mut Connection,
    project: &str,
    paused: bool,
    updated_at: Timestamp,
) -> rusqlite::Result<()> {
    // 比較と書き込みの間に別の接続から更新されないよう、読み取る前に書き込みロックを取る。
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let saved = transaction
        .query_row(
            "SELECT paused FROM project_settings WHERE project = ?1",
            [project],
            |row| row.get::<_, bool>(0),
        )
        .optional()?;
    if saved != Some(paused) {
        transaction.execute(
            UPSERT_SETTING,
            params![project, paused, updated_at.to_string()],
        )?;
    }
    transaction.commit()
}

/// `GET /projects/{project}/settings`: プロジェクトの設定の画面 (稼働と担当ごとのエージェント)。設定に無いプロジェクトには404を返す。
pub async fn show(
    State(state): State<AppState>,
    segment: Result<Path<String>, PathRejection>,
) -> Response {
    let Some(name) = configured(&state, segment) else {
        return page::not_found();
    };
    let loaded = async {
        let operation = load(&state.database, &name).await?;
        let agents = agents::load_form(&state.database, &name).await?;
        Ok::<_, db::Error>((operation, agents))
    };
    match loaded.await {
        Ok((operation, agents)) => page::render(
            StatusCode::OK,
            &page::SettingsPage::new(&name, operation, agents, Timestamp::now()),
        ),
        Err(error) => {
            // 原因のSQLiteのエラー文には保存した値が入ることがあるため、原因をたどらずに記録する。
            log::error!("プロジェクトの設定を読めません: {error}");
            page::internal_error()
        }
    }
}

/// `POST /projects/{project}/settings/operation`: 稼働か停止かを保存し、設定の画面へ戻す。
///
/// スイッチを切ると値を送らないため、フォームは停止の隠し項目のあとに稼働のスイッチを置き、同じ名前の最後の値を使う。
/// 設定に無いプロジェクトには404、稼働か停止のどちらでもない値には400を返す。
pub async fn update_operation(
    State(state): State<AppState>,
    segment: Result<Path<String>, PathRejection>,
    form: Result<Form<Vec<(String, String)>>, FormRejection>,
) -> Response {
    let Some(name) = configured(&state, segment) else {
        return page::not_found();
    };
    let Some(operation) = form.ok().and_then(|Form(fields)| {
        fields
            .iter()
            .rev()
            .find(|(field, _)| field == OPERATION_FIELD)
            .and_then(|(_, value)| Operation::from_value(value))
    }) else {
        return page::bad_request();
    };
    match save(&state.database, &name, operation, Timestamp::now()).await {
        Ok(()) => Redirect::to(&path(&name)).into_response(),
        Err(error) => {
            log::error!("プロジェクトの設定を保存できません: {error}");
            page::internal_error()
        }
    }
}

/// パスの区切りが、設定にあるプロジェクトを指していればその名前を返す。
pub(crate) fn configured(
    state: &AppState,
    segment: Result<Path<String>, PathRejection>,
) -> Option<String> {
    let Ok(Path(segment)) = segment else {
        return None;
    };
    projects::configured(&state.config.directors, &segment).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> Connection {
        let connection = Connection::open_in_memory().expect("DBを開けなかった");
        for sql in db::MIGRATIONS {
            connection
                .execute_batch(sql)
                .expect("マイグレーションを流せなかった");
        }
        connection
    }

    fn rows(connection: &Connection) -> Vec<(String, i64, String)> {
        let mut statement = connection
            .prepare("SELECT project, paused, updated_at FROM project_settings ORDER BY project")
            .expect("設定を読めなかった");
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("設定を読めなかった")
            .collect::<Result<_, _>>()
            .expect("設定の行を読めなかった")
    }

    fn at(text: &str) -> Timestamp {
        text.parse().expect("時刻を読めなかった")
    }

    #[test]
    fn 停止に切り替えると行を作り_稼働に戻すと同じ行を置き換える() {
        let mut connection = connection();

        save_operation(&mut connection, "Annict", true, at("2026-11-19T08:00:00Z"))
            .expect("停止を保存できなかった");
        assert_eq!(
            rows(&connection),
            [("Annict".to_owned(), 1, "2026-11-19T08:00:00Z".to_owned())],
            "停止の行が違う"
        );

        save_operation(&mut connection, "Annict", false, at("2026-11-19T09:00:00Z"))
            .expect("稼働を保存できなかった");
        assert_eq!(
            rows(&connection),
            [("Annict".to_owned(), 0, "2026-11-19T09:00:00Z".to_owned())],
            "稼働に置き換えなかった"
        );
    }

    #[test]
    fn 同じ値を送り直しても保存した時刻を変えない() {
        let mut connection = connection();
        save_operation(&mut connection, "Annict", true, at("2026-11-19T08:00:00Z"))
            .expect("停止を保存できなかった");

        save_operation(&mut connection, "Annict", true, at("2026-11-19T09:00:00Z"))
            .expect("停止を送り直せなかった");

        assert_eq!(
            rows(&connection),
            [("Annict".to_owned(), 1, "2026-11-19T08:00:00Z".to_owned())],
            "同じ値の送り直しで行を書き換えた"
        );
    }

    #[test]
    fn 停止か稼働以外の値はdbの制約で拒む() {
        let connection = connection();

        let result = connection.execute(
            "INSERT INTO project_settings (project, paused, updated_at) VALUES ('Annict', 2, '2026-11-19T08:00:00Z')",
            [],
        );

        assert!(result.is_err(), "0と1以外の値を保存した");
    }

    #[test]
    fn フォームの値と稼働か停止かを対応付ける() {
        for operation in [Operation::Running, Operation::Paused] {
            assert_eq!(
                Operation::from_value(operation.value()),
                Some(operation),
                "{operation:?} の値を読み戻せなかった"
            );
        }
        for value in ["", "on", "Paused", "true"] {
            assert_eq!(
                Operation::from_value(value),
                None,
                "{value:?} を稼働か停止として読んだ"
            );
        }
    }
}
