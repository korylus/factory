//! 担当ごとのエージェントの設定と、その値の検証。
//!
//! 人間はウェブアプリで、セッションを起動する担当の枠 ([`Slot`]) ごとに、エージェント・モデル・effortを決める。
//! ウェブアプリは保存するときに、ディレクターは読んだときに、同じ検証 ([`AgentEntry::validate`]) を通す。
//! ウェブアプリが破られても、この設定を通してセッションに任意の引数を渡せないよう、
//! エージェントとeffortは固定の一覧から選ばせ、モデルは文字の規則で確かめる。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// セッションを起動する担当の枠。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Slot {
    /// 作業者。実装・ブランチを作らない作業・作業報告書の作成を動かす。
    Implementer,
    /// レビュアー (奇数回目)。1回目・3回目のレビューと、そのレビューの指摘の修正を動かす。
    ReviewerOdd,
    /// レビュアー (偶数回目)。2回目・4回目のレビューと、そのレビューの指摘の修正を動かす。
    ReviewerEven,
}

impl Slot {
    /// この版が知っている枠。画面に並べる順にする。
    pub const ALL: [Self; 3] = [Self::Implementer, Self::ReviewerOdd, Self::ReviewerEven];

    /// 送受信とDBで使う値から読み取る。知らない枠なら `None` を返す。
    pub fn from_value(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|slot| slot.value() == value)
    }

    /// 送受信とDBで使う値。
    pub fn value(self) -> &'static str {
        match self {
            Self::Implementer => "implementer",
            Self::ReviewerOdd => "reviewer_odd",
            Self::ReviewerEven => "reviewer_even",
        }
    }

    /// 設定を保存していない枠に使う、既定のエージェントの設定。モデルとeffortは指定しない。
    pub fn default_setting(self) -> AgentSetting {
        let agent = match self {
            Self::Implementer | Self::ReviewerEven => Agent::Claude,
            Self::ReviewerOdd => Agent::Codex,
        };
        AgentSetting {
            agent,
            model: None,
            effort: None,
        }
    }
}

/// セッションを動かすエージェント。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Agent {
    /// Claude Code。
    Claude,
    /// Codex。
    Codex,
}

impl Agent {
    /// この版が知っているエージェント。
    pub const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    /// 送受信とDBで使う値から読み取る。知らないエージェントなら `None` を返す。
    pub fn from_value(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|agent| agent.value() == value)
    }

    /// 送受信とDBで使う値。
    pub fn value(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// セッションに指定するeffort。両方のCLIが受け付ける値に限る。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Effort {
    /// `low`。
    Low,
    /// `medium`。
    Medium,
    /// `high`。
    High,
    /// `xhigh`。
    Xhigh,
    /// `max`。
    Max,
}

impl Effort {
    /// この版が知っているeffort。画面に並べる順にする。
    pub const ALL: [Self; 5] = [Self::Low, Self::Medium, Self::High, Self::Xhigh, Self::Max];

    /// 送受信とDBで使う値から読み取る。知らないeffortなら `None` を返す。
    pub fn from_value(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|effort| effort.value() == value)
    }

    /// 送受信とDBで使う値。CLIの引数にもそのまま渡す。
    pub fn value(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// 文字の規則を満たしたモデルの名前。
///
/// 英数字で始まり、英数字と `.`・`_`・`-`・`:`・`[`・`]` だけから成る1〜64文字。
/// 新しいモデルやエイリアスをリリースせずに使えるよう、一覧を持たずに文字の規則で確かめる。
/// `-` で始まらないため、CLIの1つの引数として渡しても、オプションとして読まれない。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Model(String);

impl Model {
    /// モデルの名前の最大の文字数。
    pub const MAX_LEN: usize = 64;

    /// 文字の規則を確かめて読み取る。
    pub fn parse(value: &str) -> Result<Self, InvalidAgentSetting> {
        let mut chars = value.chars();
        let starts_alphanumeric = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
        let allowed = chars
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '[' | ']'));
        // 許す文字はすべてASCIIのため、バイト数が文字数になる。
        if starts_alphanumeric && allowed && value.len() <= Self::MAX_LEN {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidAgentSetting::Model)
        }
    }

    /// モデルの名前。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 検証を通した、1つの枠のエージェントの設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSetting {
    /// エージェント。
    pub agent: Agent,
    /// モデル。`None` なら指定せず、CLIの既定値で動かす。
    pub model: Option<Model>,
    /// effort。`None` なら指定せず、CLIの既定値で動かす。
    pub effort: Option<Effort>,
}

impl AgentSetting {
    /// 送受信の形にする。
    pub fn to_entry(&self) -> AgentEntry {
        AgentEntry {
            agent: self.agent.value().to_owned(),
            model: self.model.as_ref().map(|model| model.as_str().to_owned()),
            effort: self.effort.map(|effort| effort.value().to_owned()),
        }
    }
}

/// 送受信の形の、1つの枠のエージェントの設定。値はまだ検証していない。
///
/// 版のずれたウェブアプリとディレクターの間でも枠ごとに検証できるよう、値を文字列のまま持つ。
/// 使う前に [`AgentEntry::validate`] を通す。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEntry {
    /// エージェント ([`Agent::value`] の値)。
    pub agent: String,
    /// モデル。指定しないときはキーを省く。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// effort ([`Effort::value`] の値)。指定しないときはキーを省く。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl AgentEntry {
    /// 値を検証し、検証を通した設定にする。
    pub fn validate(&self) -> Result<AgentSetting, InvalidAgentSetting> {
        let agent = Agent::from_value(&self.agent).ok_or(InvalidAgentSetting::Agent)?;
        let model = self.model.as_deref().map(Model::parse).transpose()?;
        let effort = self
            .effort
            .as_deref()
            .map(|effort| Effort::from_value(effort).ok_or(InvalidAgentSetting::Effort))
            .transpose()?;
        Ok(AgentSetting {
            agent,
            model,
            effort,
        })
    }
}

/// 設定の応答に載った、1つの枠の読み取り結果。
///
/// 値の形式が違う枠があっても、停止設定やほかの枠を読めるよう、形式の誤りは枠ごとに保持する。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, from = "Value")]
pub enum AgentEntryResult {
    /// 設定の形式に合う値。エージェント・モデル・effortの検証はこれから行う。
    Decoded(AgentEntry),
    /// 設定の形式に合わない値。
    Malformed(Value),
}

impl From<Value> for AgentEntryResult {
    fn from(value: Value) -> Self {
        if value.is_object()
            && let Ok(entry) = AgentEntry::deserialize(&value)
        {
            return Self::Decoded(entry);
        }
        Self::Malformed(value)
    }
}

impl AgentEntryResult {
    /// 形式を確かめ、エージェント・モデル・effortを検証する。
    pub fn validate(&self) -> Result<AgentSetting, InvalidAgentSetting> {
        match self {
            Self::Decoded(entry) => entry.validate(),
            Self::Malformed(_) => Err(InvalidAgentSetting::Format),
        }
    }
}

/// エージェントの設定の値が検証を通らないときのエラー。
///
/// 値はウェブアプリから来たデータのため、メッセージに含めない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidAgentSetting {
    /// 応答に載った枠が設定の形式に合わない。
    #[error("担当ごとのエージェントの設定の形式が正しくありません")]
    Format,
    /// 知らないエージェント。
    #[error("エージェントは claude か codex にしてください")]
    Agent,
    /// モデルが文字の規則に合わない。
    #[error("モデルは英数字で始め、英数字と . _ - : [ ] だけを使って64文字以内にしてください")]
    Model,
    /// 知らないeffort。
    #[error("effortは low・medium・high・xhigh・max のどれかにしてください")]
    Effort,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 枠とエージェントとeffortは値から読み戻せ_知らない値は読まない() {
        for slot in Slot::ALL {
            assert_eq!(
                Slot::from_value(slot.value()),
                Some(slot),
                "{slot:?} を読み戻せなかった"
            );
        }
        for agent in Agent::ALL {
            assert_eq!(
                Agent::from_value(agent.value()),
                Some(agent),
                "{agent:?} を読み戻せなかった"
            );
        }
        for effort in Effort::ALL {
            assert_eq!(
                Effort::from_value(effort.value()),
                Some(effort),
                "{effort:?} を読み戻せなかった"
            );
        }
        assert_eq!(Slot::from_value("watcher"), None, "知らない枠を読んだ");
        assert_eq!(Slot::from_value("Implementer"), None, "大文字の枠を読んだ");
        assert_eq!(
            Agent::from_value("gemini"),
            None,
            "知らないエージェントを読んだ"
        );
        assert_eq!(
            Effort::from_value("minimal"),
            None,
            "知らないeffortを読んだ"
        );
        assert_eq!(Effort::from_value(""), None, "空のeffortを読んだ");
    }

    #[test]
    fn 既定は作業者と偶数回目がclaude_codeで奇数回目がcodex_モデルとeffortは指定しない() {
        let agents: Vec<Agent> = Slot::ALL
            .into_iter()
            .map(|slot| slot.default_setting().agent)
            .collect();
        assert_eq!(
            agents,
            [Agent::Claude, Agent::Codex, Agent::Claude],
            "既定のエージェントが違う"
        );
        for slot in Slot::ALL {
            let setting = slot.default_setting();
            assert_eq!(setting.model, None, "{slot:?} の既定でモデルを指定した");
            assert_eq!(setting.effort, None, "{slot:?} の既定でeffortを指定した");
        }
    }

    #[test]
    fn モデルは文字の規則に合うものだけを受け付ける() {
        let longest = "a".repeat(Model::MAX_LEN);
        let too_long = "a".repeat(Model::MAX_LEN + 1);
        for (value, valid) in [
            ("claude-opus-5-5", true),
            ("opus", true),
            ("gpt-5.3-codex", true),
            ("claude-opus-5-5[1m]", true),
            ("us.anthropic.claude:0", true),
            ("5_model", true),
            (longest.as_str(), true),
            (too_long.as_str(), false),
            ("", false),
            ("-c", false),
            ("--model", false),
            (".hidden", false),
            ("[1m]", false),
            ("_model", false),
            ("model name", false),
            ("model=x", false),
            ("model/x", false),
            ("model\n", false),
            ("モデル", false),
            ("ａbc", false),
        ] {
            let got = Model::parse(value);
            if valid {
                assert_eq!(
                    got.as_ref().map(Model::as_str),
                    Ok(value),
                    "{value:?} を受け付けなかった"
                );
            } else {
                assert_eq!(
                    got,
                    Err(InvalidAgentSetting::Model),
                    "{value:?} を受け付けた"
                );
            }
        }
    }

    #[test]
    fn 送受信の形を検証すると型の付いた設定になり_送受信の形に戻せる() {
        let entry = AgentEntry {
            agent: "claude".to_owned(),
            model: Some("claude-opus-5-5".to_owned()),
            effort: Some("high".to_owned()),
        };

        let setting = entry.validate().expect("正しい設定を検証できなかった");

        assert_eq!(
            setting,
            AgentSetting {
                agent: Agent::Claude,
                model: Some(Model("claude-opus-5-5".to_owned())),
                effort: Some(Effort::High),
            },
            "検証した設定が違う"
        );
        assert_eq!(setting.to_entry(), entry, "送受信の形に戻せなかった");
    }

    #[test]
    fn 知らないエージェントや規則に合わない値は検証を通さない() {
        let entry = |agent: &str, model: Option<&str>, effort: Option<&str>| AgentEntry {
            agent: agent.to_owned(),
            model: model.map(str::to_owned),
            effort: effort.map(str::to_owned),
        };
        for (entry, expected) in [
            (entry("gemini", None, None), InvalidAgentSetting::Agent),
            (entry("Claude", None, None), InvalidAgentSetting::Agent),
            (entry("codex", Some("-c"), None), InvalidAgentSetting::Model),
            (entry("codex", Some(""), None), InvalidAgentSetting::Model),
            (
                entry("codex", None, Some("minimal")),
                InvalidAgentSetting::Effort,
            ),
            (entry("codex", None, Some("")), InvalidAgentSetting::Effort),
        ] {
            assert_eq!(
                entry.validate(),
                Err(expected),
                "{entry:?} の検証の結果が違う"
            );
        }
    }
}
