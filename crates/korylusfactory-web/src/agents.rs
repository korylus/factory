//! 人間が決める担当ごとのエージェントの設定の読み書きと、設定の画面での保存。
//!
//! 設定の正本はウェブアプリのDBの `agent_settings` で、プロジェクトと枠ごとに1行を持つ。
//! 行が無い枠は既定 ([`Slot::default_setting`]) として扱い、この版が知らない枠の行は無視する。

use std::collections::{BTreeMap, HashMap};

use axum::Form;
use axum::extract::rejection::{FormRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Redirect, Response};
use jiff::Timestamp;
use korylusfactory_protocol::{
    Agent, AgentEntry, AgentSetting, Effort, InvalidAgentSetting, Model, Slot,
};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};

use crate::app::AppState;
use crate::db::{self, Database};
use crate::{page, settings};

/// 枠の設定を保存するSQL。プロジェクトと枠の行が無ければ作り、あれば置き換える。
const UPSERT_AGENT: &str = "INSERT INTO agent_settings (project, slot, agent, model, effort, updated_at)
VALUES (?1, ?2, ?3, ?4, ?5, ?6)
ON CONFLICT (project, slot) DO UPDATE SET
    agent = excluded.agent, model = excluded.model, effort = excluded.effort, updated_at = excluded.updated_at";

/// プロジェクトの、保存した枠ごとのエージェントの設定を読む。
///
/// この版が知っている枠の保存値を返す。ディレクターが検証できない枠に前回値を使えるよう、保存値はそのまま渡す。
pub async fn load(
    database: &Database,
    project: &str,
) -> Result<BTreeMap<Slot, AgentEntry>, db::Error> {
    let project = project.to_owned();
    database
        .call(move |connection| load_saved(connection, &project))
        .await
}

fn load_saved(
    connection: &Connection,
    project: &str,
) -> rusqlite::Result<BTreeMap<Slot, AgentEntry>> {
    let mut statement = connection
        .prepare("SELECT slot, agent, model, effort FROM agent_settings WHERE project = ?1")?;
    let mut rows = statement.query(params![project])?;
    let mut settings = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let slot: String = row.get(0)?;
        if let Some(slot) = Slot::from_value(&slot) {
            settings.insert(
                slot,
                AgentEntry {
                    agent: row.get(1)?,
                    model: row.get(2)?,
                    effort: row.get(3)?,
                },
            );
        }
    }
    Ok(settings)
}

/// 担当ごとのエージェントを保存するパス (`/projects/<名前>/settings/agents`)。
pub fn path(name: &str) -> String {
    format!("{}/agents", settings::path(name))
}

/// 枠の担当の呼び名。
pub fn slot_label(slot: Slot) -> &'static str {
    match slot {
        Slot::Implementer => "作業者",
        Slot::ReviewerOdd => "レビュアー (奇数回目)",
        Slot::ReviewerEven => "レビュアー (偶数回目)",
    }
}

/// 枠が動かすセッションの説明。
pub fn slot_description(slot: Slot) -> &'static str {
    match slot {
        Slot::Implementer => "実装と作業報告書",
        Slot::ReviewerOdd => "1回目・3回目のレビューと、その指摘の修正",
        Slot::ReviewerEven => "2回目・4回目のレビューと、その指摘の修正",
    }
}

/// エージェントの表示名。
pub fn agent_label(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "Claude Code",
        Agent::Codex => "Codex",
    }
}

/// 設定の画面のフォームの、1つの枠の値。
///
/// 保存した値か、送られた値をそのまま持ち、送られた値を描き直すときも入力を残す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotInput {
    /// 枠。
    pub slot: Slot,
    /// 選んでいるエージェント ([`Agent::value`] の値)。
    pub agent: String,
    /// 選んでいるeffort ([`Effort::value`] の値)。指定しないときは空。
    pub effort: String,
    /// 入力したモデル。指定しないときは空。
    pub model: String,
    /// モデルが規則に合わないときの理由。
    pub error: Option<InvalidAgentSetting>,
}

impl SlotInput {
    /// 保存した値 (無ければ既定) から、フォームの値を作る。
    fn saved(slot: Slot, entry: Option<&AgentEntry>) -> Self {
        let entry = entry
            .cloned()
            .unwrap_or_else(|| slot.default_setting().to_entry());
        Self {
            slot,
            agent: entry.agent,
            effort: entry.effort.unwrap_or_default(),
            model: entry.model.unwrap_or_default(),
            error: None,
        }
    }

    /// フォームの項目の名前 (例: `implementer_model`)。
    pub fn name(&self, part: &str) -> String {
        format!("{}_{part}", self.slot.value())
    }

    /// 画面の要素のIDの接頭辞 (例: `implementer`)。
    pub fn id(&self) -> &'static str {
        self.slot.value()
    }

    /// 枠の担当の呼び名。
    pub fn label(&self) -> &'static str {
        slot_label(self.slot)
    }

    /// 枠が動かすセッションの説明。
    pub fn description(&self) -> &'static str {
        slot_description(self.slot)
    }

    /// 偶数回目のレビュアーの枠かを返す。2つのレビュアーの枠を同じにしたときの注意を添える。
    pub fn is_reviewer_even(&self) -> bool {
        self.slot == Slot::ReviewerEven
    }
}

/// 設定の画面のフォームの値と、最後に保存した時刻。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsForm {
    /// 枠ごとの値。[`Slot::ALL`] の順に並べる。
    pub slots: Vec<SlotInput>,
    /// 枠の `updated_at` のうち最も新しいもの。保存した枠が無ければ `None`。
    pub saved_at: Option<Timestamp>,
}

/// プロジェクトの、保存した担当ごとのエージェントを、設定の画面のフォームの値として読む。
pub async fn load_form(database: &Database, project: &str) -> Result<AgentsForm, db::Error> {
    let project = project.to_owned();
    database
        .call(move |connection| {
            let saved = load_saved(connection, &project)?;
            let saved_at = latest_update(connection, &project)?;
            Ok(AgentsForm {
                slots: Slot::ALL
                    .into_iter()
                    .map(|slot| SlotInput::saved(slot, saved.get(&slot)))
                    .collect(),
                saved_at,
            })
        })
        .await
}

/// 知っている枠の `updated_at` のうち、最も新しいものを読む。
///
/// RFC 3339の文字列は秒の端数の桁数で並びが崩れるため、SQLiteで比べずに読み取ってから比べる。
fn latest_update(connection: &Connection, project: &str) -> rusqlite::Result<Option<Timestamp>> {
    let mut statement =
        connection.prepare("SELECT slot, updated_at FROM agent_settings WHERE project = ?1")?;
    let mut rows = statement.query(params![project])?;
    let mut latest: Option<Timestamp> = None;
    while let Some(row) = rows.next()? {
        let slot: String = row.get(0)?;
        if Slot::from_value(&slot).is_none() {
            continue;
        }
        let text: String = row.get(1)?;
        let time = text.parse::<Timestamp>().map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        latest = Some(latest.map_or(time, |latest| latest.max(time)));
    }
    Ok(latest)
}

/// 送られたフォームを、枠ごとの値として読む。
///
/// フォームからは送れない要求 (枠の項目が欠けている・同じ項目が2つある・選択肢に無いエージェントかeffort) は `None` を返す。
/// モデルが規則に合わないときは、その枠の [`SlotInput::error`] に理由を入れる。
/// 貼り付けたときに紛れ込みやすい前後の空白は、モデルを検証するときに取り除く。
pub fn parse_form(fields: &[(String, String)]) -> Option<Vec<SlotInput>> {
    let mut values: HashMap<&str, &str> = HashMap::new();
    for (name, value) in fields {
        if values.insert(name.as_str(), value.as_str()).is_some() {
            return None;
        }
    }
    Slot::ALL
        .into_iter()
        .map(|slot| {
            let field = |part: &str| {
                values
                    .get(format!("{}_{part}", slot.value()).as_str())
                    .copied()
            };
            let agent = Agent::from_value(field("agent")?)?;
            let effort = field("effort")?;
            if !effort.is_empty() {
                Effort::from_value(effort)?;
            }
            let model = field("model")?;
            let trimmed_model = model.trim();
            let error = if trimmed_model.is_empty() {
                None
            } else {
                Model::parse(trimmed_model).err()
            };
            Some(SlotInput {
                slot,
                agent: agent.value().to_owned(),
                effort: effort.to_owned(),
                model: model.to_owned(),
                error,
            })
        })
        .collect()
}

impl SlotInput {
    /// 検証を通した設定にする。モデルが規則に合わなければ `None` を返す。
    fn setting(&self) -> Option<AgentSetting> {
        let model = self.model.trim();
        AgentEntry {
            agent: self.agent.clone(),
            model: (!model.is_empty()).then(|| model.to_owned()),
            effort: (!self.effort.is_empty()).then(|| self.effort.clone()),
        }
        .validate()
        .ok()
    }
}

/// 枠ごとの設定を、1つのトランザクションで保存する。
///
/// 保存してある値と同じ枠は書き込まず、`updated_at` を最後に値を変えた時刻にする。
/// 行の無い枠は既定として比べ、既定のまま送られた枠には行を作らない。
pub async fn save(
    database: &Database,
    project: &str,
    settings: Vec<(Slot, AgentSetting)>,
) -> Result<(), db::Error> {
    let project = project.to_owned();
    database
        .call(move |connection| save_settings(connection, &project, &settings, Timestamp::now))
        .await
}

fn save_settings(
    connection: &mut Connection,
    project: &str,
    settings: &[(Slot, AgentSetting)],
    now: impl FnOnce() -> Timestamp,
) -> rusqlite::Result<()> {
    // 比較と書き込みの間に別の接続から更新されないよう、読み取る前に書き込みロックを取る。
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let updated_at = now().to_string();
    for (slot, setting) in settings {
        let entry = setting.to_entry();
        let saved = transaction
            .query_row(
                "SELECT agent, model, effort FROM agent_settings WHERE project = ?1 AND slot = ?2",
                params![project, slot.value()],
                |row| {
                    Ok(AgentEntry {
                        agent: row.get(0)?,
                        model: row.get(1)?,
                        effort: row.get(2)?,
                    })
                },
            )
            .optional()?
            .unwrap_or_else(|| slot.default_setting().to_entry());
        if saved != entry {
            transaction.execute(
                UPSERT_AGENT,
                params![
                    project,
                    slot.value(),
                    entry.agent,
                    entry.model,
                    entry.effort,
                    updated_at
                ],
            )?;
        }
    }
    transaction.commit()
}

/// `POST /projects/{project}/settings/agents`: 担当ごとのエージェントを3つの枠まとめて保存し、設定の画面へ戻す。
///
/// 設定に無いプロジェクトには404、フォームからは送れない値には400を返す。
/// モデルが規則に合わないときは保存せず、入力を残したまま422で設定の画面を描き直す。
pub async fn update(
    State(state): State<AppState>,
    segment: Result<Path<String>, PathRejection>,
    form: Result<Form<Vec<(String, String)>>, FormRejection>,
) -> Response {
    let Some(name) = settings::configured(&state, segment) else {
        return page::not_found();
    };
    let Some(slots) = form.ok().and_then(|Form(fields)| parse_form(&fields)) else {
        return page::bad_request();
    };
    let settings: Option<Vec<(Slot, AgentSetting)>> = slots
        .iter()
        .map(|input| Some((input.slot, input.setting()?)))
        .collect();
    let Some(settings) = settings else {
        return rerender(&state, &name, slots).await;
    };
    match save(&state.database, &name, settings).await {
        Ok(()) => Redirect::to(&settings::path(&name)).into_response(),
        Err(error) => {
            // 原因のSQLiteのエラー文には保存した値が入ることがあるため、原因をたどらずに記録する。
            log::error!("担当ごとのエージェントの設定を保存できません: {error}");
            page::internal_error()
        }
    }
}

/// モデルが規則に合わないときに、送られた値を残したまま422で設定の画面を描き直す。
async fn rerender(state: &AppState, name: &str, slots: Vec<SlotInput>) -> Response {
    let loaded = async {
        let operation = settings::load(&state.database, name).await?;
        let saved = load_form(&state.database, name).await?;
        Ok::<_, db::Error>((operation, saved.saved_at))
    };
    match loaded.await {
        Ok((operation, saved_at)) => page::render(
            StatusCode::UNPROCESSABLE_ENTITY,
            &page::SettingsPage::new(
                name,
                operation,
                AgentsForm { slots, saved_at },
                Timestamp::now(),
            ),
        ),
        Err(error) => {
            log::error!("プロジェクトの設定を読めません: {error}");
            page::internal_error()
        }
    }
}

#[cfg(test)]
mod tests {
    use korylusfactory_protocol::InvalidAgentSetting;

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

    fn insert(
        connection: &Connection,
        project: &str,
        slot: &str,
        agent: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> rusqlite::Result<usize> {
        connection.execute(
            "INSERT INTO agent_settings (project, slot, agent, model, effort, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, '2026-11-20T08:00:00Z')",
            params![project, slot, agent, model, effort],
        )
    }

    #[test]
    fn 保存した枠の設定をプロジェクトごとに読み_行の無い枠は載せない() {
        let connection = connection();
        insert(
            &connection,
            "Annict",
            "implementer",
            "claude",
            Some("claude-opus-5-5"),
            Some("high"),
        )
        .expect("作業者の設定を保存できなかった");
        insert(&connection, "Annict", "reviewer_odd", "codex", None, None)
            .expect("奇数回目の設定を保存できなかった");
        insert(&connection, "Mewst", "reviewer_even", "codex", None, None)
            .expect("ほかのプロジェクトの設定を保存できなかった");

        let got = load_saved(&connection, "Annict").expect("設定の行を読めなかった");

        assert_eq!(
            got,
            BTreeMap::from([
                (
                    Slot::Implementer,
                    AgentEntry {
                        agent: "claude".to_owned(),
                        model: Some("claude-opus-5-5".to_owned()),
                        effort: Some("high".to_owned()),
                    }
                ),
                (
                    Slot::ReviewerOdd,
                    AgentEntry {
                        agent: "codex".to_owned(),
                        model: None,
                        effort: None,
                    }
                ),
            ]),
            "読んだ設定が違う"
        );
        assert!(
            load_saved(&connection, "Wikino")
                .expect("設定の行を読めなかった")
                .is_empty(),
            "保存していないプロジェクトに設定があった"
        );
    }

    #[test]
    fn 知らない枠を省き_既知の枠は検証を通らない値も読む() {
        let connection = connection();
        insert(&connection, "Annict", "watcher", "claude", None, None)
            .expect("知らない枠の行を保存できなかった");
        insert(
            &connection,
            "Annict",
            "implementer",
            "codex",
            Some("-c"),
            None,
        )
        .expect("規則に合わないモデルの行を保存できなかった");
        insert(
            &connection,
            "Annict",
            "reviewer_even",
            "codex",
            None,
            Some("max"),
        )
        .expect("偶数回目の設定を保存できなかった");

        let got = load_saved(&connection, "Annict").expect("設定の行を読めなかった");

        assert_eq!(
            got.keys().copied().collect::<Vec<_>>(),
            [Slot::Implementer, Slot::ReviewerEven],
            "既知の枠の行だけを読まなかった"
        );
        assert_eq!(
            got[&Slot::Implementer].validate(),
            Err(InvalidAgentSetting::Model),
            "検証を通らないモデルを保持しなかった"
        );
    }

    #[test]
    fn 知らないエージェントとeffortと長すぎるモデルはdbの制約で拒む() {
        let connection = connection();
        let too_long = "a".repeat(65);
        for (agent, model, effort) in [
            ("gemini", None, None),
            ("claude", Some(""), None),
            ("claude", Some(too_long.as_str()), None),
            ("claude", None, Some("minimal")),
        ] {
            let result = insert(&connection, "Annict", "implementer", agent, model, effort);

            assert!(
                result.is_err(),
                "{agent:?}・{model:?}・{effort:?} を保存した"
            );
        }
        insert(
            &connection,
            "Annict",
            "implementer",
            "claude",
            Some(&"a".repeat(64)),
            None,
        )
        .expect("64文字のモデルを保存できなかった");
        assert!(
            insert(&connection, "Annict", "implementer", "codex", None, None).is_err(),
            "同じプロジェクトと枠の行を2つ保存した"
        );
    }

    fn fields(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn defaults() -> Vec<(String, String)> {
        fields(&[
            ("implementer_agent", "claude"),
            ("implementer_effort", ""),
            ("implementer_model", ""),
            ("reviewer_odd_agent", "codex"),
            ("reviewer_odd_effort", ""),
            ("reviewer_odd_model", ""),
            ("reviewer_even_agent", "claude"),
            ("reviewer_even_effort", ""),
            ("reviewer_even_model", ""),
        ])
    }

    fn replaced(name: &str, value: &str) -> Vec<(String, String)> {
        defaults()
            .into_iter()
            .map(|(field, old)| {
                let value = if field == name { value.to_owned() } else { old };
                (field, value)
            })
            .collect()
    }

    fn at(text: &str) -> Timestamp {
        text.parse().expect("時刻を読めなかった")
    }

    #[test]
    fn フォームの値を枠の順に読み_入力を残してモデルの前後の空白を除いて検証する() {
        let mut submitted = replaced("implementer_model", " opus\t");
        submitted.retain(|(name, _)| name != "reviewer_odd_model");
        submitted.push(("reviewer_odd_model".to_owned(), " -c ".to_owned()));
        submitted.push(("future_agent".to_owned(), "claude".to_owned()));

        let got = parse_form(&submitted).expect("フォームを読めなかった");

        assert_eq!(
            got.iter().map(|input| input.slot).collect::<Vec<_>>(),
            Slot::ALL,
            "枠の順に読まなかった"
        );
        assert_eq!(got[0].model, " opus\t", "モデルの前後の空白を残さなかった");
        assert_eq!(got[0].error, None, "正しいモデルを誤りとした");
        assert_eq!(got[1].model, " -c ", "誤ったモデルの入力を残さなかった");
        assert_eq!(
            got[1].error,
            Some(InvalidAgentSetting::Model),
            "規則に合わないモデルに理由を付けなかった"
        );
        assert_eq!(got[2].error, None, "空のモデルを誤りとした");
    }

    #[test]
    fn フォームからは送れない値は読まない() {
        let mut missing = defaults();
        missing.retain(|(name, _)| name != "reviewer_even_effort");
        let mut doubled = defaults();
        doubled.push(("implementer_model".to_owned(), "opus".to_owned()));
        for (case, submitted) in [
            (
                "知らないエージェント",
                replaced("implementer_agent", "gemini"),
            ),
            ("空のエージェント", replaced("implementer_agent", "")),
            ("知らないeffort", replaced("reviewer_odd_effort", "minimal")),
            ("大文字のeffort", replaced("reviewer_odd_effort", "High")),
            ("欠けた項目", missing),
            ("2つある項目", doubled),
        ] {
            assert_eq!(parse_form(&submitted), None, "{case}のフォームを読んだ");
        }
    }

    #[test]
    fn 変えた枠だけを書き込み_既定のままの枠には行を作らない() {
        let mut connection = connection();
        let codex = AgentSetting {
            agent: Agent::Codex,
            model: None,
            effort: Some(Effort::Low),
        };
        let settings = [
            (Slot::Implementer, codex.clone()),
            (Slot::ReviewerOdd, Slot::ReviewerOdd.default_setting()),
            (Slot::ReviewerEven, Slot::ReviewerEven.default_setting()),
        ];
        save_settings(&mut connection, "Annict", &settings, || {
            at("2026-11-20T08:00:00Z")
        })
        .expect("設定を保存できなかった");
        save_settings(&mut connection, "Annict", &settings, || {
            at("2026-11-20T09:00:00Z")
        })
        .expect("同じ設定を送り直せなかった");

        let saved = load_saved(&connection, "Annict").expect("設定の行を読めなかった");
        assert_eq!(
            saved,
            BTreeMap::from([(Slot::Implementer, codex.to_entry())]),
            "変えた枠だけを保存しなかった"
        );
        assert_eq!(
            latest_update(&connection, "Annict").expect("保存した時刻を読めなかった"),
            Some(at("2026-11-20T08:00:00Z")),
            "同じ値の送り直しで保存した時刻を変えた"
        );

        save_settings(
            &mut connection,
            "Annict",
            &[(Slot::Implementer, Slot::Implementer.default_setting())],
            || at("2026-11-20T10:00:00Z"),
        )
        .expect("既定に戻せなかった");
        assert_eq!(
            load_saved(&connection, "Annict").expect("設定の行を読めなかった")[&Slot::Implementer],
            Slot::Implementer.default_setting().to_entry(),
            "保存した枠を既定の値に戻さなかった"
        );
    }

    #[test]
    fn 保存時刻は書き込みロックを取ってから採る() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れなかった");
        let path = dir.path().join("agents.sqlite3");
        let mut connection = Connection::open(&path).expect("DBを開けなかった");
        for sql in db::MIGRATIONS {
            connection
                .execute_batch(sql)
                .expect("マイグレーションを流せなかった");
        }
        // ロックを持たれているときに待たずに失敗させ、時刻を採った時点のロックの有無を確かめる。
        let other = Connection::open(&path).expect("別の接続を開けなかった");
        other
            .busy_timeout(std::time::Duration::ZERO)
            .expect("別の接続の待ち時間を変えられなかった");
        let codex = AgentSetting {
            agent: Agent::Codex,
            model: None,
            effort: None,
        };

        save_settings(
            &mut connection,
            "Annict",
            &[(Slot::Implementer, codex)],
            || {
                assert!(
                    other.execute_batch("BEGIN IMMEDIATE").is_err(),
                    "書き込みロックを取る前に保存時刻を採った"
                );
                at("2026-11-20T08:00:00Z")
            },
        )
        .expect("設定を保存できなかった");

        assert_eq!(
            latest_update(&connection, "Annict").expect("保存した時刻を読めなかった"),
            Some(at("2026-11-20T08:00:00Z")),
            "ロックを取ってから採った時刻で保存しなかった"
        );
    }

    #[test]
    fn 最後に保存した時刻は知っている枠の最も新しい時刻にする() {
        let connection = connection();
        assert_eq!(
            latest_update(&connection, "Annict").expect("保存した時刻を読めなかった"),
            None,
            "保存していないのに時刻を返した"
        );
        connection
            .execute_batch(
                "INSERT INTO agent_settings (project, slot, agent, updated_at) VALUES
                 ('Annict', 'implementer', 'claude', '2026-11-20T08:00:00.5Z'),
                 ('Annict', 'reviewer_odd', 'codex', '2026-11-20T08:00:00Z'),
                 ('Annict', 'watcher', 'codex', '2026-11-21T08:00:00Z'),
                 ('Mewst', 'implementer', 'codex', '2026-11-22T08:00:00Z');",
            )
            .expect("設定を保存できなかった");

        assert_eq!(
            latest_update(&connection, "Annict").expect("保存した時刻を読めなかった"),
            Some(at("2026-11-20T08:00:00.5Z")),
            "知っている枠の最も新しい時刻を返さなかった"
        );
    }
}
