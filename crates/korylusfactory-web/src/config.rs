//! ウェブアプリの設定ファイルの読み取り。
//!
//! 設定ファイルはJSONで、`--config` でパスを渡す。
//!
//! ```json
//! {
//!   "listen": "0.0.0.0:8000",
//!   "database": "/var/lib/korylusfactory-web/factory.sqlite3",
//!   "owners": ["owner@example.com"],
//!   "directors": [{"vm": "annict-factory", "project": "Annict"}]
//! }
//! ```
//!
//! `listen` は省略すると [`DEFAULT_LISTEN`] で待ち受ける。
//! `database` の相対パスは、設定ファイルの置き場ではなく、起動したときの作業ディレクトリから解決する。
//! systemdなどで起動する本番では、作業ディレクトリによらず同じファイルを指すよう絶対パスを書く。
//! `owners` は画面を見られる人のメールアドレスで、1つ以上書く。
//! `directors` はディレクターとして状態を送れるVMと、そのVMのプロジェクトの対応で、省略すると誰も送れない。
//! `dev_identity` は開発環境でだけ書き、すべての要求をその身元からのものとみなす (`{"email": ...}` か `{"vm": ...}`)。
//! 開発用の設定例は `config.dev.json` に、管制のVMで常駐させるときの見本は `deploy/config.example.json` に置く。

use std::collections::HashSet;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// 待ち受けるアドレスの既定。exe.devのVMが既定で公開するポートにする。
pub const DEFAULT_LISTEN: &str = "0.0.0.0:8000";

/// 設定ファイルを読み取れないときのエラー。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 設定ファイルを読めない。
    #[error("設定ファイル {} を読めません", .path.display())]
    Read {
        /// 設定ファイルのパス。
        path: PathBuf,
        /// 読めなかった理由。
        #[source]
        source: io::Error,
    },

    /// 設定ファイルがJSONとして読めないか、項目が誤っている。
    #[error("設定ファイル {} の書き方が誤っています", .path.display())]
    Parse {
        /// 設定ファイルのパス。
        path: PathBuf,
        /// 誤っている箇所。
        #[source]
        source: serde_json::Error,
    },

    /// 画面を見られる人がいない。
    #[error(
        "設定ファイル {} の「owners」が空です。画面を見る人のメールアドレスを1つ以上書いてください",
        .0.display()
    )]
    NoOwners(PathBuf),

    /// DBのパス・メールアドレス・VM名・プロジェクト名に空のものがある。
    #[error("設定ファイル {} の「{field}」に空の値があります", .path.display())]
    Empty {
        /// 設定ファイルのパス。
        path: PathBuf,
        /// 空の値があった項目。
        field: &'static str,
    },

    /// 同じVM名か同じプロジェクト名が、ディレクターに2回以上ある。
    #[error(
        "設定ファイル {} の「directors」に、{field} {value:?} が2回以上あります。1つのVMと1つのプロジェクトを1対1で対応付けてください",
        .path.display()
    )]
    DuplicateDirector {
        /// 設定ファイルのパス。
        path: PathBuf,
        /// 重なった項目 (VM名かプロジェクト名)。
        field: &'static str,
        /// 重なった値。
        value: String,
    },
}

/// ウェブアプリの設定。
#[derive(Debug, Clone)]
pub struct Config {
    /// 待ち受けるアドレス。
    pub listen: SocketAddr,
    /// SQLiteのDBのファイルのパス。
    pub database: PathBuf,
    /// 画面を見られる人のメールアドレス。
    pub owners: Vec<String>,
    /// 状態を送れるディレクター。
    pub directors: Vec<Director>,
    /// 開発環境で、すべての要求の主とみなす身元。
    pub dev_identity: Option<DevIdentity>,
}

/// 状態を送れるディレクターと、そのプロジェクト。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Director {
    /// ディレクターが動くVMの名前。exe.devが `X-Exedev-Source-Vm` に付ける値と比べる。
    pub vm: String,
    /// そのVMのプロジェクトの名前。
    pub project: String,
}

/// 開発環境で、exe.devのヘッダーの代わりに使う身元。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DevIdentity {
    /// 人間として、このメールアドレスでログインしているものとみなす。
    Email(String),
    /// ディレクターとして、このVMから呼ばれたものとみなす。
    Vm(String),
}

/// 設定ファイルのJSONの形。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    listen: Option<SocketAddr>,
    database: PathBuf,
    owners: Vec<String>,
    #[serde(default)]
    directors: Vec<Director>,
    dev_identity: Option<DevIdentity>,
}

impl Config {
    /// 設定ファイルを読み取る。
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(path, &text)
    }

    /// 設定ファイルの中身を読み取る。`path` はエラーのメッセージに使う。
    pub fn parse(path: &Path, text: &str) -> Result<Self, Error> {
        let file: File = serde_json::from_str(text).map_err(|source| Error::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        let empty = |field| Error::Empty {
            path: path.to_path_buf(),
            field,
        };
        if file.database.as_os_str().is_empty() {
            return Err(empty("database"));
        }
        if file.owners.is_empty() {
            return Err(Error::NoOwners(path.to_path_buf()));
        }
        if file.owners.iter().any(|owner| owner.trim().is_empty()) {
            return Err(empty("owners"));
        }
        let mut vms = HashSet::new();
        let mut projects = HashSet::new();
        for director in &file.directors {
            if director.vm.trim().is_empty() {
                return Err(empty("directors"));
            }
            if director.project.trim().is_empty() {
                return Err(empty("directors"));
            }
            let duplicate = |field, value: &str| Error::DuplicateDirector {
                path: path.to_path_buf(),
                field,
                value: value.to_owned(),
            };
            if !vms.insert(director.vm.as_str()) {
                return Err(duplicate("VM名", &director.vm));
            }
            if !projects.insert(director.project.as_str()) {
                return Err(duplicate("プロジェクト名", &director.project));
            }
        }
        if let Some(DevIdentity::Email(value) | DevIdentity::Vm(value)) = &file.dev_identity
            && value.trim().is_empty()
        {
            return Err(empty("dev_identity"));
        }
        Ok(Self {
            listen: file.listen.unwrap_or_else(|| {
                DEFAULT_LISTEN
                    .parse()
                    .expect("既定の待ち受けのアドレスは正しいアドレスである")
            }),
            database: file.database,
            owners: file.owners,
            directors: file.directors,
            dev_identity: file.dev_identity,
        })
    }

    /// メールアドレスが、画面を見られる人のものかを返す。
    ///
    /// メールアドレスの大文字と小文字は区別しない。設定に書いた表記とexe.devが付ける表記が違っても、同じ人とみなすため。
    pub fn is_owner(&self, email: &str) -> bool {
        self.owners
            .iter()
            .any(|owner| owner.eq_ignore_ascii_case(email))
    }

    /// VM名に対応するディレクターを返す。
    pub fn director(&self, vm: &str) -> Option<&Director> {
        self.directors.iter().find(|director| director.vm == vm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, Error> {
        Config::parse(Path::new("config.json"), text)
    }

    #[test]
    fn 常駐させるときの設定の見本を読み取れる() {
        let config = parse(include_str!("../deploy/config.example.json"))
            .expect("設定の見本を読み取れなかった");

        assert_eq!(
            config.listen,
            DEFAULT_LISTEN
                .parse()
                .expect("既定の待ち受けを読めなかった"),
            "見本が既定のポートで待ち受けない"
        );
        assert!(
            config.database.is_absolute(),
            "見本のDBのパスが絶対パスではない"
        );
        assert_eq!(config.dev_identity, None, "見本で開発用の身元を有効にした");
    }

    #[test]
    fn すべての項目を読み取れる() {
        let config = parse(
            r#"{
                "listen": "127.0.0.1:9000",
                "database": "/tmp/factory.sqlite3",
                "owners": ["owner@example.com"],
                "directors": [{"vm": "annict-factory", "project": "Annict"}],
                "dev_identity": {"vm": "annict-factory"}
            }"#,
        )
        .expect("設定を読み取れなかった");

        assert_eq!(
            config.listen.to_string(),
            "127.0.0.1:9000",
            "待ち受けのアドレスが違う"
        );
        assert_eq!(
            config.database,
            PathBuf::from("/tmp/factory.sqlite3"),
            "DBのパスが違う"
        );
        assert_eq!(
            config.owners,
            ["owner@example.com"],
            "画面を見られる人が違う"
        );
        assert_eq!(
            config.directors,
            [Director {
                vm: "annict-factory".to_owned(),
                project: "Annict".to_owned()
            }],
            "ディレクターが違う"
        );
        assert_eq!(
            config.dev_identity,
            Some(DevIdentity::Vm("annict-factory".to_owned())),
            "開発用の身元が違う"
        );
    }

    #[test]
    fn 省略した項目は既定の値になる() {
        let config = parse(r#"{"database": "factory.sqlite3", "owners": ["owner@example.com"]}"#)
            .expect("設定を読み取れなかった");

        assert_eq!(
            config.listen.to_string(),
            DEFAULT_LISTEN,
            "既定の待ち受けのアドレスにならなかった"
        );
        assert!(
            config.directors.is_empty(),
            "ディレクターが空にならなかった"
        );
        assert_eq!(
            config.dev_identity, None,
            "開発用の身元が無しにならなかった"
        );
    }

    #[test]
    fn 知らない項目があると誤りにする() {
        let result = parse(r#"{"database": "a", "owners": ["owner@example.com"], "owner": "x"}"#);

        assert!(
            matches!(result, Err(Error::Parse { .. })),
            "書き誤りとして扱わなかった: {result:?}"
        );
    }

    #[test]
    fn 画面を見られる人がいないと誤りにする() {
        let result = parse(r#"{"database": "a", "owners": []}"#);

        assert!(
            matches!(result, Err(Error::NoOwners(_))),
            "空の owners を受け付けた: {result:?}"
        );
    }

    #[test]
    fn dbのパスが空なら誤りにする() {
        let result = parse(r#"{"database": "", "owners": ["owner@example.com"]}"#);

        assert!(
            matches!(
                result,
                Err(Error::Empty {
                    field: "database",
                    ..
                })
            ),
            "空のDBのパスを受け付けた: {result:?}"
        );
    }

    #[test]
    fn 空の値があると誤りにする() {
        for text in [
            r#"{"database": "a", "owners": [" "]}"#,
            r#"{"database": "a", "owners": ["o@example.com"], "directors": [{"vm": "", "project": "A"}]}"#,
            r#"{"database": "a", "owners": ["o@example.com"], "directors": [{"vm": "a", "project": ""}]}"#,
            r#"{"database": "a", "owners": ["o@example.com"], "dev_identity": {"email": ""}}"#,
        ] {
            let result = parse(text);

            assert!(
                matches!(result, Err(Error::Empty { .. })),
                "空の値を受け付けた: {text}"
            );
        }
    }

    #[test]
    fn ディレクターのvm名かプロジェクト名が重なると誤りにする() {
        for (directors, field) in [
            (
                r#"[{"vm": "a", "project": "A"}, {"vm": "a", "project": "B"}]"#,
                "VM名",
            ),
            (
                r#"[{"vm": "a", "project": "A"}, {"vm": "b", "project": "A"}]"#,
                "プロジェクト名",
            ),
        ] {
            let result = parse(&format!(
                r#"{{"database": "a", "owners": ["o@example.com"], "directors": {directors}}}"#
            ));

            assert!(
                matches!(&result, Err(Error::DuplicateDirector { field: got, .. }) if *got == field),
                "{field}の重なりを誤りにしなかった: {result:?}"
            );
        }
    }

    #[test]
    fn 画面を見られる人はメールアドレスの大文字と小文字を区別せずに見分ける() {
        let config = parse(r#"{"database": "a", "owners": ["Owner@Example.com"]}"#)
            .expect("設定を読み取れなかった");

        assert!(
            config.is_owner("owner@example.com"),
            "表記の違う同じアドレスを見分けられなかった"
        );
        assert!(
            !config.is_owner("other@example.com"),
            "別のアドレスを画面を見られる人とみなした"
        );
    }
}
