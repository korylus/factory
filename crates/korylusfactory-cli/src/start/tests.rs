//! 監督のループのテスト。
//!
//! 作業計画書とロードマップは、wikinoに答えるRunnerの差し替えで渡し、gitは一時ディレクトリのリポジトリで本物を実行する。
//! セッションは、決めた順に作業 (コミット・作業計画書のチェック・レビュードキュメントの作成) をして結果を返す偽物で起動する。
//! 時計は、待つ時間を記録するだけで実際には待たず、決めた回数だけ待ったら取り消してループを終わらせる。

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use korylusfactory_core::flow::RETRY_LIMIT;
use korylusfactory_core::state::{
    ItemPage, Notice, NoticeKind, Notified, PlanDir, QueuedItem, Reviewer, SessionResult, Step,
    Verdict, WaitItem,
};
use serde_json::{Value, json};

use super::Clock as _;
use super::*;
use crate::branch::testutil::{Repo, git_run, repo_on, write_file};
use crate::human_queue::{self, Listed, Location, Status};
use crate::notify::{self, Message, Notifier};
use crate::process::{self, Ended, Output, Session, System};
use crate::web;

/// 作業計画書のページ番号。
const PLAN_PAGE: u64 = 20;
const PLAN_URL: &str = "https://wikino.app/s/shimbaco/pages/20";
const TITLE: &str = "デモ 作業計画書";

/// wikinoの呼び出しに、持っているページで答えるRunner。gitは本物を実行する。
///
/// 下書きの一覧とページの取得だけに答え、ページはすべて下書きとして返す。
struct Wiki {
    pages: RefCell<HashMap<u64, String>>,
}

impl Wiki {
    fn page(&self, number: u64) -> String {
        self.pages
            .borrow()
            .get(&number)
            .cloned()
            .expect("ページが無い")
    }

    fn set(&self, number: u64, body: String) {
        self.pages.borrow_mut().insert(number, body);
    }

    /// 作業計画書のタスクidにチェックを付ける。
    fn check(&self, id: &str) {
        let body = self
            .page(PLAN_PAGE)
            .replace(&format!("- [ ] **{id}**"), &format!("- [x] **{id}**"));
        self.set(PLAN_PAGE, body);
    }
}

impl Runner for Wiki {
    fn run(&self, program: &str, args: &[OsString]) -> Result<Output, process::Error> {
        if program == "git" {
            return System.run(program, args);
        }
        assert_eq!(program, "wikino", "想定しないコマンドを実行した");
        let args: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let ok = |stdout: String| Output {
            stdout: stdout.into_bytes(),
            ..Output::default()
        };
        Ok(match (args[0].as_str(), args[1].as_str()) {
            ("draft", "list") => ok(format!(
                "{{\"page_number\":{PLAN_PAGE},\"title\":\"{TITLE}\",\"topic_number\":4,\"topic_name\":\"Mewst\"}}\n"
            )),
            ("page", "list") => ok(String::new()),
            ("draft", "get") => {
                let number: u64 = args[2].parse().expect("ページ番号を読み取れなかった");
                match self.pages.borrow().get(&number) {
                    Some(body) => ok(format!(
                        "---\nwikino_number: {number}\nwikino_topic_number: 4\n---\n\n{body}"
                    )),
                    None => Output {
                        code: 5,
                        ..Output::default()
                    },
                }
            }
            _ => panic!("想定しないwikinoの引数: {args:?}"),
        })
    }
}

/// セッションが行う作業。結果のJSONを返す。`None` を返すと、結果を返さずに終了コード1で終わる。
type Work<'a> = Box<dyn FnOnce(&Wiki) -> Option<Value> + 'a>;

/// 決めた順に作業をして結果を返すLauncher。渡された指示を記録する。
struct Sessions<'a> {
    wiki: &'a Wiki,
    works: RefCell<VecDeque<Work<'a>>>,
    prompts: RefCell<Vec<String>>,
    /// 起動したコマンドと引数。
    commands: RefCell<Vec<(String, Vec<String>)>>,
}

impl<'a> Sessions<'a> {
    fn new(wiki: &'a Wiki, works: Vec<Work<'a>>) -> Self {
        Self {
            wiki,
            works: RefCell::new(works.into()),
            prompts: RefCell::new(Vec::new()),
            commands: RefCell::new(Vec::new()),
        }
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts.borrow().clone()
    }

    fn commands(&self) -> Vec<(String, Vec<String>)> {
        self.commands.borrow().clone()
    }
}

impl Launcher for Sessions<'_> {
    fn launch(&self, session: &Session, cancel: &AtomicBool) -> Result<Ended, process::Error> {
        self.prompts
            .borrow_mut()
            .push(String::from_utf8(session.input.clone()).expect("指示がUTF-8ではない"));
        self.commands.borrow_mut().push((
            session.program.clone(),
            session
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
        ));
        let work = self
            .works
            .borrow_mut()
            .pop_front()
            .expect("決めた数より多くセッションを起動した");
        let result = work(self.wiki);
        if cancel.load(Ordering::SeqCst) {
            return Ok(Ended::Cancelled);
        }
        let Some(result) = result else {
            return Ok(Ended::Exited(1));
        };
        let logs = session.stdout.parent().expect("記録のディレクトリが無い");
        match session.program.as_str() {
            "claude" => fs::write(
                &session.stdout,
                json!({"is_error": false, "structured_output": result}).to_string(),
            ),
            "codex" => fs::write(logs.join("result.json"), result.to_string()),
            program => panic!("想定しないエージェント: {program}"),
        }
        .expect("結果を書けなかった");
        Ok(Ended::Exited(0))
    }
}

/// 人間キューを使わない設定のテストで渡す置き場。開かれたらテストを失敗させる。
struct NoQueue;

impl Notifier for NoQueue {
    fn notify(
        &self,
        _command: &config::Notify,
        _message: &Message,
        _timeout: Duration,
    ) -> Result<(), notify::Error> {
        panic!("通知のコマンドを使わない設定で、通知のコマンドを実行した")
    }
}

impl Open for NoQueue {
    fn open<'a>(&'a self, _location: &'a Location) -> Box<dyn human_queue::Store + 'a> {
        panic!("人間キューを使わない設定で、置き場を開いた")
    }
}

/// 偽物の置き場のページ1つ。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Page {
    /// 下書きがある。
    Draft { title: String, body: String },
    /// 公開済みで、下書きが無い。
    Published { title: String, body: String },
    /// 下書きを削除した。
    Deleted,
}

/// 人間キューの置き場の偽物。ページを番号ごとに持ち、呼ばれた操作を記録する。
#[derive(Default)]
struct Queue {
    pages: RefCell<BTreeMap<u64, Page>>,
    calls: RefCell<Vec<String>>,
    /// 作成を送ったときに、作ったうえで結果が分からなかったことにする回数。
    unconfirmed: Cell<usize>,
    /// 作成結果が分からない間に、人間がその項目を公開したことにする。
    publish_unknown: Cell<bool>,
    /// 次の読み取りだけを失敗させるページ番号。
    fail_read: Cell<Option<u64>>,
    /// 通知のコマンドに渡した内容。通知のコマンドの偽物も兼ねる。
    notes: RefCell<Vec<Message>>,
    /// 次の通知だけに返すエラー。通知の失敗で監督のループが止まらないことを確かめる。
    notify_error: RefCell<Option<notify::Error>>,
}

impl Queue {
    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    fn page(&self, number: u64) -> Page {
        self.pages
            .borrow()
            .get(&number)
            .cloned()
            .expect("ページが無い")
    }

    fn insert(&self, number: u64, page: Page) {
        self.pages.borrow_mut().insert(number, page);
    }

    fn call(&self, call: String) {
        self.calls.borrow_mut().push(call);
    }
}

impl Queue {
    fn notes(&self) -> Vec<Message> {
        self.notes.borrow().clone()
    }
}

impl Notifier for Queue {
    fn notify(
        &self,
        command: &config::Notify,
        message: &Message,
        _timeout: Duration,
    ) -> Result<(), notify::Error> {
        assert_eq!(
            command.program, "notify-test",
            "設定と違う通知のコマンドを実行した"
        );
        self.notes.borrow_mut().push(message.clone());
        if let Some(err) = self.notify_error.borrow_mut().take() {
            return Err(err);
        }
        Ok(())
    }
}

impl Open for Queue {
    fn open<'a>(&'a self, location: &'a Location) -> Box<dyn human_queue::Store + 'a> {
        assert_eq!(
            location,
            &Location {
                space: "shimbaco".to_owned(),
                topic: 12,
            },
            "設定と違う置き場を開いた"
        );
        Box::new(QueueStore(self))
    }
}

struct QueueStore<'a>(&'a Queue);

impl human_queue::Store for QueueStore<'_> {
    fn create(&self, title: &str, body: &str) -> Result<u64, human_queue::Error> {
        let queue = self.0;
        queue.call(format!("create {title}"));
        let number = queue.pages.borrow().keys().last().map_or(1, |n| n + 1);
        queue.insert(
            number,
            Page::Draft {
                title: title.to_owned(),
                body: body.to_owned(),
            },
        );
        if queue.unconfirmed.get() > 0 {
            queue.unconfirmed.set(queue.unconfirmed.get() - 1);
            if queue.publish_unknown.get() {
                queue.insert(
                    number,
                    Page::Published {
                        title: title.to_owned(),
                        body: body.to_owned(),
                    },
                );
            }
            return Err(human_queue::Error::Unconfirmed {
                title: title.to_owned(),
                source: Box::new(human_queue::Error::NoPage(number)),
            });
        }
        Ok(number)
    }

    fn list(&self) -> Result<Vec<Listed>, human_queue::Error> {
        self.0.call("list".to_owned());
        Ok(self
            .0
            .pages
            .borrow()
            .iter()
            .filter_map(|(number, page)| match page {
                Page::Draft { title, .. } => Some(Listed {
                    number: *number,
                    title: title.clone(),
                }),
                Page::Published { .. } | Page::Deleted => None,
            })
            .collect())
    }

    fn read(&self, number: u64) -> Result<Status, human_queue::Error> {
        self.0.call(format!("read {number}"));
        if self.0.fail_read.get() == Some(number) {
            self.0.fail_read.set(None);
            return Err(human_queue::Error::Changed(number));
        }
        Ok(match self.0.pages.borrow().get(&number) {
            Some(Page::Draft { title, body }) => Status::Drafted {
                title: Some(title.clone()),
                body: body.clone(),
                etag: format!("v{number}"),
            },
            Some(Page::Published { title, body }) => Status::Published {
                title: Some(title.clone()),
                body: body.clone(),
            },
            Some(Page::Deleted) | None => Status::Missing,
        })
    }

    fn delete(&self, number: u64, etag: &str) -> Result<(), human_queue::Error> {
        self.0.call(format!("delete {number}"));
        assert_eq!(etag, format!("v{number}"), "読んだ版で削除していない");
        self.0.insert(number, Page::Deleted);
        Ok(())
    }

    fn reopen(&self, number: u64, body: &str) -> Result<(), human_queue::Error> {
        self.0.call(format!("reopen {number}"));
        let Page::Published { title, .. } = self.0.page(number) else {
            panic!("公開済みでない項目 {number} を差し戻した");
        };
        self.0.insert(
            number,
            Page::Draft {
                title,
                body: body.to_owned(),
            },
        );
        Ok(())
    }

    fn update(&self, _number: u64, _body: &str, _etag: &str) -> Result<(), human_queue::Error> {
        panic!("監督のループが項目の下書きを書き換えた")
    }

    fn find(&self, title: &str) -> Result<Option<u64>, human_queue::Error> {
        if let Some(item) = self.list()?.into_iter().find(|item| item.title == title) {
            return Ok(Some(item.number));
        }
        self.0.call("pages".to_owned());
        Ok(self
            .0
            .pages
            .borrow()
            .iter()
            .find_map(|(&number, page)| match page {
                Page::Published {
                    title: page_title, ..
                } if page_title == title => Some(number),
                _ => None,
            }))
    }
}

/// 待つ時間を記録するだけで実際には待たず、決めた回数だけ待ったら取り消す時計。
struct Clock<'a> {
    now: Cell<Timestamp>,
    sleeps: RefCell<Vec<Duration>>,
    stop_after: usize,
    cancel: &'a AtomicBool,
    /// 最初の待ちの間に、人間の操作などを再現する処理。
    on_sleep: RefCell<Option<Box<dyn FnOnce() + 'a>>>,
}

impl<'a> Clock<'a> {
    fn new(stop_after: usize, cancel: &'a AtomicBool) -> Self {
        Self {
            now: Cell::new(
                "2026-11-16T01:00:00Z"
                    .parse()
                    .expect("日時を読み取れなかった"),
            ),
            sleeps: RefCell::new(Vec::new()),
            stop_after,
            cancel,
            on_sleep: RefCell::new(None),
        }
    }

    fn sleeps(&self) -> Vec<Duration> {
        self.sleeps.borrow().clone()
    }

    /// 最初に待つときだけworkを実行する。
    fn on_first_sleep(&self, work: impl FnOnce() + 'a) {
        *self.on_sleep.borrow_mut() = Some(Box::new(work));
    }
}

impl super::Clock for Clock<'_> {
    fn now(&self) -> Timestamp {
        self.now.get()
    }

    fn sleep(&self, duration: Duration) {
        let duration = SignedDuration::try_from(duration).expect("待つ時間が長すぎる");
        self.now.set(
            self.now
                .get()
                .checked_add(duration)
                .expect("日時を進められなかった"),
        );
        let mut sleeps = self.sleeps.borrow_mut();
        sleeps.push(duration.unsigned_abs());
        if sleeps.len() >= self.stop_after {
            self.cancel.store(true, Ordering::SeqCst);
        }
        drop(sleeps);
        if let Some(work) = self.on_sleep.borrow_mut().take() {
            work();
        }
    }
}

/// 設定ファイルを置いたプロジェクト。プロジェクトのルートが、タスクのリポジトリを兼ねる。
fn project() -> Repo {
    let repo = repo_on("main");
    write_file(
        &repo.root,
        config::CONFIG_PATH,
        br#"{"roadmap": "https://wikino.app/s/shimbaco/pages/1214"}"#,
    );
    repo
}

/// 承認とみなす節「やってる」にdoingを、完了とみなす節「リリース待ち」にreleasedを書いたロードマップ。
fn roadmap(doing: &str, released: &str) -> String {
    format!(
        "## やってる\n\n{doing}\n\n## 優先度: 高\n\n## 優先度: 中\n\n## リリース待ち\n\n{released}\n\n## やった\n"
    )
}

/// リポジトリrootでブランチを作る2つのタスクを持つ作業計画書。
fn plan(root: &Path) -> String {
    format!(
        "**計画書名**: demo\n\n## 設計\n\n状態ファイルを作る。\n\n## 実装判断ログ\n\n## タスクリスト\n\n\
         - [ ] **1-1**: [{root}] 状態ファイルを作る\n  - **想定行数**: 約100行 (実装50行 + テスト50行)\n\
         - [ ] **1-2**: [{root}] 状態を表示する\n  - **想定行数**: 約100行 (実装50行 + テスト50行)\n",
        root = root.display()
    )
}

/// 作業計画書をロードマップの「やってる」に置いたWiki。
fn wiki(root: &Path) -> Wiki {
    let wiki = Wiki {
        pages: RefCell::new(HashMap::new()),
    };
    wiki.set(1214, roadmap(&format!("- [[{TITLE}]]"), ""));
    wiki.set(PLAN_PAGE, plan(root));
    wiki
}

fn build() -> Build {
    Build {
        version: "0.1.0".to_owned(),
        commit: "unknown".to_owned(),
        commit_short: "unknown".to_owned(),
        date: "2026-11-16".to_owned(),
        source_dir: std::path::PathBuf::new(),
    }
}

/// 人間キューの置き場をトピック12に置く設定のプロジェクト。
fn queue_project() -> Repo {
    let repo = repo_on("main");
    write_file(
        &repo.root,
        config::CONFIG_PATH,
        br#"{"roadmap": "https://wikino.app/s/shimbaco/pages/1214", "project": "mewst", "queue": {"space": "shimbaco", "topic": 12}, "notify": {"command": ["notify-test"]}}"#,
    );
    repo
}

/// 人が行うタスクだけを持つ作業計画書。
const HUMAN_PLAN: &str = "**計画書名**: demo\n\n## 設計\n\n状態ファイルを作る。\n\n## 実装判断ログ\n\n## タスクリスト\n\n- [ ] **1-1**: 見本の項目を確かめる\n  - **担当**: shimbaco\n";

/// 監督のループが始める日時に積んだ、作業計画書demoの場面labelの項目のタイトル。
fn item_title(label: &str) -> String {
    format!("mewst - demo - {label} - 2026-11-16 01時00分00秒")
}

/// プロジェクトのルートrootで、人間キューの置き場をqueueにして監督のループを動かし、進捗と診断の出力を返す。
fn queue_outputs(root: &Path, wiki: &Wiki, queue: &Queue, clock: &Clock<'_>) -> (String, String) {
    let sessions = Sessions::new(wiki, Vec::new());
    run_with(root, wiki, &sessions, queue, queue, false, clock, &build())
}

/// プロジェクトのルートrootで監督のループを動かし、出力を返す。
fn run_loop(root: &Path, wiki: &Wiki, sessions: &Sessions<'_>, clock: &Clock<'_>) -> String {
    let build = build();
    let (out, err) = loop_outputs(root, wiki, sessions, clock, &build);
    format!("{out}{err}")
}

/// 進捗と診断の出力を別々に取得する。
fn loop_outputs(
    root: &Path,
    runner: &dyn Runner,
    launcher: &dyn Launcher,
    clock: &Clock<'_>,
    build: &Build,
) -> (String, String) {
    run_with(
        root, runner, launcher, &NoQueue, &NoQueue, false, clock, build,
    )
}

/// プロジェクトのルートrootで、人間キューの置き場をqueuesで開いて監督のループを動かし、進捗と診断の出力を返す。
///
/// 通知のコマンドはnotifierで実行し、terminalが真なら標準エラーを端末として扱う。
#[expect(
    clippy::too_many_arguments,
    reason = "差し替える外部とのやり取りを、テストごとに選んで渡すため"
)]
fn run_with(
    root: &Path,
    runner: &dyn Runner,
    launcher: &dyn Launcher,
    queues: &dyn Open,
    notifier: &dyn Notifier,
    terminal: bool,
    clock: &Clock<'_>,
    build: &Build,
) -> (String, String) {
    let start = Start {
        runner,
        launcher,
        clock,
        queues,
        notifier,
        terminal,
        time_zone: &TimeZone::UTC,
        build,
        web: &web::Link::system(),
        root,
        timing: Timing::default(),
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    run(&start, &mut out, &mut err, clock.cancel).expect("監督のループがエラーで終わった");
    (
        String::from_utf8(out).expect("進捗がUTF-8ではない"),
        String::from_utf8(err).expect("診断がUTF-8ではない"),
    )
}

fn plan_dir(root: &Path) -> PlanDir {
    Store::new(root)
        .plan("demo")
        .expect("状態の置き場を決められなかった")
}

fn load(root: &Path) -> PlanState {
    plan_dir(root)
        .load()
        .expect("状態を読めなかった")
        .expect("状態ファイルが無い")
}

/// リポジトリrootの今のブランチに、ファイルnameを足すコミットをする。
fn commit(root: &Path, name: &str) {
    write_file(root, name, name.as_bytes());
    git_run(root, &["add", name]);
    git_run(root, &["commit", "--quiet", "-m", name]);
}

/// プロジェクトのルートrootに、タスクのブランチbranchのnumber回目のレビュードキュメントを書く。
fn review(root: &Path, branch: &str, number: u32, verdict: &str, findings: &str) {
    write_file(
        root,
        &format!("docs/private/reviews/{branch}-{number:03}.md"),
        format!(
            "# コードレビュー: {branch}\n\n## ファイルごとのレビュー結果\n\n{findings}## 総合評価\n\n**評価**: {verdict}\n"
        )
        .as_bytes(),
    );
}

const AUTO_FIX: &str = "### 1. 名前を揃える\n\n**区分**: 自動対応可\n\n**対応方針**:\n\n- [ ] 修正案の通り直す (推奨)\n- [ ] 現状維持\n\n";

fn plain() -> Value {
    json!({"needs_human": null})
}

fn implemented() -> Value {
    json!({"needs_human": null, "verification": []})
}

/// plan(root) の2つのタスクを、1つ目は修正を挟んで2回のレビューで、2つ目は1回のレビューで終え、
/// 処理の流れを書かない作業報告書を書くセッションの作業。
fn whole_plan(root: &Path) -> Vec<Work<'_>> {
    vec![
        Box::new(move |wiki: &Wiki| {
            commit(root, "state.txt");
            wiki.check("1-1");
            Some(json!({"needs_human": null, "verification": [{
                "kind": "process",
                "title": "statusで状態を表示できる",
                "target": "korylusfactory status",
                "steps": ["korylusfactory status を実行する"],
                "expected": "進行中の計画書が表示される",
            }]}))
        }),
        Box::new(move |_: &Wiki| {
            review(root, "demo-1-1", 1, "Request Changes", AUTO_FIX);
            Some(plain())
        }),
        Box::new(move |_: &Wiki| {
            commit(root, "rename.txt");
            Some(plain())
        }),
        Box::new(move |_: &Wiki| {
            review(root, "demo-1-1", 2, "Approve", "");
            Some(plain())
        }),
        Box::new(move |wiki: &Wiki| {
            commit(root, "status.txt");
            wiki.check("1-2");
            Some(implemented())
        }),
        Box::new(move |_: &Wiki| {
            review(root, "demo-1-2", 1, "Approve", "");
            Some(plain())
        }),
        Box::new(|_: &Wiki| {
            Some(json!({
                "needs_human": null,
                "location": "https://wikino.app/s/shimbaco/pages/30",
                "walkthrough_skipped": "テストの計画書で、追う処理が無いため",
            }))
        }),
    ]
}

/// プロジェクトのルートrootに残した、セッションの記録のディレクトリの名前を並べて返す。
///
/// どの記録にも、セッションへの指示を残していることを確かめる。
fn session_logs(root: &Path) -> Vec<String> {
    let dir = plan_dir(root).path().join("sessions");
    let mut logs: Vec<String> = fs::read_dir(&dir)
        .expect("セッションの記録の置き場を読めなかった")
        .map(|entry| {
            entry
                .expect("セッションの記録を読めなかった")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    logs.sort();
    for name in &logs {
        assert!(
            dir.join(name).join("prompt.md").is_file(),
            "{name} に、セッションへの指示を残していない"
        );
    }
    logs
}

/// 指示promptに、手順のコマンドcommandがあることを確かめる。
fn assert_command(prompt: &str, command: &str) {
    assert!(
        prompt.contains(command),
        "指示に {command} がない:\n{prompt}"
    );
}

/// whole_plan のセッションを進め終えた状態stateに、レビュアーとセッションの記録が残っていることを確かめる。
fn assert_whole_plan_sessions(state: &PlanState) {
    let reviewers: Vec<Reviewer> = state.tasks[0]
        .review
        .reviews
        .iter()
        .map(|review| review.role)
        .collect();
    assert_eq!(
        reviewers,
        [Reviewer::Codex, Reviewer::Claude],
        "レビューを動かしたエージェントをレビュアーとして記録していない"
    );
    // 各セッションの「タスク ステップ レビューの回数 枠 エージェント 評価」。結果はすべて記録した。
    let records: Vec<String> = state
        .sessions
        .iter()
        .map(|session| {
            assert_eq!(
                session.result,
                Some(SessionResult::Recorded),
                "結果を記録していない"
            );
            format!(
                "{} {:?} {:?} {:?} {} {:?}",
                session.task.as_deref().unwrap_or("-"),
                session.step,
                session.review,
                session.slot,
                session.agent,
                session.verdict
            )
        })
        .collect();
    assert_eq!(
        records,
        [
            "1-1 Implement None Implementer claude None",
            "1-1 Review Some(1) ReviewerOdd codex Some(RequestChanges)",
            "1-1 Fix Some(1) ReviewerOdd codex None",
            "1-1 Review Some(2) ReviewerEven claude Some(Approve)",
            "1-2 Implement None Implementer claude None",
            "1-2 Review Some(1) ReviewerOdd codex Some(Approve)",
            "- Report None Implementer claude None",
        ],
        "起動したセッションの記録が違う"
    );
    assert!(
        state
            .sessions
            .iter()
            .all(|session| session.ended_at.is_some()
                && session.model.is_none()
                && session.effort.is_none()),
        "終わったセッションの時刻か、指定していないモデルとeffortの記録が違う"
    );
}

#[test]
fn ロードマップの作業計画書をタスクの実装とレビューから作業報告書まで進めて確認を待つ() {
    let repo = project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    let sessions = Sessions::new(&wiki, whole_plan(root));
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(1, &cancel);

    let out = run_loop(root, &wiki, &sessions, &clock);

    let state = load(root);
    let wait = state.wait.clone().expect("人間を待っていない");
    assert_eq!(wait.kind, WaitKind::Report, "人間を待つ場面が違う: {out}");
    assert!(
        !state.session_started,
        "完了したセッションを実行中のまま残した"
    );
    assert_eq!(
        state.title, TITLE,
        "ロードマップのリンクのタイトルを残していない"
    );
    let bases: Vec<(&str, &str)> = state
        .tasks
        .iter()
        .map(|record| (record.branch.as_str(), record.base.as_str()))
        .collect();
    assert_eq!(
        bases,
        [("demo-1-1", "main"), ("demo-1-2", "demo-1-1")],
        "2つ目のタスクのブランチを、前のタスクのブランチから作っていない"
    );
    let verdicts: Vec<Verdict> = state.tasks[0]
        .review
        .reviews
        .iter()
        .map(|review| review.verdict)
        .collect();
    assert_eq!(verdicts, [Verdict::RequestChanges, Verdict::Approve]);
    assert_whole_plan_sessions(&state);
    assert_eq!(
        state.tasks[0].verification.len(),
        1,
        "動作確認の項目を記録していない"
    );
    assert_eq!(
        state.latest_report(),
        Some("https://wikino.app/s/shimbaco/pages/30")
    );

    let prompts = sessions.prompts();
    assert_eq!(prompts.len(), 7, "起動したセッションの数が違う");
    let repo_arg = format!("repo={}", root.display());
    assert_command(
        &prompts[0],
        &format!("`/korylus-do-plan-task {PLAN_URL} 1-1 {repo_arg} base=main`"),
    );
    assert_command(&prompts[1], &format!("`/korylus-review main {PLAN_URL}`"));
    assert_command(
        &prompts[2],
        "レビュードキュメント docs/private/reviews/demo-1-1-001.md",
    );
    assert_command(&prompts[6], &format!("`/korylus-write-report {PLAN_URL}`"));
    assert_eq!(
        session_logs(root),
        [
            "20261116T010000Z-1-1-fix",
            "20261116T010000Z-1-1-implement",
            "20261116T010000Z-1-1-review-1",
            "20261116T010000Z-1-1-review-2",
            "20261116T010000Z-1-2-implement",
            "20261116T010000Z-1-2-review-1",
            "20261116T010000Z-report",
        ]
    );
    assert_eq!(
        clock.sleeps(),
        [Timing::default().state],
        "作業報告書の確認を、状態ファイルを読み直す間隔で待っていない"
    );
    for line in [
        "作業計画書 demo を始めます: https://wikino.app/s/shimbaco/pages/20\n",
        &format!(
            "demo: タスク 1-1 のブランチ demo-1-1 を {} に準備しました (元のブランチ: main)\n",
            root.display()
        ),
        "demo: タスク 1-1 の実装を記録しました\n",
        "demo: タスク 1-1 の2回目のレビューを記録しました\n",
        "demo: 人間を待っています (作業報告書の確認): 作業報告書の確認を待っています\n",
    ] {
        assert!(out.contains(line), "{line:?} を表示していない:\n{out}");
    }
}

#[test]
fn 完了の条件を満たさなければ理由を添えて間隔を空けてやり直し_上限を超えたら人間を待つ() {
    let repo = project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    // コミットもチェックもせずに、結果だけを返す。
    let works: Vec<Work<'_>> = (0..=RETRY_LIMIT)
        .map(|_| Box::new(|_: &Wiki| Some(implemented())) as Work<'_>)
        .collect();
    let sessions = Sessions::new(&wiki, works);
    let cancel = AtomicBool::new(false);
    let timing = Timing::default();
    let clock = Clock::new(4, &cancel);

    let (out, err) = loop_outputs(root, &wiki, &sessions, &clock, &build());

    let state = load(root);
    let wait = state.wait.clone().expect("人間を待っていない");
    assert_eq!(
        wait.kind,
        WaitKind::RetryLimit,
        "人間を待つ場面が違う: {out}"
    );
    assert_eq!(
        state.step,
        Some(Step::Implement),
        "同じステップのまま待っていない"
    );
    let results: Vec<_> = state
        .sessions
        .iter()
        .map(|session| session.result)
        .collect();
    assert_eq!(
        results,
        [Some(SessionResult::Unfinished); 4],
        "終えられなかったセッションの結果を記録していない"
    );
    assert_eq!(
        clock.sleeps(),
        [timing.retry, timing.retry, timing.retry, timing.state],
        "やり直しの間隔を空けていない"
    );
    let prompts = sessions.prompts();
    assert_eq!(prompts.len(), 4, "上限を超えてやり直した");
    assert!(
        !prompts[0].contains("## やり直し"),
        "初めてのセッションにやり直しを伝えた"
    );
    assert!(
        prompts[1].contains("## やり直し")
            && prompts[1]
                .contains("ブランチ demo-1-1 に、ベース main より先のコミットがありません"),
        "満たせなかった条件をやり直しのセッションに伝えていない: {}",
        prompts[1]
    );
    assert!(
        err.contains(
            "demo: タスク 1-1 の実装を終えられなかったため、5分後にやり直します (1/3回目)\n"
        ),
        "やり直すことを標準エラーへ書いていない:\n{err}"
    );
    assert!(
        !out.contains("を終えられなかったため"),
        "標準出力にやり直しの理由が混ざった:\n{out}"
    );
}

#[test]
fn セッションの記録を作れなければ開始を残さず_次の回を中断として伝えない() {
    let repo = project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    // セッションの記録の置き場を同じ名前のファイルでふさぎ、セッションを起動する前に失敗させる。
    let blocker = plan_dir(root).path().join("sessions");
    fs::create_dir_all(plan_dir(root).path()).expect("状態の置き場を作れなかった");
    fs::write(&blocker, "").expect("記録の置き場をふさげなかった");
    let sessions = Sessions::new(&wiki, Vec::new());
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(1, &cancel);

    let (_, err) = loop_outputs(root, &wiki, &sessions, &clock, &build());

    assert!(
        err.contains("セッションの記録 "),
        "記録を作れないことを伝えていない:\n{err}"
    );
    assert!(sessions.prompts().is_empty(), "セッションを起動した");
    let state = load(root);
    assert_eq!(
        state.step,
        Some(Step::Implement),
        "実装のステップに進んでいない"
    );
    assert!(
        !state.session_started,
        "起動していないセッションの開始を残した"
    );

    fs::remove_file(&blocker).expect("記録の置き場を戻せなかった");
    let sessions = Sessions::new(&wiki, vec![Box::new(|_: &Wiki| None)]);
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(1, &cancel);

    run_loop(root, &wiki, &sessions, &clock);

    let prompts = sessions.prompts();
    assert_eq!(prompts.len(), 1, "セッションを1回だけ起動していない");
    assert!(
        !prompts[0].contains("## やり直し"),
        "起動していないセッションを中断として伝えた:\n{}",
        prompts[0]
    );
}

#[test]
fn セッションが結果を返さずに終わったら途中で止まったものとしてやり直す() {
    let repo = project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    let sessions = Sessions::new(&wiki, vec![Box::new(|_: &Wiki| None)]);
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(1, &cancel);

    run_loop(root, &wiki, &sessions, &clock);

    let state = load(root);
    assert_eq!(state.retries, 1, "やり直しの回数を数えていない");
    assert_eq!(state.retry_reasons.len(), 1, "止まった理由を残していない");
    assert!(
        state.retry_reasons[0].starts_with("セッションが終了コード 1 で終わりました。記録: "),
        "止まった理由が違う: {:?}",
        state.retry_reasons
    );
}

#[test]
fn セッションが人間の判断を求めたら同じステップのまま質問の人間待ちにする() {
    let repo = project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    let sessions = Sessions::new(
        &wiki,
        vec![Box::new(|_: &Wiki| {
            Some(json!({"needs_human": "設計を変えてよいか", "verification": []}))
        })],
    );
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(2, &cancel);

    let out = run_loop(root, &wiki, &sessions, &clock);

    let state = load(root);
    let wait = state.wait.clone().expect("人間を待っていない");
    assert_eq!(wait.kind, WaitKind::Question, "人間を待つ場面が違う");
    assert_eq!(wait.details[0], "設計を変えてよいか");
    assert_eq!(
        state.step,
        Some(Step::Implement),
        "同じステップのまま待っていない"
    );
    assert_eq!(
        state
            .sessions
            .iter()
            .map(|session| session.result)
            .collect::<Vec<_>>(),
        [Some(SessionResult::Asked)],
        "人間の判断を求めたセッションの結果を記録していない"
    );
    assert_eq!(
        out.matches("demo: 人間を待っています (セッションからの質問): 実装のセッションが、人間の判断を求めて終わりました\n  設計を変えてよいか\n").count(),
        1,
        "待っている理由を1回だけ表示していない:\n{out}"
    );
    assert!(
        out.contains("対応したら korylusfactory resume を実行してください"),
        "再開のしかたを表示していない:\n{out}"
    );
}

#[test]
fn 取り消したらセッションの結果を記録せずに終える() {
    let repo = project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    let cancel = AtomicBool::new(false);
    let sessions = Sessions::new(
        &wiki,
        vec![Box::new(|_: &Wiki| {
            cancel.store(true, Ordering::SeqCst);
            Some(implemented())
        })],
    );
    let clock = Clock::new(usize::MAX, &cancel);

    run_loop(root, &wiki, &sessions, &clock);

    let state = load(root);
    assert_eq!(state.step, Some(Step::Implement), "ステップが変わった");
    assert_eq!(state.retries, 0, "やり直しの回数に数えた");
    assert!(
        state.session_started,
        "次に起動するセッションへ中断を伝える印を外した"
    );
    assert_eq!(
        state
            .sessions
            .iter()
            .map(|session| session.result)
            .collect::<Vec<_>>(),
        [Some(SessionResult::Cancelled)],
        "取り消したセッションの結果を記録していない"
    );
    assert_eq!(clock.sleeps(), [], "取り消したのに待った");
}

#[test]
fn セッションの取り消しと状態変更が重なっても取り消しを記録して待ちを残す() {
    let repo = project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    let cancel = AtomicBool::new(false);
    let wait = Wait {
        kind: WaitKind::Question,
        reason: "人間の判断を待つ".to_owned(),
        details: Vec::new(),
    };
    let sessions = Sessions::new(
        &wiki,
        vec![Box::new(|_: &Wiki| {
            let lock = plan_dir(root).lock().expect("状態のロックを取れなかった");
            let mut state = lock
                .load()
                .expect("状態を読めなかった")
                .expect("状態が無い");
            state.wait = Some(wait.clone());
            lock.save(&state).expect("人間待ちを保存できなかった");
            cancel.store(true, Ordering::SeqCst);
            Some(implemented())
        })],
    );
    let clock = Clock::new(usize::MAX, &cancel);

    run_loop(root, &wiki, &sessions, &clock);

    let state = load(root);
    assert_eq!(state.wait.as_ref(), Some(&wait), "人間待ちが変わった");
    assert!(state.session_started, "開始の印を外した");
    assert_eq!(
        state.sessions.last().and_then(|session| session.result),
        Some(SessionResult::Cancelled),
        "取り消したセッションの結果を記録していない"
    );
}

#[test]
fn 作業報告書の確認を待つ間にロードマップで完了が伝えられたら完了を記録して次を待つ() {
    let repo = project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    wiki.set(1214, roadmap("", &format!("- [[{TITLE}]]")));
    let mut state = PlanState::new(PLAN_URL, TITLE);
    state.wait = Some(Wait {
        kind: WaitKind::Report,
        reason: "作業報告書の確認を待っています".to_owned(),
        details: Vec::new(),
    });
    plan_dir(root)
        .lock()
        .expect("ロックを取れなかった")
        .save(&state)
        .expect("状態を保存できなかった");
    let sessions = Sessions::new(&wiki, Vec::new());
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(1, &cancel);

    let out = run_loop(root, &wiki, &sessions, &clock);

    assert_eq!(
        load(root).completed_at,
        Some(
            clock
                .now
                .get()
                .checked_sub(SignedDuration::from_mins(5))
                .expect("日時を戻せなかった")
        ),
        "完了を記録していない:\n{out}"
    );
    assert_eq!(clock.sleeps(), [Timing::default().roadmap]);
    for line in [
        "作業計画書 demo の完了を記録しました\n",
        "進める作業計画書がありません。ロードマップの承認の節に作業計画書が現れるのを待ちます\n",
    ] {
        assert!(out.contains(line), "{line:?} を表示していない:\n{out}");
    }
}

#[test]
fn 作業報告書の確認を待つ間に完了を確かめられなければ標準エラーへ一度だけ書く() {
    /// ロードマップの読み取りだけを失敗させ、ほかの呼び出しはwikiに任せるRunner。読み取りを試みた回数を数える。
    struct RoadmapDown<'a> {
        wiki: &'a Wiki,
        reads: Cell<usize>,
    }
    impl Runner for RoadmapDown<'_> {
        fn run(&self, program: &str, args: &[OsString]) -> Result<Output, process::Error> {
            if program == "wikino" && args.get(2).is_some_and(|arg| arg == "1214") {
                self.reads.set(self.reads.get().saturating_add(1));
                return Ok(Output {
                    code: 1,
                    ..Output::default()
                });
            }
            self.wiki.run(program, args)
        }
    }

    let repo = project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    let mut state = PlanState::new(PLAN_URL, TITLE);
    state.wait = Some(Wait {
        kind: WaitKind::Report,
        reason: "作業報告書の確認を待っています".to_owned(),
        details: Vec::new(),
    });
    plan_dir(root)
        .lock()
        .expect("ロックを取れなかった")
        .save(&state)
        .expect("状態を保存できなかった");
    let runner = RoadmapDown {
        wiki: &wiki,
        reads: Cell::new(0),
    };
    let sessions = Sessions::new(&wiki, Vec::new());
    let cancel = AtomicBool::new(false);
    let timing = Timing::default();
    // 状態を読み直す間隔で待ち続け、ロードマップを読み直す間隔が過ぎた次の回まで動かす。
    let checks = timing.roadmap.as_secs() / timing.state.as_secs();
    let clock = Clock::new(
        usize::try_from(checks).expect("回数が大きすぎる") + 1,
        &cancel,
    );

    let (out, err) = loop_outputs(root, &runner, &sessions, &clock, &build());

    assert_eq!(runner.reads.get(), 2, "完了を2回確かめていない");
    assert_eq!(
        err.matches("demo: 完了を確かめられません: ").count(),
        1,
        "完了を確かめられないことを標準エラーへ1回だけ書いていない:\n{err}"
    );
    assert!(
        !out.contains("完了を確かめられません"),
        "標準出力にエラーが混ざった:\n{out}"
    );
    assert!(
        out.contains("demo: 人間を待っています (作業報告書の確認): 作業報告書の確認を待っています"),
        "待っている理由を表示していない:\n{out}"
    );
}

#[test]
fn 並びの先頭の作業計画書を読み取れなければ飛ばさずに待ち_理由を繰り返し表示しない() {
    let repo = project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    wiki.set(
        1214,
        roadmap(&format!("- [[まだ無い 作業計画書]]\n- [[{TITLE}]]"), ""),
    );
    let sessions = Sessions::new(&wiki, Vec::new());
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(2, &cancel);

    let out = run_loop(root, &wiki, &sessions, &clock);

    assert_eq!(
        out.matches("ロードマップの「やってる」の節の先頭の作業計画書 [[まだ無い 作業計画書]] は、下書きもページも見つかりません。飛ばさずに、直されるのを待ちます\n").count(),
        1,
        "待っている理由を1回だけ表示していない:\n{out}"
    );
    assert_eq!(
        clock.sleeps(),
        [Timing::default().roadmap, Timing::default().roadmap]
    );
    assert!(
        plan_dir(root).load().expect("状態を読めなかった").is_none(),
        "読み取れない計画書を飛ばして、次の計画書を始めた"
    );
}

#[test]
fn 未コミットの変更があればブランチを準備せずに人間を待つ() {
    let repo = project();
    let root = repo.root.as_path();
    write_file(root, "memo.txt", b"memo\n");
    let wiki = wiki(root);
    let sessions = Sessions::new(&wiki, Vec::new());
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(1, &cancel);

    run_loop(root, &wiki, &sessions, &clock);

    let wait = load(root).wait.expect("人間を待っていない");
    assert_eq!(wait.kind, WaitKind::Branch, "人間を待つ場面が違う");
    assert_eq!(
        wait.reason,
        format!(
            "{} に未コミットの変更があるため、ブランチ demo-1-1 を準備できません",
            root.display()
        )
    );
    assert_eq!(wait.details, ["?? memo.txt"]);
}

#[test]
fn 同じプロジェクトでは二重に起動できない() {
    let dir = tempfile::tempdir().expect("一時ディレクトリを作れなかった");
    let _first = hold(dir.path()).expect("ロックを取れなかった");
    let err = hold(dir.path()).expect_err("二重にロックを取れてしまった");
    assert!(matches!(err, Error::Running(_)), "エラー = {err:?}");
}

#[test]
fn 読み取り失敗は標準エラーへ一度だけ書き_進捗へ混ぜない() {
    struct Failed;
    impl Runner for Failed {
        fn run(&self, program: &str, args: &[OsString]) -> Result<Output, process::Error> {
            if program == "git" {
                return System.run(program, args);
            }
            Ok(Output {
                code: 1,
                ..Output::default()
            })
        }
    }
    let repo = project();
    let wiki = wiki(&repo.root);
    let sessions = Sessions::new(&wiki, Vec::new());
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(2, &cancel);
    let (out, err) = loop_outputs(&repo.root, &Failed, &sessions, &clock, &build());

    assert!(
        out.contains("ロードマップで承認された作業計画書を進めます"),
        "進捗が無い: {out}"
    );
    assert!(
        !out.contains("失敗しました"),
        "標準出力にエラーが混ざった: {out}"
    );
    assert_eq!(
        err.matches("wikino draft get が終了コード 1 で失敗しました")
            .count(),
        1,
        "読み取り失敗を1回だけ標準エラーへ書いていない: {err}"
    );
    assert!(
        err.contains("5分後に読み直します"),
        "読み直す間隔が無い: {err}"
    );
}

#[test]
fn 古いビルドの警告は標準エラーへ書き_進捗へ混ぜない() {
    let repo = project();
    let mut build = build();
    build.source_dir = repo.root.clone();
    build.commit =
        branch::head(&System, &repo.root, "main").expect("ビルドのコミットを読めなかった");
    commit(&repo.root, "crates/demo.rs");
    let wiki = wiki(&repo.root);
    wiki.set(1214, roadmap("", ""));
    let sessions = Sessions::new(&wiki, Vec::new());
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(1, &cancel);

    let (out, err) = loop_outputs(&repo.root, &wiki, &sessions, &clock, &build);

    assert!(!out.contains("警告:"), "標準出力に警告が混ざった: {out}");
    assert!(
        out.contains("進める作業計画書がありません"),
        "進捗が無い: {out}"
    );
    assert!(
        err.contains("警告: このkorylusfactoryは"),
        "古いビルドの警告が無い: {err}"
    );
    assert!(
        err.contains("install-korylusfactory で入れ直してください"),
        "入れ直す方法が無い: {err}"
    );
}

#[test]
fn 人間待ちを人間キューに一度だけ積み_待ちの表示に項目のurlを添える() {
    let repo = queue_project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    wiki.set(PLAN_PAGE, HUMAN_PLAN.to_owned());
    let queue = Queue::default();
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(2, &cancel);

    let (out, err) = queue_outputs(root, &wiki, &queue, &clock);

    let title = item_title("人が行うタスク");
    assert_eq!(
        queue.calls(),
        [format!("create {title}")],
        "項目を一度だけ積んでいない"
    );
    let Page::Draft { body, .. } = queue.page(1) else {
        panic!("項目の下書きが無い");
    };
    assert!(
        body.contains("- 場面: 人が行うタスク (タスク 1-1)\n"),
        "項目の本文に場面が無い:\n{body}"
    );
    let state = load(root);
    let url = "https://wikino.app/s/shimbaco/pages/1/edit";
    assert_eq!(
        state.items,
        [WaitItem {
            wait: state.wait.clone().expect("人間を待っていない"),
            item: QueuedItem {
                location: Some(korylusfactory_core::state::ItemLocation {
                    space: "shimbaco".to_owned(),
                    topic: 12
                }),
                title,
                at: "2026-11-16T01:00:00"
                    .parse()
                    .expect("日時を読み取れなかった"),
                page: Some(ItemPage {
                    number: 1,
                    url: url.to_owned(),
                }),
            },
        }]
    );
    for line in [
        format!("demo: 人間キューに項目を積みました: {url}\n"),
        format!("  人間キューの項目: {url}\n"),
    ] {
        assert!(out.contains(&line), "{line:?} を表示していない:\n{out}");
    }
    assert!(
        !err.contains("人間キュー"),
        "人間キューの警告を書いた:\n{err}"
    );
}

#[test]
fn 積んだ結果が分からなければ間隔を空けて一覧で探し_送り直さない() {
    let repo = queue_project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    wiki.set(PLAN_PAGE, HUMAN_PLAN.to_owned());
    let queue = Queue::default();
    queue.unconfirmed.set(1);
    let cancel = AtomicBool::new(false);
    let timing = Timing::default();
    // 状態を読み直す間隔で待ち続け、やり直す間隔が過ぎた次の回まで動かす。
    let checks = timing.queue.as_secs() / timing.state.as_secs();
    let clock = Clock::new(
        usize::try_from(checks).expect("回数が大きすぎる") + 1,
        &cancel,
    );

    let (_, err) = queue_outputs(root, &wiki, &queue, &clock);

    let title = item_title("人が行うタスク");
    assert_eq!(
        queue.calls(),
        [format!("create {title}"), "list".to_owned()],
        "送り直す前に一覧で探していない"
    );
    assert_eq!(
        load(root).items[0]
            .item
            .page
            .as_ref()
            .map(|page| page.number),
        Some(1),
        "一覧で見つけた項目を記録していない"
    );
    assert_eq!(
        err.matches(&format!(
            "demo: 人間キューに項目を積めません: 人間キューの項目「{title}」の下書きを作れたかを確かめられません"
        ))
        .count(),
        1,
        "積めなかったことを1回だけ警告していない:\n{err}"
    );
    assert!(
        err.contains("1分後にやり直します"),
        "やり直すことを伝えていない:\n{err}"
    );
}

#[test]
fn 待ちが解けた項目は本文を残して下書きを片付け_回答済みの項目は残す() {
    let repo = queue_project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    wiki.set(1214, roadmap("", &format!("- [[{TITLE}]]")));
    let queue = Queue::default();
    queue.insert(
        30,
        Page::Draft {
            title: item_title("人が行うタスク"),
            body: "## 状況\n\n書きかけの回答\n".to_owned(),
        },
    );
    queue.insert(
        31,
        Page::Published {
            title: item_title("タスクの準備"),
            body: String::new(),
        },
    );
    let wait = Wait {
        kind: WaitKind::HumanTask,
        reason: "人が行うタスクです".to_owned(),
        details: Vec::new(),
    };
    let record = |number: u64, label: &str| WaitItem {
        wait: wait.clone(),
        item: QueuedItem {
            location: Some(korylusfactory_core::state::ItemLocation {
                space: "shimbaco".to_owned(),
                topic: 12,
            }),
            title: item_title(label),
            at: "2026-11-16T01:00:00"
                .parse()
                .expect("日時を読み取れなかった"),
            page: Some(ItemPage {
                number,
                url: format!("https://wikino.app/s/shimbaco/pages/{number}/edit"),
            }),
        },
    };
    // 完了を記録した作業計画書も、項目を片付けるまで記録が残る。
    let mut state = PlanState::new(PLAN_URL, TITLE);
    state.completed_at = Some(
        "2026-11-16T00:00:00Z"
            .parse()
            .expect("日時を読み取れなかった"),
    );
    state.items = vec![record(30, "人が行うタスク"), record(31, "タスクの準備")];
    plan_dir(root)
        .lock()
        .expect("ロックを取れなかった")
        .save(&state)
        .expect("状態を保存できなかった");
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(1, &cancel);

    let (out, err) = queue_outputs(root, &wiki, &queue, &clock);

    assert_eq!(queue.calls(), ["read 30", "delete 30", "read 31"]);
    assert_eq!(
        fs::read_to_string(root.join("docs/private/factory/plans/demo/queue/30.md"))
            .expect("片付けた項目の本文を残していない"),
        "## 状況\n\n書きかけの回答\n"
    );
    assert_eq!(load(root).items, [], "片付けた項目の記録が残った");
    assert!(
        out.contains(&format!(
            "demo: 待ちが解けたため、人間キューの項目「{}」を片付けました\n",
            item_title("人が行うタスク")
        )),
        "片付けたことを表示していない:\n{out}"
    );
    assert!(
        !err.contains("人間キュー"),
        "人間キューの警告を書いた:\n{err}"
    );
}

#[test]
fn 読み取れない先頭の作業計画書を知らせの項目として積み_読み取れたら本文を残して片付ける() {
    let repo = queue_project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    wiki.set(1214, roadmap("- [[まだ無い 作業計画書]]", ""));
    wiki.set(PLAN_PAGE, HUMAN_PLAN.to_owned());
    let queue = Queue::default();
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(1, &cancel);

    let (out, _) = queue_outputs(root, &wiki, &queue, &clock);

    let notice_title =
        "mewst - まだ無い 作業計画書 - 作業計画書を読み取れない - 2026-11-16 01時00分00秒";
    assert_eq!(queue.calls(), [format!("create {notice_title}")]);
    let notices = Notices::new(root)
        .load()
        .expect("知らせの項目の記録を読めなかった");
    assert_eq!(
        notices
            .iter()
            .map(|notice| (
                notice.kind,
                notice.plan.as_str(),
                notice.item.page.as_ref().map(|page| page.number)
            ))
            .collect::<Vec<_>>(),
        [(NoticeKind::PlanUnreadable, "まだ無い 作業計画書", Some(1))]
    );
    assert!(
        out.contains(
            "人間キューに知らせの項目を積みました: https://wikino.app/s/shimbaco/pages/1/edit\n"
        ),
        "積んだことを表示していない:\n{out}"
    );

    wiki.set(1214, roadmap(&format!("- [[{TITLE}]]"), ""));
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(1, &cancel);
    queue_outputs(root, &wiki, &queue, &clock);

    assert_eq!(
        queue.calls()[1..],
        [
            "read 1".to_owned(),
            "delete 1".to_owned(),
            format!("create {}", item_title("人が行うタスク")),
        ]
    );
    assert!(
        fs::read_to_string(root.join("docs/private/factory/queue/1.md"))
            .expect("片付けた知らせの項目の本文を残していない")
            .contains("- 場面: 作業計画書を読み取れない\n"),
        "残した本文が知らせの項目ではない"
    );
    assert_eq!(
        Notices::new(root)
            .load()
            .expect("知らせの項目の記録を読めなかった"),
        Vec::<Notice>::new(),
        "片付けた知らせの項目の記録が残った"
    );
}

#[test]
fn 完了を記録したら取り込まれていないことの知らせと作業報告書の確認の項目を片付ける() {
    let repo = queue_project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    wiki.set(1214, roadmap("", &format!("- [[{TITLE}]]")));
    let queue = Queue::default();
    queue.insert(
        7,
        Page::Draft {
            title: "取り込まれていない".to_owned(),
            body: "知らせ".to_owned(),
        },
    );
    Notices::new(root)
        .save(&[Notice {
            resolved: false,
            kind: NoticeKind::NotMerged,
            plan: TITLE.to_owned(),
            item: QueuedItem {
                location: None,
                title: "取り込まれていない".to_owned(),
                at: "2026-11-16T00:00:00"
                    .parse()
                    .expect("日時を読み取れなかった"),
                page: Some(ItemPage {
                    number: 7,
                    url: "https://wikino.app/s/shimbaco/pages/7/edit".to_owned(),
                }),
            },
        }])
        .expect("知らせの項目の記録を書けなかった");
    let mut state = PlanState::new(PLAN_URL, TITLE);
    state.wait = Some(Wait {
        kind: WaitKind::Report,
        reason: "作業報告書の確認を待っています".to_owned(),
        details: Vec::new(),
    });
    plan_dir(root)
        .lock()
        .expect("ロックを取れなかった")
        .save(&state)
        .expect("状態を保存できなかった");
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(1, &cancel);

    let (out, err) = queue_outputs(root, &wiki, &queue, &clock);

    assert_eq!(
        queue.calls(),
        [
            format!("create {}", item_title("作業報告書の確認")),
            "read 7".to_owned(),
            "delete 7".to_owned(),
            "read 8".to_owned(),
            "delete 8".to_owned(),
        ],
        "知らせと項目を片付けていない:\n{out}{err}"
    );
    let state = load(root);
    assert!(state.is_completed(), "完了を記録していない");
    assert_eq!(state.items, [], "片付けた項目の記録が残った");
    assert_eq!(
        Notices::new(root)
            .load()
            .expect("知らせの項目の記録を読めなかった"),
        Vec::<Notice>::new()
    );
}

#[test]
fn 結果不明の項目が再起動前に公開されても_同じページを回収する() {
    let repo = queue_project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    wiki.set(PLAN_PAGE, HUMAN_PLAN.to_owned());
    let queue = Queue::default();
    queue.unconfirmed.set(1);
    queue.publish_unknown.set(true);
    let cancel = AtomicBool::new(false);
    queue_outputs(root, &wiki, &queue, &Clock::new(1, &cancel));
    assert!(
        load(root).items[0].item.page.is_none(),
        "結果不明なのに番号がある"
    );
    let cancel = AtomicBool::new(false);
    queue_outputs(root, &wiki, &queue, &Clock::new(1, &cancel));

    assert_eq!(
        queue.calls(),
        [
            format!("create {}", item_title("人が行うタスク")),
            "list".to_owned(),
            "pages".to_owned()
        ]
    );
    assert_eq!(
        queue.pages.borrow().len(),
        1,
        "公開済みの項目を重複作成した"
    );
    assert_eq!(
        load(root).items[0]
            .item
            .page
            .as_ref()
            .map(|page| page.number),
        Some(1),
        "公開済みのページ番号を回収しない"
    );
}

/// ページ7に、作成時の置き場が分かる知らせを置く。
fn recorded_notice(root: &Path, queue: &Queue, kind: NoticeKind, plan: &str) -> Notice {
    let notice = Notice {
        resolved: false,
        kind,
        plan: plan.to_owned(),
        item: QueuedItem {
            location: Some(korylusfactory_core::state::ItemLocation {
                space: "shimbaco".to_owned(),
                topic: 12,
            }),
            title: "前の知らせ".to_owned(),
            at: "2026-11-16T00:00:00".parse().expect("日時を読めなかった"),
            page: Some(ItemPage {
                number: 7,
                url: "https://wikino.app/s/shimbaco/pages/7/edit".to_owned(),
            }),
        },
    };
    queue.insert(
        7,
        Page::Draft {
            title: notice.item.title.clone(),
            body: "知らせの本文".to_owned(),
        },
    );
    Notices::new(root)
        .save(std::slice::from_ref(&notice))
        .expect("知らせを保存できなかった");
    notice
}

#[test]
fn 完了時に知らせの片付けに失敗しても_再起動後に片付け直す() {
    let repo = queue_project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    wiki.set(1214, roadmap("", &format!("- [[{TITLE}]]")));
    let queue = Queue::default();
    recorded_notice(root, &queue, NoticeKind::NotMerged, TITLE);
    queue.fail_read.set(Some(7));
    let mut state = PlanState::new(PLAN_URL, TITLE);
    state.wait = Some(Wait {
        kind: WaitKind::Report,
        reason: "報告書の確認".to_owned(),
        details: Vec::new(),
    });
    plan_dir(root)
        .lock()
        .expect("ロックを取れなかった")
        .save(&state)
        .expect("保存できなかった");
    let cancel = AtomicBool::new(false);
    queue_outputs(root, &wiki, &queue, &Clock::new(1, &cancel));

    assert!(load(root).is_completed(), "片付けの失敗で完了を止めた");
    assert!(
        Notices::new(root).load().expect("知らせを読めなかった")[0].resolved,
        "片付け待ちを記録しない"
    );
    let cancel = AtomicBool::new(false);
    queue_outputs(root, &wiki, &queue, &Clock::new(1, &cancel));

    assert_eq!(queue.page(7), Page::Deleted, "完了後に知らせを片付けない");
    assert!(
        Notices::new(root)
            .load()
            .expect("知らせを読めなかった")
            .is_empty(),
        "片付けた記録が残った"
    );
    assert_eq!(
        queue
            .calls()
            .iter()
            .filter(|call| *call == "read 7")
            .count(),
        2,
        "知らせの読み取りを再試行しない"
    );
}

#[test]
fn 読み取れた知らせの片付けに失敗しても_計画書を始めたあとに再試行する() {
    let repo = queue_project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    wiki.set(PLAN_PAGE, HUMAN_PLAN.to_owned());
    let queue = Queue::default();
    recorded_notice(
        root,
        &queue,
        NoticeKind::PlanUnreadable,
        "前に読み取れなかった計画書",
    );
    queue.fail_read.set(Some(7));
    let cancel = AtomicBool::new(false);
    let checks = Timing::default().queue.as_secs() / Timing::default().state.as_secs();
    let clock = Clock::new(
        usize::try_from(checks).expect("回数が大きすぎる") + 1,
        &cancel,
    );
    queue_outputs(root, &wiki, &queue, &clock);

    assert_eq!(
        load(root).wait.expect("人間を待っていない").kind,
        WaitKind::HumanTask,
        "次の計画書を始めていない"
    );
    assert_eq!(
        queue.page(7),
        Page::Deleted,
        "次の計画書の進行中に知らせを片付けない"
    );
    assert!(
        Notices::new(root)
            .load()
            .expect("知らせを読めなかった")
            .is_empty(),
        "片付けた記録が残った"
    );
    assert_eq!(
        queue
            .calls()
            .iter()
            .filter(|call| *call == "read 7")
            .count(),
        2,
        "片付けを再試行しない"
    );
}

#[test]
fn 置き場を変えても項目の回収と片付けは元のスペースへ送る() {
    for has_location in [true, false] {
        for pending in [true, false] {
            let repo = queue_project();
            let root = repo.root.as_path();
            let wiki = wiki(root);
            wiki.set(1214, roadmap("", &format!("- [[{TITLE}]]")));
            let queue = Queue::default();
            let notice = recorded_notice(root, &queue, NoticeKind::NotMerged, TITLE);
            Notices::new(root).save(&[]).expect("知らせを消せなかった");
            let mut item = notice.item;
            if !has_location {
                item.location = None;
            }
            if pending {
                item.page = None;
            }
            let wait = Wait {
                kind: WaitKind::Question,
                reason: "質問".to_owned(),
                details: Vec::new(),
            };
            let mut state = PlanState::new(PLAN_URL, TITLE);
            if pending {
                state.wait = Some(wait.clone());
            } else {
                state.completed_at =
                    Some("2026-11-16T00:00:00Z".parse().expect("日時を読めなかった"));
            }
            state.items = vec![WaitItem { wait, item }];
            plan_dir(root)
                .lock()
                .expect("ロックを取れなかった")
                .save(&state)
                .expect("保存できなかった");
            write_file(root, config::CONFIG_PATH, br#"{"roadmap":"https://wikino.app/s/shimbaco/pages/1214","project":"mewst","queue":{"space":"new-space","topic":99}}"#);
            let cancel = AtomicBool::new(false);
            // Queue::openは元のスペース・トピック以外で開くと失敗する。
            let (_, err) = queue_outputs(root, &wiki, &queue, &Clock::new(1, &cancel));
            if !has_location {
                assert!(queue.calls().is_empty(), "古い記録で現在の置き場を操作した");
                assert!(
                    err.contains("作成時の置き場を確かめられません"),
                    "置き場不明を警告しない: {err}"
                );
                assert_eq!(load(root).items.len(), 1, "未処理の記録を消した");
            } else if pending {
                assert_eq!(
                    load(root).items[0]
                        .item
                        .page
                        .as_ref()
                        .map(|page| page.number),
                    Some(7),
                    "元の置き場で回収しない"
                );
                assert_eq!(queue.calls(), ["list"], "回収で重複作成した");
            } else {
                assert_eq!(queue.page(7), Page::Deleted, "元の置き場で片付けない");
                assert!(load(root).items.is_empty(), "片付けた記録が残った");
            }
        }
    }
}

#[test]
fn 知らせの片付けも_保存した置き場を使い_古い記録の別スペースを操作しない() {
    for has_location in [true, false] {
        let repo = queue_project();
        let root = repo.root.as_path();
        let wiki = wiki(root);
        wiki.set(1214, roadmap("", ""));
        let queue = Queue::default();
        let mut notice = recorded_notice(root, &queue, NoticeKind::NotMerged, TITLE);
        notice.resolved = true;
        if !has_location {
            notice.item.location = None;
        }
        Notices::new(root)
            .save(&[notice])
            .expect("知らせを保存できなかった");
        write_file(root, config::CONFIG_PATH, br#"{"roadmap":"https://wikino.app/s/shimbaco/pages/1214","project":"mewst","queue":{"space":"new-space","topic":99}}"#);
        let cancel = AtomicBool::new(false);
        let (_, err) = queue_outputs(root, &wiki, &queue, &Clock::new(1, &cancel));
        if has_location {
            assert_eq!(queue.page(7), Page::Deleted, "元の置き場で片付けない");
            assert!(
                Notices::new(root)
                    .load()
                    .expect("知らせを読めなかった")
                    .is_empty(),
                "記録が残った"
            );
        } else {
            assert!(queue.calls().is_empty(), "古い記録で別スペースを操作した");
            assert!(
                err.contains("作成時の置き場を確かめられません"),
                "置き場不明を警告しない: {err}"
            );
            assert_eq!(
                Notices::new(root)
                    .load()
                    .expect("知らせを読めなかった")
                    .len(),
                1,
                "未処理の記録を消した"
            );
        }
    }
}

#[test]
fn 作業報告書の確認の待ちを離れたら_取り込まれていないことの知らせを片付ける() {
    /// ロードマップの読み取りだけを失敗させ、作業報告書の確認を待つ間に完了を確かめられなくするRunner。
    struct RoadmapDown<'a>(&'a Wiki);
    impl Runner for RoadmapDown<'_> {
        fn run(&self, program: &str, args: &[OsString]) -> Result<Output, process::Error> {
            if program == "wikino" && args.get(2).is_some_and(|arg| arg == "1214") {
                return Ok(Output {
                    code: 1,
                    ..Output::default()
                });
            }
            self.0.run(program, args)
        }
    }

    for reporting in [true, false] {
        let repo = queue_project();
        let root = repo.root.as_path();
        let wiki = wiki(root);
        wiki.set(PLAN_PAGE, HUMAN_PLAN.to_owned());
        let queue = Queue::default();
        recorded_notice(root, &queue, NoticeKind::NotMerged, TITLE);
        // 作業報告書の確認を待っていなければ、`resume` でその待ちが解けたあとの状態である。
        let mut state = PlanState::new(PLAN_URL, TITLE);
        if reporting {
            state.wait = Some(Wait {
                kind: WaitKind::Report,
                reason: "作業報告書の確認を待っています".to_owned(),
                details: Vec::new(),
            });
        }
        plan_dir(root)
            .lock()
            .expect("ロックを取れなかった")
            .save(&state)
            .expect("状態を保存できなかった");
        let down = RoadmapDown(&wiki);
        let runner: &dyn Runner = if reporting { &down } else { &wiki };
        let sessions = Sessions::new(&wiki, Vec::new());
        let cancel = AtomicBool::new(false);

        let (out, err) = run_with(
            root,
            runner,
            &sessions,
            &queue,
            &queue,
            false,
            &Clock::new(1, &cancel),
            &build(),
        );

        let notices = Notices::new(root)
            .load()
            .expect("知らせの項目の記録を読めなかった");
        if reporting {
            assert_eq!(
                queue.page(7),
                Page::Draft {
                    title: "前の知らせ".to_owned(),
                    body: "知らせの本文".to_owned(),
                },
                "作業報告書の確認を待つ間に、取り込まれたかを確かめずに片付けた:\n{out}{err}"
            );
            assert!(
                notices.len() == 1 && !notices[0].resolved,
                "作業報告書の確認を待つ間に、知らせを解消済みにした: {notices:?}"
            );
        } else {
            assert_eq!(
                queue.page(7),
                Page::Deleted,
                "作業報告書の確認の待ちを離れても知らせを片付けない:\n{out}{err}"
            );
            assert_eq!(notices, Vec::<Notice>::new(), "片付けた記録が残った");
            assert_eq!(
                load(root).wait.map(|wait| wait.kind),
                Some(WaitKind::HumanTask),
                "知らせの片付けで作業計画書の進行を止めた"
            );
        }
    }
}

/// 人が行うタスクを2つ持つ作業計画書。
const TWO_HUMAN_PLAN: &str = "**計画書名**: demo\n\n## 設計\n\n状態ファイルを作る。\n\n## 実装判断ログ\n\n## タスクリスト\n\n- [ ] **1-1**: 見本の項目を確かめる\n  - **担当**: shimbaco\n- [ ] **1-2**: 見本の項目を片付ける\n  - **担当**: shimbaco\n";

/// 作業計画書demoを待ちwaitで人間待ちにし、その待ちにページnumberの項目を積んだ記録を持たせて保存する。
fn save_waiting(
    root: &Path,
    wait: Wait,
    number: u64,
    label: &str,
    edit: impl FnOnce(&mut PlanState),
) {
    let mut state = PlanState::new(PLAN_URL, TITLE);
    state.wait = Some(wait.clone());
    state.items.push(WaitItem {
        wait,
        item: QueuedItem {
            location: Some(korylusfactory_core::state::ItemLocation {
                space: "shimbaco".to_owned(),
                topic: 12,
            }),
            title: item_title(label),
            at: "2026-11-16T01:00:00"
                .parse()
                .expect("日時を読み取れなかった"),
            page: Some(ItemPage {
                number,
                url: format!("https://wikino.app/s/shimbaco/pages/{number}/edit"),
            }),
        },
    });
    edit(&mut state);
    plan_dir(root)
        .lock()
        .expect("ロックを取れなかった")
        .save(&state)
        .expect("状態を保存できなかった");
}

fn human_wait() -> Wait {
    Wait {
        kind: WaitKind::HumanTask,
        reason: "人が行うタスクです".to_owned(),
        details: vec!["1-1 見本の項目を確かめる".to_owned()],
    }
}

#[test]
fn 公開された項目の回答で再開し_回答済みの項目は片付けずに記録から除く() {
    let repo = queue_project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    wiki.set(PLAN_PAGE, TWO_HUMAN_PLAN.to_owned());
    wiki.check("1-1");
    let queue = Queue::default();
    queue.insert(
        5,
        Page::Published {
            title: item_title("人が行うタスク"),
            body: "## 状況\n\n- 場面: 人が行うタスク (タスク 1-1)\n\n## 回答\n".to_owned(),
        },
    );
    save_waiting(root, human_wait(), 5, "人が行うタスク", |_| {});
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(1, &cancel);

    let (out, _) = queue_outputs(root, &wiki, &queue, &clock);

    assert_eq!(
        queue.calls(),
        [
            "read 5".to_owned(),
            format!("create {}", item_title("人が行うタスク"))
        ],
        "回答を読んでから次のタスクの項目を積んでいない:\n{out}"
    );
    assert!(
        out.contains("demo: 人間キューの項目の回答で、人が行うタスクの人間待ちから再開しました: https://wikino.app/s/shimbaco/pages/5/edit\n"),
        "再開したことを表示していない:\n{out}"
    );
    let state = load(root);
    assert_eq!(
        state.wait.as_ref().map(|wait| wait.details.clone()),
        Some(vec!["1-2 見本の項目を片付ける".to_owned()]),
        "次の人が行うタスクで待っていない"
    );
    assert_eq!(
        state
            .items
            .iter()
            .map(|record| record.item.page.as_ref().map(|page| page.number))
            .collect::<Vec<_>>(),
        [Some(6)],
        "回答済みの項目の記録が残った"
    );
    assert!(
        matches!(queue.page(5), Page::Published { .. }),
        "回答済みの項目を片付けた"
    );
}

#[test]
fn 回答を適用できなければ理由を書いて差し戻し_項目を読む間隔ごとに読み直す() {
    let repo = queue_project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    let queue = Queue::default();
    queue.insert(
        5,
        Page::Published {
            title: item_title("作業報告書の確認"),
            body: "## 状況\n\n- 場面: 作業報告書の確認\n\n## 回答\n\n回答を書き終えたら、このページを公開してください。\n"
                .to_owned(),
        },
    );
    let snapshot = plan(root);
    save_waiting(
        root,
        Wait {
            kind: WaitKind::Report,
            reason: "作業報告書の確認を待っています".to_owned(),
            details: Vec::new(),
        },
        5,
        "作業報告書の確認",
        |state| state.snapshot = Some(snapshot),
    );
    let cancel = AtomicBool::new(false);
    let timing = Timing::default();
    // 状態を読み直す間隔で待ち続け、項目を読む間隔が過ぎた次の回まで動かす。
    let checks = timing.queue.as_secs() / timing.state.as_secs();
    let clock = Clock::new(
        usize::try_from(checks).expect("回数が大きすぎる") + 1,
        &cancel,
    );

    let (out, _) = queue_outputs(root, &wiki, &queue, &clock);

    assert_eq!(
        queue.calls(),
        ["read 5", "reopen 5", "read 5"],
        "差し戻してから、項目を読む間隔を空けて読み直していない:\n{out}"
    );
    let Page::Draft {
        body: sent_back, ..
    } = queue.page(5)
    else {
        panic!("差し戻しの下書きが無い");
    };
    assert!(
        sent_back
            .starts_with("## 差し戻し\n\nディレクターが、公開された回答を適用できませんでした"),
        "差し戻しの理由を本文の先頭に書いていない:\n{sent_back}"
    );
    assert!(
        sent_back.contains("フェーズfb1のタスクが追加されていません"),
        "再開できない理由を書いていない:\n{sent_back}"
    );
    assert!(
        out.contains("demo: 人間キューの項目の回答を適用できないため、理由を書いて差し戻しました: https://wikino.app/s/shimbaco/pages/5/edit\n"),
        "差し戻したことを表示していない:\n{out}"
    );
    let state = load(root);
    assert!(state.wait.is_some(), "差し戻したのに再開した");
    assert_eq!(state.items.len(), 1, "差し戻した項目の記録を除いた");
}

#[test]
fn 項目の下書きもページも無くなっていたら記録を除いて積み直す() {
    let repo = queue_project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    wiki.set(PLAN_PAGE, HUMAN_PLAN.to_owned());
    let queue = Queue::default();
    save_waiting(root, human_wait(), 5, "人が行うタスク", |_| {});
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(2, &cancel);

    let (out, _) = queue_outputs(root, &wiki, &queue, &clock);

    // 積み直すのは次の回のため、積んだ日時は状態を読み直す間隔のあとになる。
    assert_eq!(
        queue.calls(),
        [
            "read 5".to_owned(),
            "create mewst - demo - 人が行うタスク - 2026-11-16 01時00分05秒".to_owned(),
        ],
        "積み直していない:\n{out}"
    );
    assert!(
        out.contains(&format!(
            "demo: 人間キューの項目「{}」の下書きもページも無くなっていたため、積み直します\n",
            item_title("人が行うタスク")
        )),
        "積み直すことを表示していない:\n{out}"
    );
    assert_eq!(
        load(root).items[0]
            .item
            .page
            .as_ref()
            .map(|page| page.number),
        Some(1),
        "積み直した項目を記録していない"
    );
}

/// 標準エラーを端末として扱い、人間キューの置き場と通知のコマンドをqueueにして監督のループを動かし、診断の出力を返す。
fn terminal_err(root: &Path, wiki: &Wiki, queue: &Queue, clock: &Clock<'_>) -> String {
    let sessions = Sessions::new(wiki, Vec::new());
    run_with(root, wiki, &sessions, queue, queue, true, clock, &build()).1
}

/// 場面sceneの人間待ちを、URLurlで知らせた内容。
fn note(scene: &str, url: Option<&str>) -> Message {
    Message {
        project: "mewst".to_owned(),
        plan: "demo".to_owned(),
        scene: scene.to_owned(),
        url: url.map(str::to_owned),
    }
}

#[test]
fn 人間待ちを通知のコマンドで一度だけ知らせ_端末のタイトルとベルを出す() {
    let repo = queue_project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    wiki.set(PLAN_PAGE, HUMAN_PLAN.to_owned());
    let queue = Queue::default();
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(3, &cancel);

    let err = terminal_err(root, &wiki, &queue, &clock);

    let url = "https://wikino.app/s/shimbaco/pages/1/edit";
    assert_eq!(queue.notes(), [note("人が行うタスク", Some(url))]);
    let state = load(root);
    assert_eq!(
        state.notified,
        Some(Notified {
            wait: state.wait.clone().expect("人間を待っていない"),
            url: Some(url.to_owned()),
        })
    );
    assert_eq!(
        err.matches("\x1b]2;korylusfactory - 人間待ち - 人が行うタスク\x1b\\")
            .count(),
        1,
        "人間待ちのタイトルを一度だけ出していない:\n{err:?}"
    );
    assert_eq!(
        err.matches('\x07').count(),
        1,
        "ベルを一度だけ鳴らしていない"
    );
}

#[test]
fn 項目を積めなければurlを渡さずに知らせ_積めたらurlを添えてもう一度知らせる() {
    let repo = queue_project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    wiki.set(PLAN_PAGE, HUMAN_PLAN.to_owned());
    let queue = Queue::default();
    queue.unconfirmed.set(1);
    let cancel = AtomicBool::new(false);
    let timing = Timing::default();
    let checks = timing.queue.as_secs() / timing.state.as_secs();
    let clock = Clock::new(
        usize::try_from(checks).expect("回数が大きすぎる") + 2,
        &cancel,
    );

    queue_outputs(root, &wiki, &queue, &clock);

    assert_eq!(
        queue.notes(),
        [
            note("人が行うタスク", None),
            note(
                "人が行うタスク",
                Some("https://wikino.app/s/shimbaco/pages/1/edit")
            ),
        ]
    );
}

#[test]
fn 人間キューを使わない設定でも人間待ちを通知のコマンドで知らせる() {
    let repo = repo_on("main");
    let root = repo.root.as_path();
    write_file(
        root,
        config::CONFIG_PATH,
        br#"{"roadmap": "https://wikino.app/s/shimbaco/pages/1214", "project": "mewst", "notify": {"command": ["notify-test"]}}"#,
    );
    let wiki = wiki(root);
    wiki.set(PLAN_PAGE, HUMAN_PLAN.to_owned());
    let notes = Queue::default();
    let sessions = Sessions::new(&wiki, Vec::new());
    let cancel = AtomicBool::new(false);

    run_with(
        root,
        &wiki,
        &sessions,
        &NoQueue,
        &notes,
        false,
        &Clock::new(3, &cancel),
        &build(),
    );

    assert_eq!(notes.notes(), [note("人が行うタスク", None)]);
}

#[test]
fn 通知が失敗か時間切れでも警告だけにし_同じ待ちの監視を続ける() {
    for error in [
        notify::Error::Failed {
            program: "notify-test".to_owned(),
            code: 3,
            stderr: "送れません".to_owned(),
        },
        notify::Error::TimedOut {
            program: "notify-test".to_owned(),
            timeout: Duration::ZERO,
        },
    ] {
        let repo = queue_project();
        let root = repo.root.as_path();
        let wiki = wiki(root);
        wiki.set(PLAN_PAGE, HUMAN_PLAN.to_owned());
        let queue = Queue::default();
        *queue.notify_error.borrow_mut() = Some(error);
        let cancel = AtomicBool::new(false);
        let clock = Clock::new(3, &cancel);
        let (_, err) = queue_outputs(root, &wiki, &queue, &clock);
        assert!(
            err.contains("人間待ちを通知のコマンドで知らせられません"),
            "警告が無い: {err}"
        );
        assert_eq!(clock.sleeps().len(), 3, "通知の失敗で監視が止まった");
        assert_eq!(queue.notes().len(), 1, "通知の失敗で繰り返し知らせた");
        let state = load(root);
        assert!(
            state.wait.is_some() && state.notified.is_some(),
            "人間待ちと通知済みの記録が無い"
        );
    }
}

/// 人間キューを使わず、ブランチを作らない作業の同じ質問を繰り返せるプロジェクトとWikino。
fn question_project() -> (Repo, Wiki) {
    let repo = project();
    let root = repo.root.as_path();
    write_file(root, config::CONFIG_PATH, br#"{"roadmap":"https://wikino.app/s/shimbaco/pages/1214","project":"mewst","notify":{"command":["notify-test"]}}"#);
    let wiki = wiki(root);
    wiki.set(PLAN_PAGE, "**計画書名**: demo\n\n## タスクリスト\n\n- [ ] **1-1**: 下書きを作る\n  - **担当**: implementer\n".to_owned());
    let mut state = PlanState::new(PLAN_URL, TITLE);
    state.task = Some("1-1".to_owned());
    state.step = Some(Step::Work);
    state.snapshot = Some(wiki.page(PLAN_PAGE));
    flow::ask(&mut state, "設計を変えてよいか".to_owned()).expect("質問を記録できなかった");
    plan_dir(root)
        .lock()
        .expect("ロックを取れなかった")
        .save(&state)
        .expect("状態を保存できなかった");
    (repo, wiki)
}

/// 人間が質問に対応して再開する操作を、状態ファイルのロックの中で行う。
fn resume_question(root: &Path, wiki: &Wiki) {
    let dir = plan_dir(root);
    let lock = dir.lock().expect("ロックを取れなかった");
    let mut state = lock
        .load()
        .expect("状態を読めなかった")
        .expect("状態が無い");
    let project = Project {
        root: root.to_path_buf(),
        repo: root.to_path_buf(),
    };
    let plan = PlanDocument::parse(wiki.page(PLAN_PAGE)).expect("作業計画書を読めなかった");
    flow::resume(&mut state, &plan, &project, None).expect("再開できなかった");
    lock.save(&state).expect("状態を保存できなかった");
}

#[test]
fn 作業に戻ったあと同じ質問で止まっても通知とベルをもう一度出す() {
    let (repo, wiki) = question_project();
    let root = repo.root.as_path();
    let notes = Queue::default();
    let sessions = Sessions::new(
        &wiki,
        vec![Box::new(|_: &Wiki| {
            Some(json!({"needs_human":"設計を変えてよいか"}))
        })],
    );
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(2, &cancel);
    clock.on_first_sleep(|| resume_question(root, &wiki));
    let (_, err) = run_with(
        root,
        &wiki,
        &sessions,
        &NoQueue,
        &notes,
        true,
        &clock,
        &build(),
    );
    assert_eq!(sessions.prompts().len(), 1, "待ちの間に作業へ戻っていない");
    assert_eq!(
        notes.notes(),
        [
            note("セッションからの質問", None),
            note("セッションからの質問", None)
        ]
    );
    assert_eq!(
        err.matches('\x07').count(),
        2,
        "新しい待ちでベルを鳴らしていない"
    );
}

#[test]
fn 起動し直しても続いている待ちは通知せず_再発した待ちは通知する() {
    let (repo, wiki) = question_project();
    let root = repo.root.as_path();
    let notes = Queue::default();
    let sessions = Sessions::new(&wiki, Vec::new());
    for _ in 0..2 {
        let cancel = AtomicBool::new(false);
        run_with(
            root,
            &wiki,
            &sessions,
            &NoQueue,
            &notes,
            false,
            &Clock::new(1, &cancel),
            &build(),
        );
        assert_eq!(
            notes.notes().len(),
            1,
            "継続中の待ちを起動し直すたびに知らせた"
        );
    }
    resume_question(root, &wiki);
    let dir = plan_dir(root);
    let lock = dir.lock().expect("ロックを取れなかった");
    let mut state = lock
        .load()
        .expect("状態を読めなかった")
        .expect("状態が無い");
    flow::ask(&mut state, "設計を変えてよいか".to_owned()).expect("再度の質問を記録できなかった");
    lock.save(&state).expect("状態を保存できなかった");
    drop(lock);
    let cancel = AtomicBool::new(false);
    run_with(
        root,
        &wiki,
        &sessions,
        &NoQueue,
        &notes,
        false,
        &Clock::new(1, &cancel),
        &build(),
    );
    assert_eq!(notes.notes().len(), 2, "再発した待ちを知らせていない");
}

#[test]
fn 読み取れない計画書のベルは項目を積めたかどうかにかかわらず一度だけ鳴る() {
    for (queue_enabled, fail_create) in [(true, false), (true, true), (false, false)] {
        let repo = if queue_enabled {
            queue_project()
        } else {
            project()
        };
        let root = repo.root.as_path();
        let wiki = wiki(root);
        wiki.set(1214, roadmap("- [[まだ無い 作業計画書]]", ""));
        let queue = Queue::default();
        queue.unconfirmed.set(usize::from(fail_create));
        let cancel = AtomicBool::new(false);
        let err = terminal_err(root, &wiki, &queue, &Clock::new(1, &cancel));
        assert_eq!(
            err.matches('\x07').count(),
            1,
            "ベルが一度ではない: {err:?}"
        );
        assert!(
            err.find("\x1b]2;") < err.find('\x07'),
            "タイトルより先にベルを鳴らした: {err:?}"
        );
    }
}

#[test]
fn 差し戻したら差し戻しの場面で知らせる() {
    let repo = queue_project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    let queue = Queue::default();
    queue.insert(
        5,
        Page::Published {
            title: item_title("作業報告書の確認"),
            body: "## 状況\n\n- 場面: 作業報告書の確認\n\n## 回答\n".to_owned(),
        },
    );
    let wait = Wait {
        kind: WaitKind::Report,
        reason: "作業報告書の確認を待っています".to_owned(),
        details: Vec::new(),
    };
    let url = "https://wikino.app/s/shimbaco/pages/5/edit";
    let snapshot = plan(root);
    save_waiting(root, wait.clone(), 5, "作業報告書の確認", |state| {
        state.snapshot = Some(snapshot);
        // 待ちを知らせたあとの差し戻しだけを確かめる。
        state.notified = Some(Notified {
            wait,
            url: Some(url.to_owned()),
        });
    });
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(1, &cancel);

    queue_outputs(root, &wiki, &queue, &clock);

    assert_eq!(
        queue.notes(),
        [note("作業報告書の確認 (差し戻し)", Some(url))]
    );
}

#[test]
fn 作業計画書を進めるときと進めるものが無いときの状態を端末のタイトルに出す() {
    let repo = project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    wiki.set(
        PLAN_PAGE,
        "**計画書名**: demo\n\n## 設計\n\n状態ファイルを作る。\n\n## 実装判断ログ\n\n## タスクリスト\n\n- [ ] **1-1**: 下書きを作る\n  - **担当**: implementer\n".to_owned(),
    );
    let sessions = Sessions::new(
        &wiki,
        vec![
            Box::new(|wiki: &Wiki| {
                wiki.check("1-1");
                Some(plain())
            }),
            Box::new(|_: &Wiki| None),
        ],
    );
    let cancel = AtomicBool::new(false);
    let (_, err) = run_with(
        root,
        &wiki,
        &sessions,
        &NoQueue,
        &NoQueue,
        true,
        &Clock::new(1, &cancel),
        &build(),
    );
    for title in [
        "\x1b]2;korylusfactory - demo - タスク 1-1 のブランチを作らない作業\x1b\\",
        "\x1b]2;korylusfactory - demo - 作業報告書の作成\x1b\\",
    ] {
        assert!(err.contains(title), "{title:?} を出していない:\n{err:?}");
    }
    assert!(!err.contains('\x07'), "人間待ちではないのにベルを鳴らした");

    let repo = project();
    let root = repo.root.as_path();
    let idle = Wiki {
        pages: RefCell::new(HashMap::new()),
    };
    idle.set(1214, roadmap("", ""));
    let cancel = AtomicBool::new(false);
    let err = terminal_err(root, &idle, &Queue::default(), &Clock::new(2, &cancel));
    assert_eq!(
        err.matches("\x1b]2;korylusfactory - 進める作業計画書なし\x1b\\")
            .count(),
        1,
        "進めるものが無いことをタイトルに一度だけ出していない:\n{err:?}"
    );
}

#[test]
fn 作業報告書の確認の待ちで取り込まれていない知らせを積んでもベルは人間待ちごとに1回で_通知には計画書名を渡す()
 {
    use crate::branch::testutil::{branch_over_tag, temp_dir};

    let repo = queue_project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    wiki.set(1214, roadmap("", &format!("- [[{TITLE}]]")));
    let queue = Queue::default();
    // 最後のタスクのブランチを、リモートへ送ったまま既定のベースに取り込まない。
    let remote = repo_on("main");
    let (_dir, clone) = temp_dir();
    git_run(
        &clone,
        &["clone", "--quiet", &remote.root.display().to_string(), "."],
    );
    branch_over_tag(&clone, "demo-1-1");
    git_run(
        &clone,
        &["push", "--quiet", "origin", "refs/heads/demo-1-1"],
    );
    let mut state = PlanState::new(PLAN_URL, TITLE);
    state.tasks.push(korylusfactory_core::state::TaskRecord {
        id: "1-1".to_owned(),
        repo: clone.clone(),
        dir: clone.clone(),
        branch: "demo-1-1".to_owned(),
        base: "main".to_owned(),
        prepared: true,
        review: korylusfactory_core::state::ReviewTrail::default(),
        verification: Vec::new(),
    });
    state.wait = Some(Wait {
        kind: WaitKind::Report,
        reason: "作業報告書の確認を待っています".to_owned(),
        details: Vec::new(),
    });
    plan_dir(root)
        .lock()
        .expect("ロックを取れなかった")
        .save(&state)
        .expect("状態を保存できなかった");
    let cancel = AtomicBool::new(false);
    // 知らせを積んだ回のあとも、作業報告書の確認の待ちが続く回を動かす。
    let err = terminal_err(root, &wiki, &queue, &Clock::new(4, &cancel));

    assert_eq!(
        err.matches('\x07').count(),
        2,
        "作業報告書の確認と取り込まれていないことの知らせで、ベルが1回ずつではない: {err:?}"
    );
    assert_eq!(
        queue.notes(),
        [
            note(
                "作業報告書の確認",
                Some("https://wikino.app/s/shimbaco/pages/1/edit")
            ),
            note(
                "ブランチが取り込まれていない",
                Some("https://wikino.app/s/shimbaco/pages/2/edit")
            ),
        ],
        "知らせの通知に計画書名を渡していない"
    );
}

/// 送った状態の報告を記録し、決めたステータスを返す経路。
struct Recorded {
    status: u16,
    reports: std::sync::Mutex<Vec<(String, korylusfactory_protocol::StatusReport)>>,
    running: std::sync::mpsc::Sender<()>,
}

impl web::Transport for Recorded {
    fn put(&self, url: &str, body: &[u8]) -> Result<u16, String> {
        let report: korylusfactory_protocol::StatusReport =
            serde_json::from_slice(body).expect("送った本文が状態の報告ではない");
        let running = report.running;
        self.reports
            .lock()
            .expect("送った報告を記録できなかった")
            .push((url.to_owned(), report));
        if running {
            self.running
                .send(())
                .expect("実行中の報告を知らせられなかった");
        }
        Ok(self.status)
    }

    fn get(&self, _: &str) -> Result<(u16, Vec<u8>), String> {
        Ok((200, br#"{"paused": false}"#.to_vec()))
    }
}

/// ウェブアプリを設定したプロジェクトで監督のループを動かし、送った報告・進捗と診断・届かなかったときの警告を返す。
fn run_reporting(
    status: u16,
    sessions: impl FnOnce(&Wiki) -> Vec<Work<'_>>,
    stop_after: usize,
) -> (
    Vec<(String, korylusfactory_protocol::StatusReport)>,
    String,
    Vec<String>,
) {
    let repo = repo_on("main");
    let root = repo.root.as_path();
    write_file(
        root,
        config::CONFIG_PATH,
        br#"{"roadmap": "https://wikino.app/s/shimbaco/pages/1214", "web": {"url": "https://factory.example.com/"}}"#,
    );
    let wiki = wiki(root);
    let (running, started) = std::sync::mpsc::channel();
    let work = sessions(&wiki)
        .into_iter()
        .map(|work| {
            let started = &started;
            Box::new(move |wiki: &Wiki| {
                started
                    .recv_timeout(Duration::from_secs(5))
                    .expect("セッション中の報告を受け取れなかった");
                work(wiki)
            }) as Work<'_>
        })
        .collect();
    let sessions = Sessions::new(&wiki, work);
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(stop_after, &cancel);
    let transport = std::sync::Arc::new(Recorded {
        status,
        reports: std::sync::Mutex::new(Vec::new()),
        running,
    });
    let warnings = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let warned = std::sync::Arc::clone(&warnings);
    let link = web::Link {
        transport: transport.clone(),
        warn: std::sync::Arc::new(move |text: &str| {
            warned
                .lock()
                .expect("警告を記録できなかった")
                .push(text.to_owned());
        }),
        load_roadmap: std::sync::Arc::new(|_, _| Ok(Vec::new())),
        interval: web::INTERVAL,
    };
    let build = build();
    let start = Start {
        runner: &wiki,
        launcher: &sessions,
        clock: &clock,
        queues: &NoQueue,
        notifier: &NoQueue,
        terminal: false,
        time_zone: &TimeZone::UTC,
        build: &build,
        web: &link,
        root,
        timing: Timing::default(),
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    run(&start, &mut out, &mut err, clock.cancel).expect("監督のループがエラーで終わった");
    let reports = transport
        .reports
        .lock()
        .expect("送った報告を読めなかった")
        .clone();
    let warnings = warnings.lock().expect("警告を読めなかった").clone();
    (
        reports,
        format!(
            "{}{}",
            String::from_utf8(out).expect("進捗がUTF-8ではない"),
            String::from_utf8(err).expect("診断がUTF-8ではない")
        ),
        warnings,
    )
}

#[test]
fn ウェブアプリを設定するとセッションの前後の状態の報告を送る() {
    let (reports, out, warnings) = run_reporting(
        204,
        |_| {
            vec![Box::new(|_: &Wiki| {
                Some(json!({"needs_human": "設計を変えてよいか", "verification": []}))
            })]
        },
        2,
    );

    assert!(
        reports
            .iter()
            .all(|(url, _)| url == "https://factory.example.com/api/v1/director/status"),
        "送り先が違う: {reports:?}"
    );
    let running = reports
        .iter()
        .find(|(_, report)| report.running)
        .map(|(_, report)| report)
        .expect("セッションを始める前の報告を送っていない");
    let plan = &running.progress[0];
    assert_eq!(plan.name, "demo", "進行中の作業計画書の名前が違う:\n{out}");
    assert_eq!(plan.task.as_deref(), Some("1-1"), "進めているタスクが違う");
    assert_eq!(
        plan.step.as_ref().map(|step| step.label.as_str()),
        Some("実装"),
        "進めているステップが違う"
    );
    let tasks: Vec<(&str, korylusfactory_protocol::TaskStatus)> = plan
        .tasks
        .iter()
        .map(|task| (task.number.as_str(), task.status))
        .collect();
    assert_eq!(
        tasks,
        [
            ("1-1", korylusfactory_protocol::TaskStatus::InProgress),
            ("1-2", korylusfactory_protocol::TaskStatus::Pending)
        ],
        "タスクの一覧が違う"
    );
    let (_, last) = reports.last().expect("報告を送っていない");
    assert!(!last.running, "セッションのあとの報告を実行中のまま送った");
    assert_eq!(
        last.progress[0]
            .wait
            .as_ref()
            .map(|wait| wait.label.as_str()),
        Some("セッションからの質問"),
        "人間待ちを報告していない"
    );
    assert!(
        last.upcoming.is_empty(),
        "進行中の作業計画書をこれから進めるものとして送った: {:?}",
        last.upcoming
    );
    assert!(warnings.is_empty(), "届いたのに警告を出した: {warnings:?}");
}

#[test]
fn ウェブアプリが受け付けなくても作業計画書を進め_警告を1回だけ出す() {
    let (reports, out, warnings) = run_reporting(
        503,
        |_| {
            vec![Box::new(|_: &Wiki| {
                Some(json!({"needs_human": "設計を変えてよいか", "verification": []}))
            })]
        },
        2,
    );

    assert!(reports.len() >= 2, "報告を送り続けていない: {reports:?}");
    assert!(
        out.contains("demo: 人間を待っています (セッションからの質問)"),
        "ウェブアプリに届かないと作業計画書を進めなかった:\n{out}"
    );
    assert_eq!(
        warnings,
        [
            "ウェブアプリが状態の報告を受け付けませんでした (ステータス 503)。作業計画書は進め続けます"
        ],
        "警告の出し方が違う"
    );
}

#[test]
fn 取り消してあれば本物の時計は待たずに戻る() {
    let cancel = AtomicBool::new(true);

    SystemClock::new(&cancel).sleep_with(Duration::from_mins(10), |_| {
        panic!("取り消してあるのに待機を始めた");
    });
}

#[test]
fn 待機中に別のスレッドで取り消すと時計の待ちが終わる() {
    let cancel = AtomicBool::new(false);
    let timeout = Duration::from_secs(5);

    std::thread::scope(|scope| {
        let (waiting, entered) = mpsc::channel();
        let (cancelled, resume) = mpsc::channel();
        let cancel = &cancel;
        scope.spawn(move || {
            entered.recv_timeout(timeout).expect("待機を始めなかった");
            cancel.store(true, Ordering::SeqCst);
            cancelled.send(()).expect("取り消しを知らせられなかった");
        });

        let mut waited = false;
        SystemClock::new(cancel).sleep_with(Duration::from_mins(10), |_| {
            assert!(!waited, "取り消しを確認せずに次の待機に入った");
            waited = true;
            waiting.send(()).expect("待機の開始を知らせられなかった");
            resume
                .recv_timeout(timeout)
                .expect("取り消しの通知が届かなかった");
        });
        assert!(waited, "待機に入る前に戻った");
    });
}

/// 稼働の設定を読んだときに返す結果。
type SettingsResult = Result<(u16, Vec<u8>), String>;

/// 稼働を返す応答。
const RUNNING: &[u8] = br#"{"paused": false}"#;
/// 停止を返す応答。
const PAUSED: &[u8] = br#"{"paused": true}"#;

/// 本文bodyを返す、200の応答。
fn ok(body: &[u8]) -> (u16, Vec<u8>) {
    (200, body.to_vec())
}

/// 決めた順に稼働の設定を返し、送った状態の報告を記録する経路。最後の結果は繰り返し返す。
struct Operated {
    settings: std::sync::Mutex<VecDeque<SettingsResult>>,
    reads: std::sync::Mutex<usize>,
    reports: std::sync::Mutex<Vec<korylusfactory_protocol::StatusReport>>,
}

impl Operated {
    fn new(settings: Vec<SettingsResult>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            settings: std::sync::Mutex::new(settings.into()),
            reads: std::sync::Mutex::new(0),
            reports: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn reads(&self) -> usize {
        *self.reads.lock().expect("読んだ回数を読めなかった")
    }

    fn last_report(&self) -> korylusfactory_protocol::StatusReport {
        self.reports
            .lock()
            .expect("送った報告を読めなかった")
            .last()
            .cloned()
            .expect("状態の報告を送っていない")
    }
}

impl web::Transport for Operated {
    fn put(&self, _: &str, body: &[u8]) -> Result<u16, String> {
        let report = serde_json::from_slice(body).expect("送った本文が状態の報告ではない");
        self.reports
            .lock()
            .expect("送った報告を記録できなかった")
            .push(report);
        Ok(204)
    }

    fn get(&self, url: &str) -> Result<(u16, Vec<u8>), String> {
        assert_eq!(
            url, "https://factory.example.com/api/v1/director/settings",
            "設定を読むURLが違う"
        );
        *self.reads.lock().expect("読んだ回数を書けなかった") += 1;
        let mut settings = self.settings.lock().expect("設定の結果を読めなかった");
        if settings.len() > 1 {
            settings.pop_front().expect("設定の結果が無い")
        } else {
            settings.front().cloned().expect("設定の結果が無い")
        }
    }
}

/// ウェブアプリを設定したプロジェクト。プロジェクトのルートが、タスクのリポジトリを兼ねる。
fn web_project() -> (Repo, Wiki) {
    let repo = repo_on("main");
    let root = repo.root.as_path();
    write_file(
        root,
        config::CONFIG_PATH,
        br#"{"roadmap": "https://wikino.app/s/shimbaco/pages/1214", "web": {"url": "https://factory.example.com/"}}"#,
    );
    let wiki = wiki(root);
    (repo, wiki)
}

/// ウェブアプリから稼働の設定をtransportで読むプロジェクトrootで監督のループを動かし、進捗と診断・警告を返す。
fn run_operating(
    root: &Path,
    wiki: &Wiki,
    sessions: &dyn Launcher,
    clock: &Clock<'_>,
    queues: &dyn Open,
    transport: &std::sync::Arc<Operated>,
) -> (String, Vec<String>) {
    let warnings = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let warned = std::sync::Arc::clone(&warnings);
    let link = web::Link {
        transport: transport.clone(),
        warn: std::sync::Arc::new(move |text: &str| {
            warned
                .lock()
                .expect("警告を記録できなかった")
                .push(text.to_owned());
        }),
        load_roadmap: std::sync::Arc::new(|_, _| Ok(Vec::new())),
        interval: web::INTERVAL,
    };
    let build = build();
    let start = Start {
        runner: wiki,
        launcher: sessions,
        clock,
        queues,
        notifier: &NoQueue,
        terminal: false,
        time_zone: &TimeZone::UTC,
        build: &build,
        web: &link,
        root,
        timing: Timing::default(),
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    run(&start, &mut out, &mut err, clock.cancel).expect("監督のループがエラーで終わった");
    let warnings = warnings.lock().expect("警告を読めなかった").clone();
    (
        format!(
            "{}{}",
            String::from_utf8(out).expect("進捗がUTF-8ではない"),
            String::from_utf8(err).expect("診断がUTF-8ではない")
        ),
        warnings,
    )
}

const PAUSED_TEXT: &str = "ウェブアプリの設定で停止しています。稼働に戻されたら作業を再開します";
const RESUMED_TEXT: &str = "ウェブアプリの設定で稼働に戻されました。作業を再開します";

fn question() -> Work<'static> {
    Box::new(|_: &Wiki| Some(json!({"needs_human": "設計を変えてよいか", "verification": []})))
}

#[test]
fn 停止している間は作業計画書を始めず_1分ごとに読み直して稼働に戻ると始める() {
    let (repo, wiki) = web_project();
    let root = repo.root.as_path();
    let sessions = Sessions::new(&wiki, vec![question()]);
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(3, &cancel);
    let transport = Operated::new(vec![Ok(ok(PAUSED)), Ok(ok(PAUSED)), Ok(ok(RUNNING))]);

    let (out, warnings) = run_operating(root, &wiki, &sessions, &clock, &NoQueue, &transport);

    assert_eq!(
        clock.sleeps()[..2],
        [Duration::from_mins(1), Duration::from_mins(1)],
        "停止している間に1分ごとに読み直していない:\n{out}"
    );
    assert_eq!(
        out.matches(PAUSED_TEXT).count(),
        1,
        "停止を1回だけ表示していない:\n{out}"
    );
    assert!(
        out.contains(RESUMED_TEXT),
        "稼働に戻ったことを表示していない:\n{out}"
    );
    assert!(
        out.find(RESUMED_TEXT) < out.find("作業計画書 demo を始めます"),
        "稼働に戻る前に作業計画書を始めた:\n{out}"
    );
    assert_eq!(
        sessions.prompts().len(),
        1,
        "稼働に戻ってからセッションを起動していない"
    );
    assert!(
        !transport.last_report().paused,
        "稼働に戻ったのに停止として報告した"
    );
    assert_eq!(
        crate::operation::load(root)
            .expect("最後に読めた設定を読めなかった")
            .map(|saved| saved.paused),
        Some(false),
        "最後に読めた設定を書いていない"
    );
    assert!(warnings.is_empty(), "読めたのに警告を出した: {warnings:?}");
}

#[test]
fn セッションのあとに停止に切り替えると次のセッションを起動せず_停止として報告する() {
    let (repo, wiki) = web_project();
    let root = repo.root.as_path();
    let sessions = Sessions::new(
        &wiki,
        vec![Box::new(move |wiki: &Wiki| {
            commit(root, "state.txt");
            wiki.check("1-1");
            Some(implemented())
        })],
    );
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(1, &cancel);
    // 作業計画書を始める前と、実装のセッションの前は稼働で、レビューの前に停止に切り替わる。
    let transport = Operated::new(vec![Ok(ok(RUNNING)), Ok(ok(RUNNING)), Ok(ok(PAUSED))]);

    let (out, _) = run_operating(root, &wiki, &sessions, &clock, &NoQueue, &transport);

    assert_eq!(
        sessions.prompts().len(),
        1,
        "停止に切り替えたあとにセッションを起動した:\n{out}"
    );
    assert!(out.contains(PAUSED_TEXT), "停止を表示していない:\n{out}");
    assert_eq!(load(root).step, Some(Step::Review), "停止中に状態を進めた");
    let report = transport.last_report();
    assert!(
        report.paused && !report.running,
        "停止を反映したことを報告していない: {report:?}"
    );
    assert_eq!(
        crate::operation::load(root)
            .expect("最後に読めた設定を読めなかった")
            .map(|saved| saved.paused),
        Some(true),
        "読んだ停止の設定を書いていない"
    );
}

#[test]
fn 停止の設定でも人間キューで受け取った回答を適用し_次のセッションは起動しない() {
    let (repo, wiki) = web_project();
    let root = repo.root.as_path();
    write_file(
        root,
        config::CONFIG_PATH,
        br#"{
            "roadmap": "https://wikino.app/s/shimbaco/pages/1214",
            "project": "mewst",
            "queue": {"space": "shimbaco", "topic": 12},
            "web": {"url": "https://factory.example.com/"}
        }"#,
    );
    wiki.set(PLAN_PAGE, "**計画書名**: demo\n\n## タスクリスト\n\n- [ ] **1-1**: 下書きを作る\n  - **担当**: implementer\n".to_owned());
    let label = "セッションからの質問";
    let question = "設計を変えてよいか";
    let answer = "設計を変えて進めてください。";
    save_waiting(
        root,
        Wait {
            kind: WaitKind::Question,
            reason: "人間の判断を求めて終わりました".to_owned(),
            details: vec![question.to_owned()],
        },
        5,
        label,
        |state| {
            state.task = Some("1-1".to_owned());
            state.step = Some(Step::Work);
            state.snapshot = Some(wiki.page(PLAN_PAGE));
        },
    );
    let queue = Queue::default();
    queue.insert(
        5,
        Page::Draft {
            title: item_title(label),
            body: String::new(),
        },
    );
    let sessions = Sessions::new(&wiki, Vec::new());
    let cancel = AtomicBool::new(false);
    // 5秒ずつ人間を待ち、1分後に回答を読んでから停止を反映した回で終える。
    let clock = Clock::new(13, &cancel);
    clock.on_first_sleep(|| {
        queue.insert(
            5,
            Page::Published {
                title: item_title(label),
                body: format!("## 回答\n\n### 質問への回答\n\n{answer}\n"),
            },
        );
    });
    let transport = Operated::new(vec![Ok(ok(PAUSED))]);

    let (out, _) = run_operating(root, &wiki, &sessions, &clock, &queue, &transport);

    assert!(
        out.contains("demo: 人間を待っています (セッションからの質問)"),
        "人間待ちを表示していない:\n{out}"
    );
    assert_eq!(
        queue.calls(),
        ["read 5", "read 5"],
        "下書きと公開後の回答を読まなかった"
    );
    let state = load(root);
    assert!(state.wait.is_none(), "回答を適用して待ちを解かなかった");
    assert_eq!(
        state.answers,
        [korylusfactory_core::state::HumanAnswer {
            question: question.to_owned(),
            answer: answer.to_owned(),
        }],
        "人間キューから読んだ回答を記録しなかった"
    );
    assert!(
        out.contains(PAUSED_TEXT),
        "待ちが解けたあとに停止を表示していない:\n{out}"
    );
    assert_eq!(transport.reads(), 1, "人間を待っている間に設定を読んだ");
    assert!(
        transport.last_report().paused,
        "回答のあとに停止を報告していない"
    );
    assert!(
        sessions.prompts().is_empty(),
        "停止中にセッションを起動した"
    );
}

#[test]
fn 届かないときは最後に読めた停止の設定に従い_警告を1回だけ出す() {
    let (repo, wiki) = web_project();
    let root = repo.root.as_path();
    crate::operation::save(
        root,
        &crate::operation::Saved {
            agents: std::collections::BTreeMap::new(),
            paused: true,
            read_at: "2026-11-16T00:00:00Z".parse().expect("時刻を読めなかった"),
        },
    )
    .expect("最後に読めた設定を書けなかった");
    let sessions = Sessions::new(&wiki, Vec::new());
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(2, &cancel);
    let transport = Operated::new(vec![Err("接続できません".to_owned())]);

    let (out, warnings) = run_operating(root, &wiki, &sessions, &clock, &NoQueue, &transport);

    assert!(
        sessions.prompts().is_empty(),
        "最後に読めた停止の設定に従わなかった"
    );
    assert!(out.contains(PAUSED_TEXT), "停止を表示していない:\n{out}");
    assert_eq!(transport.reads(), 2, "1分ごとに読み直していない");
    assert_eq!(
        warnings,
        [
            "ウェブアプリから稼働の設定を読めません: 接続できません。最後に読めた停止の設定に従います"
        ],
        "警告の出し方が違う"
    );
    assert!(transport.last_report().paused, "停止を報告していない");
}

#[test]
fn 届かずに最後に読めた設定も無ければ稼働として進め_1分経つまで読み直さない() {
    let (repo, wiki) = web_project();
    let root = repo.root.as_path();
    let sessions = Sessions::new(&wiki, vec![question()]);
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(1, &cancel);
    let transport = Operated::new(vec![Ok((503, Vec::new()))]);

    let (out, warnings) = run_operating(root, &wiki, &sessions, &clock, &NoQueue, &transport);

    assert_eq!(
        sessions.prompts().len(),
        1,
        "届かないときに稼働として進めなかった:\n{out}"
    );
    assert_eq!(
        transport.reads(),
        1,
        "届かなかったあと、1分経つ前に読み直した"
    );
    assert_eq!(
        warnings,
        ["ウェブアプリが稼働の設定を返しませんでした (ステータス 503)。稼働として進めます"],
        "警告の出し方が違う"
    );
    assert!(!out.contains(PAUSED_TEXT), "停止を表示した:\n{out}");
    assert_eq!(
        crate::operation::load(root).expect("最後に読めた設定を読めなかった"),
        None,
        "読めなかった設定を書いた"
    );
}

#[test]
fn ウェブアプリを設定しなければ稼働の設定を読まずに進める() {
    let repo = project();
    let root = repo.root.as_path();
    let wiki = wiki(root);
    let sessions = Sessions::new(&wiki, vec![question()]);
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(1, &cancel);

    let (out, _) = loop_outputs(root, &wiki, &sessions, &clock, &build());

    assert_eq!(
        sessions.prompts().len(),
        1,
        "ウェブアプリが無いのに進めなかった:\n{out}"
    );
    assert!(
        !root.join(crate::operation::PATH).exists(),
        "ウェブアプリが無いのに最後に読めた設定を書いた"
    );
}

#[test]
fn 保存が失敗しても書けるようになれば最初に読んだ停止を保存する() {
    for next_read in [Ok(ok(PAUSED)), Err("通信できません".to_owned())] {
        let (repo, wiki) = web_project();
        let root = repo.root.as_path();
        let sessions = Sessions::new(&wiki, Vec::new());
        let cancel = AtomicBool::new(false);
        let clock = Clock::new(2, &cancel);
        let read_at = clock.now();
        let running = crate::operation::Saved {
            agents: std::collections::BTreeMap::new(),
            paused: false,
            read_at,
        };
        crate::operation::save(root, &running).expect("以前の稼働の設定を書けなかった");

        let temporary = root.join(crate::operation::PATH).with_extension("json.tmp");
        fs::create_dir(&temporary).expect("書けない一時ファイルのパスを用意できなかった");
        clock.on_first_sleep(move || {
            assert_eq!(
                crate::operation::load(root).expect("設定を読めなかった"),
                Some(running),
                "保存に失敗した設定がファイルに反映された"
            );
            fs::remove_dir(&temporary).expect("設定の保存先を戻せなかった");
        });
        let transport = Operated::new(vec![Ok(ok(PAUSED)), next_read]);
        let (_, warnings) = run_operating(root, &wiki, &sessions, &clock, &NoQueue, &transport);

        assert!(
            warnings
                .iter()
                .any(|text| text.contains("読んだ設定に従います")),
            "書けなかったときに警告しなかった"
        );
        assert_eq!(
            crate::operation::load(root).expect("書き直した設定を読めなかった"),
            Some(crate::operation::Saved {
                agents: std::collections::BTreeMap::new(),
                paused: true,
                read_at
            }),
            "保存先の回復後に、最初に読んだ時刻と停止を保存していない"
        );
        let cancel = AtomicBool::new(false);
        let restarted = Operated::new(vec![Err("通信できません".to_owned())]);
        run_operating(
            root,
            &wiki,
            &sessions,
            &Clock::new(1, &cancel),
            &NoQueue,
            &restarted,
        );

        assert!(
            restarted.last_report().paused,
            "届かないまま起動し直したときに停止を反映していない"
        );
        assert!(
            sessions.prompts().is_empty(),
            "停止中にセッションを起動した"
        );
    }
}

#[test]
fn 配列の応答を受け取っても最後に読んだ停止に従う() {
    let (repo, wiki) = web_project();
    let root = repo.root.as_path();
    let sessions = Sessions::new(&wiki, Vec::new());
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(2, &cancel);
    let saved = crate::operation::Saved {
        agents: std::collections::BTreeMap::new(),
        paused: true,
        read_at: clock.now(),
    };
    crate::operation::save(root, &saved).expect("以前の停止の設定を書けなかった");
    let transport = Operated::new(vec![Ok(ok(b"[]")), Ok(ok(b"[false]"))]);

    let (_, warnings) = run_operating(root, &wiki, &sessions, &clock, &NoQueue, &transport);

    assert_eq!(transport.reads(), 2, "両方の応答を読まなかった");
    assert_eq!(
        warnings,
        [
            "ウェブアプリから受け取った稼働の設定を読み取れません: JSONオブジェクトではありません。最後に読めた停止の設定に従います"
        ],
        "設定の形式が違うことを警告しなかった"
    );
    assert_eq!(
        crate::operation::load(root).expect("設定を読めなかった"),
        Some(saved),
        "停止の設定を書き換えた"
    );
    assert!(transport.last_report().paused, "停止を報告していない");
    assert!(
        sessions.prompts().is_empty(),
        "停止中にセッションを起動した"
    );
}

#[test]
fn 通信が終わった時刻を記録し_失敗から1分経つまで読み直さない() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    struct Delayed {
        transport: Arc<Operated>,
        seconds: AtomicU64,
    }

    impl web::Transport for Delayed {
        fn put(&self, url: &str, body: &[u8]) -> Result<u16, String> {
            self.transport.put(url, body)
        }

        fn get(&self, url: &str) -> SettingsResult {
            self.sleep(Duration::from_secs(10));
            self.transport.get(url)
        }
    }

    impl super::Clock for Delayed {
        fn now(&self) -> Timestamp {
            let seconds =
                i64::try_from(self.seconds.load(Ordering::SeqCst)).expect("経過時間が大きすぎる");
            Timestamp::from_second(seconds).expect("時刻にできなかった")
        }

        fn sleep(&self, duration: Duration) {
            self.seconds.fetch_add(duration.as_secs(), Ordering::SeqCst);
        }
    }

    let dir = tempfile::tempdir().expect("一時ディレクトリを作れなかった");
    let endpoint = web::Endpoint::parse("https://factory.example.com/").expect("URLを読めなかった");
    let transport = Operated::new(vec![Err("通信できません".to_owned()), Ok(ok(PAUSED))]);
    let clock = Arc::new(Delayed {
        transport: Arc::clone(&transport),
        seconds: AtomicU64::new(0),
    });
    let link = web::Link {
        transport: clock.clone(),
        warn: Arc::new(|_| {}),
        load_roadmap: Arc::new(|_, _| Ok(Vec::new())),
        interval: web::INTERVAL,
    };
    let mut operation = Operation::new(
        &endpoint,
        &link,
        dir.path().to_path_buf(),
        SignedDuration::from_mins(1),
    );

    assert!(!operation.paused(clock.as_ref()), "初回の失敗で停止した");
    clock.sleep(Duration::from_secs(59));
    assert!(
        !operation.paused(clock.as_ref()),
        "失敗から1分経つ前に停止を読んだ"
    );
    assert_eq!(transport.reads(), 1, "失敗から1分経つ前に読み直した");
    clock.sleep(Duration::from_secs(1));
    assert!(
        operation.paused(clock.as_ref()),
        "失敗から1分後に停止を読まなかった"
    );
    assert_eq!(
        crate::operation::load(dir.path()).expect("保存した設定を読めなかった"),
        Some(crate::operation::Saved {
            agents: std::collections::BTreeMap::new(),
            paused: true,
            read_at: Timestamp::from_second(80).expect("時刻にできなかった")
        }),
        "応答を読み終えた時刻を記録していない"
    );
}

/// 引数argsに、続けて並んだ値wantがあるかを返す。
fn has_args(args: &[String], want: &[&str]) -> bool {
    args.windows(want.len()).any(|window| window == want)
}

/// 実装と、1回目のレビュー (Approve) のセッションの作業。
fn implement_and_review(root: &Path) -> Vec<Work<'_>> {
    vec![
        Box::new(move |wiki: &Wiki| {
            commit(root, "state.txt");
            wiki.check("1-1");
            Some(implemented())
        }),
        Box::new(move |_: &Wiki| {
            review(root, "demo-1-1", 1, "Approve", "");
            Some(plain())
        }),
    ]
}

#[test]
fn 奇数回目と偶数回目のレビュアーを同じ設定で起動できる() {
    let (repo, wiki) = web_project();
    let root = repo.root.as_path();
    // 実装・1回目のレビュー・修正・2回目のレビューを動かす。
    let sessions = Sessions::new(&wiki, whole_plan(root).into_iter().take(4).collect());
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(1, &cancel);
    let implementer = br#"{"paused": false, "agents": {"implementer": {"agent": "codex", "model": "gpt-5.3-codex", "effort": "high"}}}"#;
    let reviewers = br#"{"paused": false, "agents": {"reviewer_odd": {"agent": "claude", "model": "claude-opus-5-5", "effort": "xhigh"}, "reviewer_even": {"agent": "claude", "model": "claude-opus-5-5", "effort": "xhigh"}}}"#;
    // 実装の前に作業者を、1回目のレビューの前にレビュアーを変え、2回目のあとに停止する。
    let transport = Operated::new(vec![
        Ok(ok(RUNNING)),
        Ok(ok(implementer)),
        Ok(ok(reviewers)),
        Ok(ok(reviewers)),
        Ok(ok(reviewers)),
        Ok(ok(PAUSED)),
    ]);

    let (out, warnings) = run_operating(root, &wiki, &sessions, &clock, &NoQueue, &transport);

    let commands = sessions.commands();
    assert_eq!(commands.len(), 4, "起動したセッションの数が違う:\n{out}");
    let (program, args) = &commands[0];
    assert_eq!(
        program, "codex",
        "作業者の設定のエージェントで起動していない"
    );
    assert!(
        has_args(args, &["-m", "gpt-5.3-codex"])
            && has_args(args, &["-c", "model_reasoning_effort=high"]),
        "作業者の設定のモデルとeffortを渡していない: {args:?}"
    );
    for (program, args) in &commands[1..] {
        assert_eq!(
            program, "claude",
            "レビュアーの設定のエージェントで起動していない"
        );
        assert!(
            has_args(args, &["--model", "claude-opus-5-5"])
                && has_args(args, &["--effort", "xhigh"]),
            "レビュアーの設定のモデルとeffortを渡していない: {args:?}"
        );
    }
    assert!(
        out.contains("demo: タスク 1-1 の実装を始めます (Codex・gpt-5.3-codex・high)。"),
        "使うエージェントとモデルとeffortを表示していない:\n{out}"
    );
    let state = load(root);
    let reviewers: Vec<_> = state.tasks[0]
        .review
        .reviews
        .iter()
        .map(|review| (review.number, review.role))
        .collect();
    assert_eq!(
        reviewers,
        [(1, Reviewer::Claude), (2, Reviewer::Claude)],
        "奇数回目と偶数回目のレビューを設定したレビュアーとして記録していない"
    );
    let first = &state.sessions[0];
    assert_eq!(
        (
            first.agent.as_str(),
            first.model.as_deref(),
            first.effort.as_deref()
        ),
        ("codex", Some("gpt-5.3-codex"), Some("high")),
        "使ったエージェントの設定を記録していない"
    );
    let saved = crate::operation::load(root)
        .expect("最後に読めた設定を読めなかった")
        .expect("最後に読めた設定が無い");
    assert!(
        saved.paused && saved.agents.is_empty(),
        "agentsの無い応答を読んだあとに、すべての枠を既定に戻した設定を書いていない: {saved:?}"
    );
    assert!(warnings.is_empty(), "読めたのに警告を出した: {warnings:?}");
}

#[test]
fn 実行中に変えたエージェントの設定は同じ枠の次回起動から使う() {
    let (repo, wiki) = web_project();
    let root = repo.root.as_path();
    let initial = br#"{"paused": false, "agents": {"implementer": {"agent": "claude", "model": "model-before", "effort": "low"}}}"#;
    let updated = br#"{"paused": false, "agents": {"implementer": {"agent": "codex", "model": "model-after", "effort": "high"}}}"#;
    let transport = Operated::new(vec![Ok(ok(initial))]);
    let reads_at_launch = Cell::new(0);
    let sessions = Sessions::new(
        &wiki,
        vec![
            Box::new(|_: &Wiki| {
                reads_at_launch.set(transport.reads());
                *transport.settings.lock().expect("設定を変更できなかった") =
                    VecDeque::from([Ok(ok(updated))]);
                let state = load(root);
                let running = state.sessions.last().expect("実行中の記録が無い");
                assert!(running.is_open(), "実行中の記録が閉じている");
                assert_eq!(
                    (
                        running.agent.as_str(),
                        running.model.as_deref(),
                        running.effort.as_deref()
                    ),
                    ("claude", Some("model-before"), Some("low")),
                    "実行中の記録が変更後の設定に変わった"
                );
                Some(json!({"needs_human": "確認してください", "verification": []}))
            }),
            question(),
        ],
    );
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(2, &cancel);
    clock.on_first_sleep(|| {
        assert_eq!(
            transport.reads(),
            reads_at_launch.get(),
            "実行中から人間待ちに入るまでに設定を読み直した"
        );
        resume_question(root, &wiki);
    });

    let (out, warnings) = run_operating(root, &wiki, &sessions, &clock, &NoQueue, &transport);

    let commands = sessions.commands();
    let [(first, first_args), (second, second_args)] = commands.as_slice() else {
        panic!("同じ枠を2回起動しなかった:\n{out}");
    };
    assert_eq!(first, "claude", "最初のエージェントが違う");
    assert!(
        has_args(first_args, &["--model", "model-before"])
            && has_args(first_args, &["--effort", "low"]),
        "最初のモデルかeffortが違う: {first_args:?}"
    );
    assert_eq!(second, "codex", "変更後のエージェントで起動していない");
    assert!(
        has_args(second_args, &["-m", "model-after"])
            && has_args(second_args, &["-c", "model_reasoning_effort=high"]),
        "変更後のモデルかeffortを渡していない: {second_args:?}"
    );
    let state = load(root);
    let recorded: Vec<_> = state
        .sessions
        .iter()
        .map(|session| {
            (
                session.agent.as_str(),
                session.model.as_deref(),
                session.effort.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        recorded,
        [
            ("claude", Some("model-before"), Some("low")),
            ("codex", Some("model-after"), Some("high")),
        ],
        "実際に起動した設定を記録していない"
    );
    assert!(
        warnings.is_empty(),
        "設定を読めたのに警告を出した: {warnings:?}"
    );
}

#[test]
fn 規則に合わない枠は警告して最後に読めたその枠の値を使い_不正な値を引数に渡さない() {
    let (repo, wiki) = web_project();
    let root = repo.root.as_path();
    let sessions = Sessions::new(&wiki, implement_and_review(root));
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(1, &cancel);
    let valid = br#"{"paused": false, "agents": {"implementer": {"agent": "codex", "model": "gpt-5.3-codex"}}}"#;
    let invalid = br#"{"paused": false, "agents": {"implementer": {"agent": "codex", "model": "--yolo"}, "reviewer_odd": {"agent": "codex", "effort": "-c x=1"}}}"#;
    let transport = Operated::new(vec![
        Ok(ok(valid)),
        Ok(ok(invalid)),
        Ok(ok(invalid)),
        Ok(ok(PAUSED)),
    ]);

    let (out, warnings) = run_operating(root, &wiki, &sessions, &clock, &NoQueue, &transport);

    let commands = sessions.commands();
    assert_eq!(commands.len(), 2, "起動したセッションの数が違う:\n{out}");
    let (program, args) = &commands[0];
    assert_eq!(
        program, "codex",
        "最後に読めた作業者のエージェントを使っていない"
    );
    assert!(
        has_args(args, &["-m", "gpt-5.3-codex"]),
        "最後に読めた作業者のモデルを使っていない: {args:?}"
    );
    let (program, args) = &commands[1];
    assert_eq!(
        program, "codex",
        "奇数回目のレビュアーを既定のエージェントにしなかった"
    );
    assert!(
        !has_args(args, &["-c"]),
        "検証を通らないeffortを引数に渡した: {args:?}"
    );
    for (_, args) in &commands {
        assert!(
            args.iter()
                .all(|arg| !arg.contains("yolo") && !arg.contains("x=1")),
            "検証を通らない値を引数に渡した: {args:?}"
        );
    }
    assert_eq!(
        warnings,
        [
            "ウェブアプリの担当ごとのエージェントの設定 (作業者) を使えません: モデルは英数字で始め、英数字と . _ - : [ ] だけを使って64文字以内にしてください。最後に読めたこの枠の設定に従います",
            "ウェブアプリの担当ごとのエージェントの設定 (レビュアー (奇数回目)) を使えません: effortは low・medium・high・xhigh・max のどれかにしてください。既定のエージェントで動かします",
        ],
        "同じ理由の警告を繰り返したか、警告が違う"
    );
}

#[test]
fn ウェブアプリに届かないときは最後に読めた担当ごとのエージェントに従う() {
    let (repo, wiki) = web_project();
    let root = repo.root.as_path();
    let mut saved = crate::operation::Saved {
        agents: std::collections::BTreeMap::new(),
        paused: false,
        read_at: "2026-11-16T00:00:00Z".parse().expect("時刻を読めなかった"),
    };
    saved.agents.insert(
        korylusfactory_protocol::Slot::Implementer,
        korylusfactory_protocol::AgentSetting {
            agent: korylusfactory_protocol::Agent::Codex,
            model: None,
            effort: Some(korylusfactory_protocol::Effort::Low),
        },
    );
    crate::operation::save(root, &saved).expect("最後に読めた設定を書けなかった");
    let sessions = Sessions::new(&wiki, vec![question()]);
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(1, &cancel);
    let transport = Operated::new(vec![Err("接続できません".to_owned())]);

    let (out, _) = run_operating(root, &wiki, &sessions, &clock, &NoQueue, &transport);

    let commands = sessions.commands();
    assert_eq!(commands.len(), 1, "起動したセッションの数が違う:\n{out}");
    let (program, args) = &commands[0];
    assert_eq!(
        program, "codex",
        "最後に読めた作業者のエージェントを使っていない"
    );
    assert!(
        has_args(args, &["-c", "model_reasoning_effort=low"]),
        "最後に読めた作業者のeffortを使っていない: {args:?}"
    );
}

/// 最初のセッションで決めた出力を書いて失敗し、以降はsessionsに任せるLauncher。
struct FailFirst<'s, 'a> {
    sessions: &'s Sessions<'a>,
    stdout: &'static str,
    stderr: &'static str,
    failed: Cell<bool>,
    /// 起動したコマンドと引数。
    commands: RefCell<Vec<(String, Vec<String>)>>,
}

impl Launcher for FailFirst<'_, '_> {
    fn launch(&self, session: &Session, cancel: &AtomicBool) -> Result<Ended, process::Error> {
        self.commands.borrow_mut().push((
            session.program.clone(),
            session
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
        ));
        if self.failed.replace(true) {
            return self.sessions.launch(session, cancel);
        }
        fs::write(&session.stdout, self.stdout).expect("標準出力を書けなかった");
        fs::write(&session.stderr, self.stderr).expect("標準エラーを書けなかった");
        Ok(Ended::Exited(1))
    }
}

#[test]
fn 設定した値をcliが受け付けなければやり直さずに人間を待ち_再開すると読み直した設定で起動する() {
    let (repo, wiki) = web_project();
    let root = repo.root.as_path();
    let sessions = Sessions::new(
        &wiki,
        vec![Box::new(move |wiki: &Wiki| {
            commit(root, "state.txt");
            wiki.check("1-1");
            Some(implemented())
        })],
    );
    let launcher = FailFirst {
        sessions: &sessions,
        stdout: r#"{"is_error": true, "result": "There's an issue with the selected model (no-such-model)."}"#,
        stderr: "[claude-code:unrecognized_model] {\"model\":\"no-such-model\"}\n",
        failed: Cell::new(false),
        commands: RefCell::new(Vec::new()),
    };
    let wrong = br#"{"paused": false, "agents": {"implementer": {"agent": "claude", "model": "no-such-model"}}}"#;
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(1, &cancel);
    let transport = Operated::new(vec![Ok(ok(wrong))]);

    let (out, _) = run_operating(root, &wiki, &launcher, &clock, &NoQueue, &transport);

    assert_eq!(
        launcher.commands.borrow().len(),
        1,
        "値を拒まれたセッションをやり直した:\n{out}"
    );
    let state = load(root);
    let wait = state.wait.clone().expect("人間を待っていない");
    assert_eq!(
        wait.kind,
        WaitKind::AgentSettings,
        "人間を待つ場面が違う: {out}"
    );
    assert_eq!(
        state.step,
        Some(Step::Implement),
        "同じステップのまま待っていない"
    );
    assert_eq!(state.retries, 0, "値を拒まれた失敗をやり直しの回数に数えた");
    assert_eq!(
        wait.details[..2],
        [
            "作業者: Claude Code・no-such-model・既定のeffort".to_owned(),
            "CLIの出力: [claude-code:unrecognized_model] {\"model\":\"no-such-model\"}".to_owned(),
        ],
        "枠と使った値とCLIの文面を待っている理由に書いていない"
    );
    assert_eq!(
        state.sessions.last().and_then(|session| session.result),
        Some(SessionResult::SettingsRejected),
        "セッションの記録の結果に残していない"
    );

    resume_question(root, &wiki);
    let fixed = br#"{"paused": false, "agents": {"implementer": {"agent": "claude", "model": "claude-opus-5-5"}}}"#;
    let cancel = AtomicBool::new(false);
    let clock = Clock::new(1, &cancel);
    let transport = Operated::new(vec![Ok(ok(fixed)), Ok(ok(PAUSED))]);

    let (out, _) = run_operating(root, &wiki, &launcher, &clock, &NoQueue, &transport);

    let commands = launcher.commands.borrow().clone();
    assert_eq!(commands.len(), 2, "再開したあとに起動していない:\n{out}");
    assert!(
        has_args(&commands[1].1, &["--model", "claude-opus-5-5"]),
        "再開したあとに読み直した設定で起動していない: {:?}",
        commands[1].1
    );
    assert_eq!(
        load(root).step,
        Some(Step::Review),
        "再開したあとに同じステップを終えていない"
    );
}

#[test]
fn codexの設定と無関係な失敗は同じステップをやり直す() {
    let configured = br#"{"paused": false, "agents": {"implementer": {"agent": "codex", "model": "gpt-5.3-codex", "effort": "max"}}}"#;
    let cases = [
        (
            "APIの別の項目の列挙値エラー",
            r#"{"type":"error","message":"[tools.0.type] [invalid_enum_value] Invalid value."}"#,
        ),
        (
            "通常の出力にモデル拒否の文面があった後のレート制限",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"Tested the diagnostic: model is not supported."}}
{"type":"error","message":"rate limited"}"#,
        ),
    ];
    for (case, stdout) in cases {
        let (repo, wiki) = web_project();
        let root = repo.root.as_path();
        let sessions = Sessions::new(&wiki, vec![question()]);
        let launcher = FailFirst {
            sessions: &sessions,
            stdout,
            stderr: "",
            failed: Cell::new(false),
            commands: RefCell::new(Vec::new()),
        };
        let cancel = AtomicBool::new(false);
        let clock = Clock::new(2, &cancel);
        let transport = Operated::new(vec![Ok(ok(configured))]);

        let (out, _) = run_operating(root, &wiki, &launcher, &clock, &NoQueue, &transport);

        let state = load(root);
        let sessions: Vec<_> = state
            .sessions
            .iter()
            .map(|session| (session.step, session.result))
            .collect();
        assert_eq!(
            sessions,
            [
                (Step::Implement, Some(SessionResult::Unfinished)),
                (Step::Implement, Some(SessionResult::Asked)),
            ],
            "{case}のあとに同じステップをやり直さなかった:\n{out}"
        );
        assert_eq!(
            clock.sleeps().first(),
            Some(&Timing::default().retry),
            "{case}のあとにやり直しの間隔を空けなかった"
        );
    }
}
