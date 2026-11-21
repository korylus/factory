//! ディレクター (`korylusfactory start`) とウェブアプリがやり取りする型。
//!
//! ディレクターは [`StatusReport`] をJSONにして `PUT /api/v1/director/status` で送り、ウェブアプリはそれを受け取って表示する。
//! また、人間がウェブアプリで決めた [`Settings`] を `GET /api/v1/director/settings` で読む。
//! ディレクターとウェブアプリは別々に入れ直すため、版がずれても読めるよう、次を守る。
//!
//! - 送る側も受ける側も、知らない項目を無視する (`deny_unknown_fields` を付けない)
//! - 値が無いことはキーを省いて表す。受け取るときは、キーの省略と `null` を同じに扱う
//! - 一覧は空でも `[]` で送る。受け取るときは、キーの省略を空の一覧として扱う
//! - 項目を足すときは、無くても読めるよう `Option` か `#[serde(default)]` にする

mod agents;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use agents::{
    Agent, AgentEntry, AgentEntryResult, AgentSetting, Effort, InvalidAgentSetting, Model, Slot,
};

/// ディレクターが送る、プロジェクトの状態の報告。
///
/// 送るたびにプロジェクトの状態を丸ごと置き換える。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    /// 送った時刻 (RFC 3339のUTC。例: `2026-11-19T08:00:00Z`)。
    pub sent_at: String,
    /// 送ったkorylusfactoryの版。
    pub korylusfactory: Version,
    /// 作業者かレビュアーのセッションを実行しているかどうか。
    pub running: bool,
    /// ウェブアプリの設定で停止したことを反映し、次の作業を始めずに待っているかどうか。
    ///
    /// 停止に切り替えられても、セッションを実行している間は設定をまだ読んでいないため偽になる。
    /// この項目の無い版のディレクターの報告は、偽として読む。
    #[serde(default)]
    pub paused: bool,
    /// 進め始めて、まだ完了を記録していない作業計画書。
    #[serde(default)]
    pub progress: Vec<PlanProgress>,
    /// これから進める作業計画書。進める順に並べる。
    #[serde(default)]
    pub upcoming: Vec<UpcomingPlan>,
}

/// korylusfactoryの版。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    /// バージョン番号 (例: `0.1.0`)。
    pub version: String,
    /// ビルド元のコミット。
    pub commit: String,
}

/// 進行中の作業計画書1つの状態。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanProgress {
    /// 計画書名 (例: `webapp-status`)。
    pub name: String,
    /// 作業計画書のタイトル。
    pub title: String,
    /// 作業計画書のページのURL。
    pub url: String,
    /// 進めているタスクの番号 (例: `1-2`)。作業報告書を進めているときは無い。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// 進めているステップ。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<Step>,
    /// 人間を待っているときの、待っている理由。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<Wait>,
    /// 作業計画書のタスクの一覧。計画書に書いた順に並べる。
    #[serde(default)]
    pub tasks: Vec<Task>,
    /// この作業計画書で直近に起動したセッション。新しい順に並べ、実行中のものを含む。
    ///
    /// ディレクターは直近の5件を送る。この項目の無い版のディレクターの報告は、空として読む。
    #[serde(default)]
    pub sessions: Vec<Session>,
}

/// ディレクターが起動したセッション1つ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// タスクの番号 (例: `1-2`)。作業報告書のセッションでは無い。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// セッションで進めたステップ。
    pub step: Step,
    /// レビューの回数 (1始まり)。レビューと、その指摘の修正のときだけ持つ。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<u32>,
    /// セッションを動かした担当。
    pub role: Role,
    /// 使ったエージェント ([`Agent::value`] の値)。
    pub agent: String,
    /// 指定したモデル。指定しなかったときは無い。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 指定したeffort ([`Effort::value`] の値)。指定しなかったときは無い。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// 起動した時刻 (RFC 3339のUTC)。
    pub started_at: String,
    /// 終わった時刻 (RFC 3339のUTC)。実行中なら無い。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// 終わったときの結果。実行中なら無い。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Outcome>,
}

/// セッションを動かした担当。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    /// 担当の種類 (例: `implementer`)。
    pub kind: String,
    /// 画面に出す表示名 (例: `作業者`)。
    pub label: String,
}

/// セッションが終わったときの結果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    /// 結果の種類 (CLIの状態ファイルと同じ値。例: `recorded`)。
    pub kind: String,
    /// 画面に出す表示名 (例: `Request Changes`・`質問を返した`)。
    pub label: String,
}

/// 進めているステップ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// ステップの種類 (CLIの状態ファイルと同じ値。例: `review`)。
    pub kind: String,
    /// 画面に出す表示名 (CLIの `status` と同じ言葉。例: `レビュー`)。
    pub label: String,
}

/// 人間を待っている理由。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wait {
    /// 人間待ちの種類 (CLIの状態ファイルと同じ値)。
    pub kind: String,
    /// 画面に出す表示名 (CLIの `status` と同じ言葉。例: `セッションからの質問`)。
    pub label: String,
    /// 待っている理由の文面。セッションやWikinoから来たデータで、画面ではエスケープして出す。
    pub reason: String,
}

/// 作業計画書のタスク1つ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// タスクの番号 (例: `1-2`)。
    pub number: String,
    /// タスクのタイトル。
    pub title: String,
    /// タスクの進み具合。
    pub status: TaskStatus,
}

/// タスクの進み具合。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// 済み。
    Done,
    /// 進めている。
    InProgress,
    /// 未着手。
    Pending,
    /// この版の知らない進み具合。新しい版のディレクターが足した値を受け取っても、報告全体を拒まないためのもの。
    #[serde(other)]
    Unknown,
}

/// これから進める作業計画書1つ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpcomingPlan {
    /// ロードマップに書いた作業計画書のタイトル。
    pub title: String,
    /// 作業計画書のページのURL。ロードマップのリンク先のページが見つからないときは無い。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// 人間がウェブアプリで決めた、プロジェクトの設定。ディレクターが読む。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Settings {
    /// 停止していれば真。ディレクターは、実行中のセッションが終わったところで次の作業を始めずに待つ。
    /// この項目の無い応答は、稼働として読む。
    #[serde(default)]
    pub paused: bool,
    /// 担当ごとのエージェントの設定。キーは枠 ([`Slot::value`] の値) で、人間が保存した枠だけを載せる。
    ///
    /// 載っていない枠は既定 ([`Slot::default_setting`]) として扱い、知らない枠は無視する。
    /// 形式の誤りも枠ごとに保持する。使う前に [`AgentEntryResult::validate`] を通す。
    /// この項目の無い応答は、すべての枠を既定として読む。
    #[serde(default)]
    pub agents: BTreeMap<String, AgentEntryResult>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn report() -> StatusReport {
        StatusReport {
            sent_at: "2026-11-19T08:00:00Z".to_owned(),
            korylusfactory: Version {
                version: "0.1.0".to_owned(),
                commit: "abc1234".to_owned(),
            },
            running: true,
            paused: false,
            progress: vec![PlanProgress {
                name: "webapp-status".to_owned(),
                title: "ウェブアプリの骨格と状態の表示 作業計画書".to_owned(),
                url: "https://wikino.example.com/s/example/pages/1".to_owned(),
                task: Some("1-2".to_owned()),
                step: Some(Step {
                    kind: "review".to_owned(),
                    label: "レビュー".to_owned(),
                }),
                wait: None,
                tasks: vec![Task {
                    number: "1-2".to_owned(),
                    title: "ウェブアプリのクレートを作る".to_owned(),
                    status: TaskStatus::InProgress,
                }],
                sessions: vec![
                    Session {
                        task: Some("1-2".to_owned()),
                        step: Step {
                            kind: "review".to_owned(),
                            label: "レビュー".to_owned(),
                        },
                        review: Some(2),
                        role: Role {
                            kind: "reviewer".to_owned(),
                            label: "レビュアー".to_owned(),
                        },
                        agent: "claude".to_owned(),
                        model: Some("claude-opus-5-5".to_owned()),
                        effort: Some("high".to_owned()),
                        started_at: "2026-11-19T07:50:00Z".to_owned(),
                        ended_at: None,
                        result: None,
                    },
                    Session {
                        task: Some("1-2".to_owned()),
                        step: Step {
                            kind: "implement".to_owned(),
                            label: "実装".to_owned(),
                        },
                        review: None,
                        role: Role {
                            kind: "implementer".to_owned(),
                            label: "作業者".to_owned(),
                        },
                        agent: "codex".to_owned(),
                        model: None,
                        effort: None,
                        started_at: "2026-11-19T07:20:00Z".to_owned(),
                        ended_at: Some("2026-11-19T07:48:00Z".to_owned()),
                        result: Some(Outcome {
                            kind: "recorded".to_owned(),
                            label: "記録した".to_owned(),
                        }),
                    },
                ],
            }],
            upcoming: vec![UpcomingPlan {
                title: "メッセージ 作業計画書".to_owned(),
                url: None,
            }],
        }
    }

    #[test]
    fn 報告をjsonにして読み戻すと同じ値になる() {
        let paused_report = StatusReport {
            running: false,
            paused: true,
            ..report()
        };
        for report in [report(), paused_report] {
            let value = serde_json::to_value(&report).expect("JSONにできなかった");
            assert_eq!(
                value["paused"],
                json!(report.paused),
                "停止のJSONの形が違う"
            );

            let back: StatusReport = serde_json::from_value(value).expect("JSONを読めなかった");
            assert_eq!(back, report, "読み戻した報告が元と違う");
        }
    }

    #[test]
    fn 値が無い項目はキーを省いて送る() {
        let value = serde_json::to_value(report()).expect("JSONにできなかった");

        assert_eq!(
            value["progress"][0].get("wait"),
            None,
            "無い人間待ちのキーを省かなかった"
        );
        assert_eq!(
            value["upcoming"][0].get("url"),
            None,
            "無いURLのキーを省かなかった"
        );
        assert_eq!(
            value["progress"][0]["tasks"][0]["status"],
            json!("in_progress"),
            "進み具合の値の形が違う"
        );
        let running = &value["progress"][0]["sessions"][0];
        for key in ["ended_at", "result"] {
            assert_eq!(
                running.get(key),
                None,
                "実行中のセッションの {key} のキーを省かなかった"
            );
        }
        let implementer = &value["progress"][0]["sessions"][1];
        for key in ["review", "model", "effort"] {
            assert_eq!(
                implementer.get(key),
                None,
                "指定しなかったセッションの {key} のキーを省かなかった"
            );
        }
    }

    #[test]
    fn 知らない項目は無視して読む() {
        let mut value = serde_json::to_value(report()).expect("JSONにできなかった");
        value["future"] = json!({"nested": true});
        value["progress"][0]["future"] = json!(1);
        value["progress"][0]["tasks"][0]["status"] = json!("blocked");

        let got: StatusReport =
            serde_json::from_value(value).expect("知らない項目のある報告を読めなかった");

        assert_eq!(
            got.progress[0].tasks[0].status,
            TaskStatus::Unknown,
            "知らない進み具合を Unknown として読まなかった"
        );
    }

    #[test]
    fn 省いたキーとnullは値が無いものとして読む() {
        let got: StatusReport = serde_json::from_value(json!({
            "sent_at": "2026-11-19T08:00:00Z",
            "korylusfactory": {"version": "0.1.0", "commit": "abc1234"},
            "running": false,
            "progress": [{"name": "a", "title": "A", "url": "https://example.com/", "task": null}]
        }))
        .expect("最小の報告を読めなかった");

        assert_eq!(
            got.progress[0].task, None,
            "nullのタスクを値が無いものとして読まなかった"
        );
        assert!(
            got.progress[0].tasks.is_empty(),
            "省いたタスクの一覧を空にしなかった"
        );
        assert!(
            got.progress[0].sessions.is_empty(),
            "省いたセッションの一覧を空にしなかった"
        );
        assert!(
            got.upcoming.is_empty(),
            "省いたこれから進める作業計画書を空にしなかった"
        );
        assert!(!got.paused, "省いた停止を偽として読まなかった");
    }

    #[test]
    fn 設定は停止をjsonの真偽値で読み書きし_省いた停止は稼働として読む() {
        let value = serde_json::to_value(Settings {
            paused: true,
            ..Settings::default()
        })
        .expect("JSONにできなかった");
        assert_eq!(
            value,
            json!({"paused": true, "agents": {}}),
            "設定のJSONの形が違う"
        );
        let paused: Settings = serde_json::from_value(value).expect("停止の設定を読めなかった");
        assert!(paused.paused, "停止を真として読まなかった");

        let empty: Settings =
            serde_json::from_value(json!({"future": 1})).expect("停止の無い設定を読めなかった");
        assert_eq!(
            empty,
            Settings::default(),
            "省いた停止を稼働として読まなかった"
        );
    }

    #[test]
    fn 担当ごとのエージェントは保存した枠だけを載せ_指定しないモデルとeffortはキーを省く() {
        let settings = Settings {
            paused: false,
            agents: BTreeMap::from([
                (
                    Slot::Implementer.value().to_owned(),
                    AgentEntryResult::Decoded(
                        AgentSetting {
                            agent: Agent::Claude,
                            model: Some(
                                Model::parse("claude-opus-5-5").expect("モデルを読めなかった"),
                            ),
                            effort: Some(Effort::High),
                        }
                        .to_entry(),
                    ),
                ),
                (
                    Slot::ReviewerOdd.value().to_owned(),
                    AgentEntryResult::Decoded(Slot::ReviewerOdd.default_setting().to_entry()),
                ),
            ]),
        };

        let value = serde_json::to_value(&settings).expect("JSONにできなかった");

        assert_eq!(
            value,
            json!({
                "paused": false,
                "agents": {
                    "implementer": {"agent": "claude", "model": "claude-opus-5-5", "effort": "high"},
                    "reviewer_odd": {"agent": "codex"}
                }
            }),
            "担当ごとのエージェントのJSONの形が違う"
        );
        let back: Settings = serde_json::from_value(value).expect("JSONを読めなかった");
        assert_eq!(back, settings, "読み戻した設定が元と違う");
    }

    #[test]
    fn 担当ごとのエージェントの無い設定は空として読み_知らない枠と値は検証の前まで読む() {
        let missing: Settings = serde_json::from_value(json!({"paused": true}))
            .expect("agentsの無い設定を読めなかった");
        assert!(
            missing.agents.is_empty(),
            "省いたagentsを空として読まなかった"
        );

        let got: Settings = serde_json::from_value(json!({
            "agents": {
                "watcher": {"agent": "claude", "future": 1},
                "reviewer_even": {"agent": "gemini", "model": null, "effort": "minimal"}
            }
        }))
        .expect("知らない枠と値のある設定を読めなかった");

        assert_eq!(
            got.agents.keys().collect::<Vec<_>>(),
            ["reviewer_even", "watcher"],
            "知らない枠と値のある枠を読まなかった"
        );
        let even = &got.agents["reviewer_even"];
        assert_eq!(
            *even,
            AgentEntryResult::Decoded(AgentEntry {
                agent: "gemini".to_owned(),
                model: None,
                effort: Some("minimal".to_owned()),
            }),
            "nullのモデルを指定しないものとして読まなかった"
        );
        assert_eq!(
            even.validate(),
            Err(InvalidAgentSetting::Agent),
            "知らないエージェントが検証を通った"
        );
    }

    #[test]
    fn 形式の違う枠があっても停止と正常な枠を読み_誤った枠は検証で拒む() {
        let got: Settings = serde_json::from_str(
            r#"{
                "paused": true,
                "agents": {
                    "watcher": {},
                    "implementer": {"agent": "claude"},
                    "reviewer_odd": {"agent": "codex", "effort": 42},
                    "reviewer_even": ["claude", null, null]
                }
            }"#,
        )
        .expect("枠の形式が違うと設定全体を読めなかった");

        assert!(got.paused, "停止を真として読まなかった");
        assert_eq!(
            got.agents["implementer"].validate(),
            Ok(Slot::Implementer.default_setting()),
            "正常な枠を読めなかった"
        );
        for slot in ["reviewer_odd", "reviewer_even"] {
            assert_eq!(
                got.agents[slot].validate(),
                Err(InvalidAgentSetting::Format),
                "{slot} の形式の誤りを検出しなかった"
            );
        }
    }
}
