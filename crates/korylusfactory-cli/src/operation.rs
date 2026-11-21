//! 管制のウェブアプリから最後に読めた稼働と担当ごとのエージェントの設定を、状態の置き場のファイルに残す。
//!
//! `korylusfactory start` は、ウェブアプリから読んだ設定が前と変わったときに、読んだ時刻と一緒に書く。
//! 起動した直後にウェブアプリに届かないときはこのファイルの設定に従い、`korylusfactory status` は停止中かをここから出す。
//! ファイルを書くのは `korylusfactory start` だけで、書きかけのファイルが残らないよう、一時ファイルに書いてから置き換える。

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use korylusfactory_protocol::{AgentEntryResult, AgentSetting, Slot};
use serde::{Deserialize, Serialize};

/// 最後に読めた稼働の設定のファイルの、プロジェクトのルートからの相対パス。
pub const PATH: &str = "docs/private/factory/web-settings.json";

/// 書き込みの途中に使う一時ファイルの、プロジェクトのルートからの相対パス。
const TEMP_PATH: &str = "docs/private/factory/web-settings.json.tmp";

/// 最後に読めた稼働の設定のファイルを読み書きできないときのエラー。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// ファイルを読めない。
    #[error("最後に読めた稼働の設定 {} を読めません", .path.display())]
    Read {
        /// 読もうとしたファイル。
        path: PathBuf,
        /// 読めなかった理由。
        #[source]
        source: io::Error,
    },

    /// ファイルの中身を設定として読み取れない。
    #[error("最後に読めた稼働の設定 {} を読み取れません", .path.display())]
    Parse {
        /// 読もうとしたファイル。
        path: PathBuf,
        /// 読み取れなかった理由。
        #[source]
        source: serde_json::Error,
    },

    /// ファイルを書けない。
    #[error("最後に読めた稼働の設定 {} を書けません", .path.display())]
    Write {
        /// 書こうとしたファイル。
        path: PathBuf,
        /// 書けなかった理由。
        #[source]
        source: io::Error,
    },
}

/// 最後に読めた稼働と担当ごとのエージェントの設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    /// 停止していれば真。
    pub paused: bool,
    /// 担当ごとのエージェントのうち、検証を通した枠の設定。載っていない枠は既定として扱う。
    pub agents: BTreeMap<Slot, AgentSetting>,
    /// ウェブアプリからこの設定を読んだ時刻。
    pub read_at: Timestamp,
}

impl Saved {
    /// 枠slotの設定を返す。載っていなければ既定を返す。
    pub fn agent(&self, slot: Slot) -> AgentSetting {
        self.agents
            .get(&slot)
            .cloned()
            .unwrap_or_else(|| slot.default_setting())
    }
}

/// 枠slotの担当の呼び名。
pub fn slot_label(slot: Slot) -> &'static str {
    match slot {
        Slot::Implementer => "作業者",
        Slot::ReviewerOdd => "レビュアー (奇数回目)",
        Slot::ReviewerEven => "レビュアー (偶数回目)",
    }
}

/// ファイルに書く形。担当ごとのエージェントは、ウェブアプリの応答と同じ形で書く。
#[derive(Serialize, Deserialize)]
struct SavedFile {
    paused: bool,
    /// この項目の無いファイル (前の版のkorylusfactoryが書いたもの) は、すべての枠を既定として読む。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    agents: BTreeMap<String, AgentEntryResult>,
    /// RFC 3339のUTCの時刻。
    read_at: String,
}

/// プロジェクトのルートrootの、最後に読めた稼働の設定を読む。ファイルが無ければ `None` を返す。
pub fn load(root: &Path) -> Result<Option<Saved>, Error> {
    let path = root.join(PATH);
    let data = match fs::read(&path) {
        Ok(data) => data,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(Error::Read { path, source }),
    };
    let file: SavedFile = serde_json::from_slice(&data).map_err(|source| Error::Parse {
        path: path.clone(),
        source,
    })?;
    let read_at = file.read_at.parse().map_err(|_| Error::Parse {
        path: path.clone(),
        source: serde::de::Error::custom("read_at がRFC 3339の時刻ではありません"),
    })?;
    // ファイルはこのコマンドだけが検証を通した値で書くため、知らない枠と検証を通らない値は読み飛ばして既定にする。
    let agents = file
        .agents
        .iter()
        .filter_map(|(slot, entry)| Some((Slot::from_value(slot)?, entry.validate().ok()?)))
        .collect();
    Ok(Some(Saved {
        paused: file.paused,
        agents,
        read_at,
    }))
}

/// プロジェクトのルートrootに、最後に読めた稼働の設定savedを書く。
pub fn save(root: &Path, saved: &Saved) -> Result<(), Error> {
    let path = root.join(PATH);
    let write_error = |source| Error::Write {
        path: path.clone(),
        source,
    };
    let mut data = serde_json::to_vec_pretty(&SavedFile {
        paused: saved.paused,
        agents: saved
            .agents
            .iter()
            .map(|(slot, setting)| {
                (
                    slot.value().to_owned(),
                    AgentEntryResult::Decoded(setting.to_entry()),
                )
            })
            .collect(),
        read_at: saved.read_at.to_string(),
    })
    .map_err(|source| write_error(io::Error::other(source)))?;
    data.push(b'\n');
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(write_error)?;
    }
    let temp = root.join(TEMP_PATH);
    if let Err(source) = write_synced(&temp, &data).and_then(|()| fs::rename(&temp, &path)) {
        // 置き換えなかった一時ファイルは次の書き込みで上書きするため、消せなくても続けてよい。
        let _ = fs::remove_file(&temp);
        return Err(write_error(source));
    }
    Ok(())
}

/// pathにdataを書き、置き換えたあとに中身が失われないよう、ディスクに書き出すまで待つ。
fn write_synced(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(data)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(paused: bool) -> Saved {
        Saved {
            paused,
            agents: BTreeMap::new(),
            read_at: "2026-11-19T08:00:00Z".parse().expect("時刻を読めなかった"),
        }
    }

    #[test]
    fn 書いた設定を読み戻せる_ファイルが無ければ無いものとして読む() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れなかった");

        assert_eq!(
            load(dir.path()).expect("無いファイルを読めなかった"),
            None,
            "無いファイルを設定として読んだ"
        );
        save(dir.path(), &saved(true)).expect("設定を書けなかった");
        save(dir.path(), &saved(false)).expect("設定を書き直せなかった");

        assert_eq!(
            load(dir.path()).expect("書いた設定を読めなかった"),
            Some(saved(false)),
            "読み戻した設定が違う"
        );
        assert!(!dir.path().join(TEMP_PATH).exists(), "一時ファイルを残した");
    }

    #[test]
    fn 読み取れないファイルはエラーにする() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れなかった");
        let path = dir.path().join(PATH);
        fs::create_dir_all(path.parent().expect("親が無い")).expect("置き場を作れなかった");

        for broken in ["{", r#"{"paused": true, "read_at": "きのう"}"#] {
            fs::write(&path, broken).expect("ファイルを書けなかった");

            assert!(
                matches!(load(dir.path()), Err(Error::Parse { .. })),
                "読み取れないファイルを設定として読んだ"
            );
        }
    }

    #[test]
    fn 担当ごとのエージェントを応答と同じ形で書いて読み戻し_載っていない枠は既定にする() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れなかった");
        let setting = AgentSetting {
            agent: korylusfactory_protocol::Agent::Codex,
            model: Some(
                korylusfactory_protocol::Model::parse("gpt-5.3-codex")
                    .expect("モデルを読めなかった"),
            ),
            effort: Some(korylusfactory_protocol::Effort::High),
        };
        let mut written = saved(false);
        written.agents.insert(Slot::Implementer, setting.clone());

        save(dir.path(), &written).expect("設定を書けなかった");

        let text = fs::read_to_string(dir.path().join(PATH)).expect("ファイルを読めなかった");
        let value: serde_json::Value = serde_json::from_str(&text).expect("JSONではない");
        assert_eq!(
            value["agents"],
            serde_json::json!({"implementer": {"agent": "codex", "model": "gpt-5.3-codex", "effort": "high"}}),
            "担当ごとのエージェントの形が違う"
        );
        let got = load(dir.path())
            .expect("書いた設定を読めなかった")
            .expect("設定が無い");
        assert_eq!(got, written, "読み戻した設定が違う");
        assert_eq!(
            got.agent(Slot::Implementer),
            setting,
            "保存した枠の設定が違う"
        );
        assert_eq!(
            got.agent(Slot::ReviewerOdd),
            Slot::ReviewerOdd.default_setting(),
            "載っていない枠を既定にしなかった"
        );
    }

    #[test]
    fn 担当ごとのエージェントの無いファイルと検証を通らない枠は既定として読む() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れなかった");
        let path = dir.path().join(PATH);
        fs::create_dir_all(path.parent().expect("親が無い")).expect("置き場を作れなかった");

        for text in [
            r#"{"paused": false, "read_at": "2026-11-19T08:00:00Z"}"#,
            r#"{"paused": false, "agents": {"implementer": {"agent": "codex", "model": "-c"}, "reviewer_odd": [1], "watcher": {"agent": "claude"}}, "read_at": "2026-11-19T08:00:00Z"}"#,
        ] {
            fs::write(&path, text).expect("ファイルを書けなかった");

            let got = load(dir.path())
                .expect("ファイルを読めなかった")
                .expect("設定が無い");

            assert!(
                got.agents.is_empty(),
                "{text} の担当ごとのエージェントを既定として読まなかった"
            );
        }
    }
}
