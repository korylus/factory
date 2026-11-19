//! SQLiteのDBを開き、マイグレーションを流す。
//!
//! マイグレーションは [`MIGRATIONS`] に順に並べたSQLで、DBの `user_version` に流し終えた数を持つ。
//! 流したマイグレーションは書き換えず、変えるときは末尾に足す。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use rusqlite::{Connection, TransactionBehavior};

/// 流すマイグレーションのSQL。並べた順に流す。
pub const MIGRATIONS: &[&str] = &[
    // プロジェクトごとに、ディレクターから最後に受け取った状態の報告 (JSON) を1行で持つ。
    // 状態の正本は各VMの状態ファイルで、ここは表示のための写しのため、報告の中身を列に分けない。
    "CREATE TABLE project_statuses (
        project TEXT PRIMARY KEY NOT NULL,
        report TEXT NOT NULL,
        received_at TEXT NOT NULL
    ) STRICT",
];

/// DBを開けないか、DBの操作に失敗したときのエラー。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// DBのファイルを開けない。
    #[error("DB {} を開けません", .path.display())]
    Open {
        /// DBのファイルのパス。
        path: PathBuf,
        /// 開けなかった理由。
        #[source]
        source: rusqlite::Error,
    },

    /// DBが再起動後に残るファイルを開いていない。
    #[error("DB {} はファイルに残りません。保存先のファイルのパスを指定してください", .0.display())]
    NoPersistentFile(PathBuf),

    /// マイグレーションを流せない。
    #[error("DBのマイグレーション {number} 番を流せません")]
    Migrate {
        /// 流せなかったマイグレーションの番号 (1始まり)。
        number: usize,
        /// 流せなかった理由。
        #[source]
        source: rusqlite::Error,
    },

    /// DBが、このウェブアプリの知らないマイグレーションまで流してある。
    #[error(
        "DBは {applied} 番までマイグレーションを流してありますが、このウェブアプリは {known} 番までしか知りません。新しいウェブアプリを入れてください"
    )]
    Newer {
        /// DBに流してあるマイグレーションの数。
        applied: usize,
        /// このウェブアプリが持つマイグレーションの数。
        known: usize,
    },

    /// DBの操作に失敗した。
    #[error("DBの操作に失敗しました")]
    Query(#[source] rusqlite::Error),

    /// DBの操作の途中で、操作を走らせたスレッドが止まった。
    #[error("DBの操作の途中で処理が止まりました")]
    Interrupted,
}

/// ウェブアプリのDB。接続を1つ持ち、操作を順に行う。
///
/// 書き込むのはディレクターからの状態の報告だけで、要求の数も少ないため、接続を分けずに1つで足りる。
#[derive(Debug, Clone)]
pub struct Database {
    connection: Arc<Mutex<Connection>>,
}

impl Database {
    /// DBのファイルを開き、まだ流していないマイグレーションを流す。ファイルが無ければ作る。
    pub fn open(path: &Path) -> Result<Self, Error> {
        let open_error = |source| Error::Open {
            path: path.to_path_buf(),
            source,
        };
        let mut connection = Connection::open(path).map_err(open_error)?;
        let has_file: bool = connection
            .query_row(
                "SELECT file <> '' FROM pragma_database_list WHERE name = 'main'",
                [],
                |row| row.get(0),
            )
            .map_err(open_error)?;
        if !has_file {
            return Err(Error::NoPersistentFile(path.to_path_buf()));
        }
        // ほかの接続 (sqlite3のコマンドで中を覗くときなど) と同時に開いても、読み書きが待ち合わせられるようにする。
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(open_error)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(open_error)?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(open_error)?;
        migrate(&mut connection, MIGRATIONS)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// DBの接続を使う処理を、非同期の処理を止めないよう別のスレッドで行う。
    pub async fn call<T, F>(&self, f: F) -> Result<T, Error>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, rusqlite::Error> + Send + 'static,
    {
        let connection = Arc::clone(&self.connection);
        tokio::task::spawn_blocking(move || {
            // 接続を使う処理がパニックしても、接続そのものは壊れていないため使い続ける。
            let mut connection = connection.lock().unwrap_or_else(PoisonError::into_inner);
            f(&mut connection).map_err(Error::Query)
        })
        .await
        .map_err(|_| Error::Interrupted)?
    }
}

/// まだ流していないマイグレーションを、1つずつトランザクションの中で流す。
fn migrate(connection: &mut Connection, migrations: &[&str]) -> Result<(), Error> {
    loop {
        // 待っている間に別の接続が適用することがあるため、書き込みロックを取ってから版を読む。
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(Error::Query)?;
        let applied: i64 = transaction
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(Error::Query)?;
        // user_versionは、このウェブアプリが流した数だけを書くため、負の値にはならない。
        let applied = usize::try_from(applied).unwrap_or(usize::MAX);
        if applied > migrations.len() {
            return Err(Error::Newer {
                applied,
                known: migrations.len(),
            });
        }
        let Some(sql) = migrations.get(applied) else {
            return transaction.rollback().map_err(Error::Query);
        };

        let number = applied + 1;
        let migrate_error = |source| Error::Migrate { number, source };
        transaction.execute_batch(sql).map_err(migrate_error)?;
        let version = i64::try_from(number).expect("マイグレーションの数はi64に収まる");
        transaction
            .pragma_update(None, "user_version", version)
            .map_err(migrate_error)?;
        transaction.commit().map_err(migrate_error)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_version(connection: &Connection) -> i64 {
        connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("user_versionを読めなかった")
    }

    fn tables(connection: &Connection) -> Vec<String> {
        let mut statement = connection
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
            .expect("テーブルの一覧を読めなかった");
        statement
            .query_map([], |row| row.get(0))
            .expect("テーブルの一覧を読めなかった")
            .collect::<Result<_, _>>()
            .expect("テーブルの名前を読めなかった")
    }

    #[test]
    fn まだ流していないマイグレーションだけを順に流す() {
        let mut connection = Connection::open_in_memory().expect("DBを開けなかった");
        migrate(&mut connection, &["CREATE TABLE a (id INTEGER)"]).expect("1つ目を流せなかった");

        migrate(
            &mut connection,
            &["CREATE TABLE a (id INTEGER)", "CREATE TABLE b (id INTEGER)"],
        )
        .expect("2つ目を流せなかった");

        assert_eq!(user_version(&connection), 2, "流した数を記録していない");
        assert_eq!(tables(&connection), ["a", "b"], "流したテーブルが違う");
    }

    #[test]
    fn ロックを待つ間に別の接続で流したマイグレーションは流し直さない() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れなかった");
        let path = dir.path().join("factory.sqlite3");
        let first = Connection::open(&path).expect("最初の接続を開けなかった");
        let mut second = Connection::open(&path).expect("次の接続を開けなかった");

        first
            .execute_batch("BEGIN IMMEDIATE; CREATE TABLE a (id INTEGER); PRAGMA user_version = 1;")
            .expect("最初の接続でマイグレーションを流せなかった");
        // 2つ目の接続がロックに当たった時点でコミットし、競合の順序を確定させる。
        second
            .busy_handler(Some(move |attempt| {
                attempt == 0 && first.execute_batch("COMMIT").is_ok()
            }))
            .expect("ロック待ちの処理を登録できなかった");

        migrate(
            &mut second,
            &["CREATE TABLE a (id INTEGER)", "CREATE TABLE b (id INTEGER)"],
        )
        .expect("ロック待ちのあとでマイグレーションを流せなかった");

        assert_eq!(user_version(&second), 2, "流した数を記録していない");
        assert_eq!(tables(&second), ["a", "b"], "流したテーブルが違う");
    }

    #[test]
    fn 失敗したマイグレーションは取り消して止める() {
        let mut connection = Connection::open_in_memory().expect("DBを開けなかった");

        let result = migrate(
            &mut connection,
            &[
                "CREATE TABLE a (id INTEGER)",
                "CREATE TABLE b (id INTEGER); 誤り",
            ],
        );

        assert!(
            matches!(result, Err(Error::Migrate { number: 2, .. })),
            "2番の失敗として止めなかった: {result:?}"
        );
        assert_eq!(
            user_version(&connection),
            1,
            "失敗したマイグレーションを流したことにした"
        );
        assert_eq!(
            tables(&connection),
            ["a"],
            "失敗したマイグレーションの途中の変更が残った"
        );
    }

    #[test]
    fn 知らないマイグレーションまで流してあるdbは開かない() {
        let mut connection = Connection::open_in_memory().expect("DBを開けなかった");
        migrate(&mut connection, &["SELECT 1", "SELECT 2"])
            .expect("マイグレーションを流せなかった");

        let result = migrate(&mut connection, &["SELECT 1"]);

        assert!(
            matches!(
                result,
                Err(Error::Newer {
                    applied: 2,
                    known: 1
                })
            ),
            "新しいDBを開いた: {result:?}"
        );
    }

    #[tokio::test]
    async fn 別のスレッドで保存した値はdbを開き直しても読める() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れなかった");
        let path = dir.path().join("factory.sqlite3");
        let database = Database::open(&path).expect("DBを開けなかった");
        database
            .call(|connection| {
                connection.execute_batch(
                    "CREATE TABLE sample (id INTEGER); INSERT INTO sample VALUES (1);",
                )
            })
            .await
            .expect("DBに値を保存できなかった");
        drop(database);

        let reopened = Database::open(&path).expect("DBを開き直せなかった");
        let got = reopened
            .call(|connection| {
                connection.query_row("SELECT id FROM sample", [], |row| row.get::<_, i64>(0))
            })
            .await
            .expect("保存した値を読めなかった");

        assert_eq!(got, 1, "保存した値が違う");
    }

    #[test]
    fn 終了すると消えるdbは開けない() {
        for path in ["", ":memory:", "file:factory?mode=memory"] {
            let result = Database::open(Path::new(path));

            assert!(
                matches!(result, Err(Error::NoPersistentFile(_))),
                "終了すると消えるDBを開いた: {path:?}: {result:?}"
            );
        }
    }

    #[test]
    fn 置き場のディレクトリが無いdbは開けない() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れなかった");

        let result = Database::open(&dir.path().join("missing").join("factory.sqlite3"));

        assert!(
            matches!(result, Err(Error::Open { .. })),
            "開けないDBを開いた: {result:?}"
        );
    }
}
