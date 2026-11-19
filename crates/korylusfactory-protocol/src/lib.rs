//! ディレクター (`korylusfactory start`) とウェブアプリがやり取りする型。
//!
//! ディレクターは [`StatusReport`] をJSONにして `PUT /api/v1/director/status` で送り、ウェブアプリはそれを受け取って表示する。
//! ディレクターとウェブアプリは別々に入れ直すため、版がずれても読めるよう、次を守る。
//!
//! - 送る側も受ける側も、知らない項目を無視する (`deny_unknown_fields` を付けない)
//! - 値が無いことはキーを省いて表す。受け取るときは、キーの省略と `null` を同じに扱う
//! - 一覧は空でも `[]` で送る。受け取るときは、キーの省略を空の一覧として扱う
//! - 項目を足すときは、無くても読めるよう `Option` か `#[serde(default)]` にする

use serde::{Deserialize, Serialize};

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
            }],
            upcoming: vec![UpcomingPlan {
                title: "メッセージ 作業計画書".to_owned(),
                url: None,
            }],
        }
    }

    #[test]
    fn 報告をjsonにして読み戻すと同じ値になる() {
        let report = report();

        let text = serde_json::to_string(&report).expect("JSONにできなかった");
        let back: StatusReport = serde_json::from_str(&text).expect("JSONを読めなかった");

        assert_eq!(back, report, "読み戻した報告が元と違う");
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
            got.upcoming.is_empty(),
            "省いたこれから進める作業計画書を空にしなかった"
        );
    }
}
