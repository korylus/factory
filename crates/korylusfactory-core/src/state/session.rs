//! ディレクターが起動したセッションの記録。
//!
//! セッションを起動するときに開き、結果を確かめて記録するときに閉じる。
//! 状態の報告で直近のセッションを人間に見せるために残し、進行の判定には使わない。

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use super::{Step, Verdict};

/// セッションを起動する担当の枠。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Slot {
    /// 作業者。実装・ブランチを作らない作業・作業報告書の作成を動かす。
    Implementer,
    /// レビュアー (奇数回目)。1回目・3回目のレビューと、そのレビューの指摘の修正を動かす。
    ReviewerOdd,
    /// レビュアー (偶数回目)。2回目・4回目のレビューと、そのレビューの指摘の修正を動かす。
    ReviewerEven,
}

impl Slot {
    /// number回目 (1始まり) のレビューと、その指摘の修正を動かす枠を返す。
    pub fn for_review(number: u32) -> Self {
        if number % 2 == 1 {
            Slot::ReviewerOdd
        } else {
            Slot::ReviewerEven
        }
    }
}

/// セッションを動かしたエージェントと、指定した値。値はコマンドのクレートが決め、ここでは記録するだけにする。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAgent {
    /// エージェント (例: `claude`)。
    pub agent: String,
    /// 指定したモデル。指定しなかったときは `None`。
    pub model: Option<String>,
    /// 指定したeffort。指定しなかったときは `None`。
    pub effort: Option<String>,
}

/// 起動したセッション1つの記録。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// タスクの番号。作業報告書のセッションでは `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// セッションで進めたステップ。
    pub step: Step,
    /// レビューの回数 (1始まり)。レビューと、その指摘の修正のときだけ持つ。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<u32>,
    /// セッションを動かした枠。
    pub slot: Slot,
    /// 使ったエージェント。
    pub agent: String,
    /// 指定したモデル。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 指定したeffort。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// 起動した時刻。
    pub started_at: Timestamp,
    /// セッションが終わった時刻。実行中か、中断で終了時刻が分からないときは `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<Timestamp>,
    /// 結果。実行中なら `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<SessionResult>,
    /// レビューを記録したときの総合評価。
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_verdict"
    )]
    pub verdict: Option<Verdict>,
}

impl SessionRecord {
    /// まだ結果を記録していないかを返す。
    pub fn is_open(&self) -> bool {
        self.result.is_none()
    }
}

/// セッションの結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionResult {
    /// 結果を確かめて記録した。
    Recorded,
    /// セッションが人間の判断を求めて終わった。
    Asked,
    /// 途中で止まったか完了の条件を満たさず、記録できなかった。やり直すか、やり直しの上限で人間待ちにした。
    Unfinished,
    /// 担当ごとのエージェントの設定 (モデルかeffort) を、セッションのCLIが受け付けなかった。
    SettingsRejected,
    /// `korylusfactory start` を止めたため、セッションを終わらせた。
    Cancelled,
    /// セッションの間に状態が変わったため、結果を記録しなかった。
    Stale,
    /// 結果を記録する前に `korylusfactory start` が止まった。次のセッションを起動するときに閉じる。
    Interrupted,
}

impl SessionResult {
    /// 結果の表示名を返す。
    pub fn label(self) -> &'static str {
        match self {
            SessionResult::Recorded => "記録した",
            SessionResult::Asked => "人間の判断を求めた",
            SessionResult::Unfinished => "終えられなかった",
            SessionResult::SettingsRejected => "設定をCLIが受け付けなかった",
            SessionResult::Cancelled => "取り消した",
            SessionResult::Stale => "状態が変わったため記録しなかった",
            SessionResult::Interrupted => "中断",
        }
    }
}

/// `Option<Verdict>` を、レビュードキュメントに書く表記 (`Request Changes` など) で読み書きする。
mod optional_verdict {
    use serde::{Deserialize, Deserializer, Serializer};

    use super::Verdict;
    use crate::state::record::verdict;

    #[expect(
        clippy::ref_option,
        clippy::trivially_copy_pass_by_ref,
        reason = "serdeの `with` が、書く値を参照で渡すため"
    )]
    pub(super) fn serialize<S: Serializer>(
        value: &Option<Verdict>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(verdict) => serializer.serialize_some(verdict.as_str()),
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Verdict>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .as_deref()
            .map(verdict::parse)
            .transpose()
    }
}
