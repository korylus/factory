//! 管制のウェブアプリへ、プロジェクトの状態の報告を送り、人間が決めた稼働の設定を読む。
//!
//! `korylusfactory start` は、状態ファイルを書いたあとと一定の間隔ごとに、状態の報告を `PUT /api/v1/director/status` で送る。
//! 送るのは別のスレッド ([`Reporter`]) で行い、ウェブアプリに届かなくても作業計画書を進め続ける。
//! 届かないときは警告を出し、同じ理由の警告は届くようになるまで繰り返さない。
//!
//! 稼働の設定は、監督のループが作業を始める前に `GET /api/v1/director/settings` で読む ([`read_settings`])。

use std::collections::HashSet;
use std::fmt;
use std::io::{self, Write as _};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use korylusfactory_core::flow::{Plan, PlanDocument};
use korylusfactory_core::state::{self, PlanState, SessionRecord, SessionResult, Slot, Store};
use korylusfactory_protocol::{
    Outcome, PlanProgress, Role, Session, Settings, StatusReport, Step, Task, TaskStatus,
    UpcomingPlan, Version, Wait,
};
use serde::Serialize;
use ureq::http::Uri;

use crate::config::Config;
use crate::process::System;
use crate::roadmap::{self, Entry, State};
use crate::version::Build;

/// ウェブアプリのURLからの、状態の報告を受け取るAPIのパス。
pub const STATUS_PATH: &str = "/api/v1/director/status";

/// ウェブアプリのURLからの、稼働の設定を読むAPIのパス。
pub const SETTINGS_PATH: &str = "/api/v1/director/settings";

/// 稼働の設定の応答として読む本文の上限。設定は数十バイトのため、これを超える応答は設定として扱わない。
const SETTINGS_LIMIT: u64 = 64 * 1024;

/// 状態の報告を送る時間と、稼働の設定を読む時間の上限。
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// 状態が変わらなくても状態の報告を送り直す間隔。ウェブアプリは、5分を超えて届かないプロジェクトを不通とみなす。
pub const INTERVAL: Duration = Duration::from_mins(1);

/// 設定のURLから組み立てた、状態の報告を送るエンドポイントと、稼働の設定を読むエンドポイント。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    status: String,
    settings: String,
}

impl Endpoint {
    /// ホストのあるHTTP(S) URLを読み、パスに報告と設定のAPIを足す。
    pub fn parse(base: &str) -> Option<Self> {
        let uri: Uri = base.parse().ok()?;
        let authority = uri.authority()?;
        if !matches!(uri.scheme_str(), Some("http" | "https"))
            || authority.host().is_empty()
            || authority.as_str().contains('@')
            || (authority.as_str() != authority.host() && authority.port_u16().is_none())
            || uri.query().is_some()
            || base.contains('#')
        {
            return None;
        }
        let base = uri.path().trim_end_matches('/');
        let url = |api: &str| {
            let mut parts = uri.clone().into_parts();
            parts.path_and_query = Some(format!("{base}{api}").parse().ok()?);
            Uri::from_parts(parts).ok().map(|uri| uri.to_string())
        };
        Some(Self {
            status: url(STATUS_PATH)?,
            settings: url(SETTINGS_PATH)?,
        })
    }

    /// 報告を送るURL。
    pub fn as_str(&self) -> &str {
        &self.status
    }

    /// 稼働の設定を読むURL。
    pub fn settings(&self) -> &str {
        &self.settings
    }
}

/// ウェブアプリへの通信の経路。テストでは差し替えて、実際には通信しない。
pub trait Transport: Send + Sync {
    /// urlへ、JSONの本文bodyをPUTで送り、応答のステータスコードを返す。届かなければ理由を返す。
    fn put(&self, url: &str, body: &[u8]) -> Result<u16, String>;

    /// urlをGETで読み、応答のステータスコードと本文を返す。届かないか、本文を読めなければ理由を返す。
    fn get(&self, url: &str) -> Result<(u16, Vec<u8>), String>;
}

/// HTTPで送る本物の経路。
#[derive(Debug)]
pub struct Http {
    agent: ureq::Agent,
}

impl Http {
    /// 送る時間の上限をtimeoutにした経路を作る。
    pub fn new(timeout: Duration) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            // リダイレクト先のログイン画面を報告の受理と見間違えない。
            .max_redirects(0)
            // 4xxと5xxも、ステータスコードとして受け取って警告に書く。
            .http_status_as_error(false)
            .build()
            .into();
        Self { agent }
    }
}

impl Transport for Http {
    fn put(&self, url: &str, body: &[u8]) -> Result<u16, String> {
        self.agent
            .put(url)
            .header("Content-Type", "application/json")
            .send(body)
            .map(|response| response.status().as_u16())
            .map_err(|err| err.to_string())
    }

    fn get(&self, url: &str) -> Result<(u16, Vec<u8>), String> {
        let mut response = self
            .agent
            .get(url)
            .header("Accept", "application/json")
            .call()
            .map_err(|err| err.to_string())?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(SETTINGS_LIMIT)
            .read_to_vec()
            .map_err(|err| err.to_string())?;
        Ok((status, body))
    }
}

/// endpointから、linkの経路で稼働の設定を読む。読めなければ、警告に書く理由を返す。
pub fn read_settings(endpoint: &Endpoint, link: &Link) -> Result<Settings, String> {
    match link.transport.get(endpoint.settings()) {
        Ok((200, body)) => {
            // Serdeは構造体を配列からも読めるため、APIが返すオブジェクトに限る。
            if body.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
                return Err("ウェブアプリから受け取った稼働の設定を読み取れません: JSONオブジェクトではありません".to_owned());
            }
            serde_json::from_slice(&body).map_err(|err| {
                format!("ウェブアプリから受け取った稼働の設定を読み取れません: {err}")
            })
        }
        Ok((status, _)) => Err(format!(
            "ウェブアプリが稼働の設定を返しませんでした (ステータス {status})"
        )),
        Err(reason) => Err(format!("ウェブアプリから稼働の設定を読めません: {reason}")),
    }
}

/// 報告用にロードマップを読み直す処理。監督のループとは別のスレッドで呼ぶ。
pub type LoadRoadmap = dyn Fn(&Config, &Store) -> Result<Vec<Entry>, roadmap::Error> + Send + Sync;

/// 状態の報告に使う、外部とのやり取りと送信間隔。
#[derive(Clone)]
pub struct Link {
    /// ウェブアプリへの通信の経路。
    pub transport: Arc<dyn Transport>,
    /// 届かなかったときの警告を書く。送るスレッドから呼ぶ。
    pub warn: Arc<dyn Fn(&str) + Send + Sync>,
    /// 報告に載せるロードマップの並びを読み直す。
    pub load_roadmap: Arc<LoadRoadmap>,
    /// 状態が変わらなくても送り直す間隔。
    pub interval: Duration,
}

impl Link {
    /// HTTPで送り、警告を標準エラーに書く本物の経路。
    pub fn system() -> Self {
        Self {
            transport: Arc::new(Http::new(TIMEOUT)),
            warn: Arc::new(|text| {
                // 標準エラーに書けなくても、報告の送信は続ける。
                let _ = writeln!(io::stderr().lock(), "{text}");
            }),
            load_roadmap: Arc::new(|config, store| roadmap::load(&System, config, store)),
            interval: INTERVAL,
        }
    }
}

impl fmt::Debug for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Link")
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}

/// 状態の報告を、別のスレッドからウェブアプリへ送る。
///
/// 捨てると、処理中の送信と、未送信の最新の状態までを送り終えてからスレッドを終える。
#[derive(Debug)]
pub struct Reporter {
    sender: Option<mpsc::SyncSender<()>>,
    pending: Arc<Mutex<Option<StatusReport>>>,
    thread: Option<JoinHandle<()>>,
}

impl Reporter {
    /// 設定済みのエンドポイントへ、linkの経路で送るスレッドを起動する。
    pub fn spawn(endpoint: &Endpoint, link: &Link) -> io::Result<Self> {
        // チャネルは通知だけを渡し、送る内容は常に最新の1件だけを共有する。
        let (sender, receiver) = mpsc::sync_channel(1);
        let pending = Arc::new(Mutex::new(None));
        let incoming = Arc::clone(&pending);
        let mut outbox = Outbox::new(endpoint.as_str().to_owned(), link.clone());
        let interval = link.interval;
        let thread = thread::Builder::new()
            .name("factory-report".to_owned())
            .spawn(move || {
                loop {
                    let notification = match outbox.due_in(Instant::now(), interval) {
                        Some(wait) => receiver.recv_timeout(wait),
                        None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
                    };
                    match notification {
                        Ok(()) => {
                            let report = incoming
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .take();
                            if let Some(report) = report {
                                outbox.update(report);
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => outbox.resend(),
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            pending,
            thread: Some(thread),
        })
    }

    /// 最新の状態の報告reportを渡す。前に渡したものと変わっていれば、すぐに送る。
    pub fn update(&self, report: StatusReport) {
        if let Some(sender) = &self.sender {
            *self.pending.lock().unwrap_or_else(PoisonError::into_inner) = Some(report);
            // 既に通知があるときは、その通知で最新の報告が読まれる。
            // 送るスレッドが終わっていても、監督のループは進める。
            let _ = sender.try_send(());
        }
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        // 送り口を閉じると、スレッドは残っている通知を処理してから終わる。
        self.sender = None;
        if let Some(thread) = self.thread.take() {
            // 送るスレッドがパニックしていても、終わる処理は続ける。
            let _ = thread.join();
        }
    }
}

/// 送るスレッドが持つ、最新の報告と、送った記録。
struct Outbox {
    endpoint: String,
    link: Link,
    /// 最後に渡された報告。`sent_at` は送るときに埋める。
    latest: Option<StatusReport>,
    /// 最後に送った時刻。
    sent: Option<Instant>,
    /// 最後に届いてから出した警告。
    warned: HashSet<String>,
}

impl Outbox {
    fn new(endpoint: String, link: Link) -> Self {
        Self {
            endpoint,
            link,
            latest: None,
            sent: None,
            warned: HashSet::new(),
        }
    }

    /// 次に送り直すまでの時間を返す。まだ報告が無ければ `None` を返す。
    fn due_in(&self, now: Instant, interval: Duration) -> Option<Duration> {
        self.latest.as_ref()?;
        Some(self.sent.map_or(Duration::ZERO, |sent| {
            (sent + interval).saturating_duration_since(now)
        }))
    }

    /// 最新の報告をreportにする。前の報告から変わっていれば、すぐに送る。
    fn update(&mut self, report: StatusReport) {
        if self.latest.as_ref() == Some(&report) {
            return;
        }
        self.latest = Some(report);
        self.resend();
    }

    /// 最新の報告を、送った時刻を埋めて送る。
    fn resend(&mut self) {
        let Some(latest) = &self.latest else {
            return;
        };
        let mut report = latest.clone();
        report.sent_at = Timestamp::now().to_string();
        self.sent = Some(Instant::now());
        let result = match serde_json::to_vec(&report) {
            Ok(body) => self.link.transport.put(&self.endpoint, &body),
            Err(err) => Err(err.to_string()),
        };
        let warning = match result {
            Ok(204) => None,
            Ok(status) => Some(format!(
                "ウェブアプリが状態の報告を受け付けませんでした (ステータス {status})。作業計画書は進め続けます"
            )),
            Err(reason) => Some(format!(
                "ウェブアプリに状態の報告を送れません: {reason}。作業計画書は進め続けます"
            )),
        };
        if let Some(text) = warning {
            if self.warned.insert(text.clone()) {
                (self.link.warn)(&text);
            }
        } else {
            self.warned.clear();
        }
    }
}

/// 状態の報告を作る。`sent_at` は空のままにし、送るときに埋める。
///
/// buildは実行しているkorylusfactoryの版、runningはセッションを実行しているか、pausedは停止を反映して待っているか、
/// progressは進行中の作業計画書の計画書名と状態、entriesはロードマップのこれから進める作業計画書の並び。
pub fn report(
    build: &Build,
    running: bool,
    paused: bool,
    progress: &[(String, PlanState)],
    entries: &[Entry],
) -> StatusReport {
    let names: Vec<&str> = progress.iter().map(|(name, _)| name.as_str()).collect();
    let upcoming = entries
        .iter()
        .filter(|entry| match &entry.state {
            State::Ready { name, .. } => !names.contains(&name.as_str()),
            _ => true,
        })
        .map(|entry| UpcomingPlan {
            title: entry.candidate.link.title.clone(),
            url: match &entry.state {
                State::Ready { url, .. } | State::Invalid { url, .. } => Some(url.clone()),
                State::NotFound => None,
            },
        })
        .collect();
    StatusReport {
        sent_at: String::new(),
        korylusfactory: Version {
            version: build.version.clone(),
            commit: build.commit.clone(),
        },
        running,
        paused,
        progress: progress
            .iter()
            .map(|(name, state)| plan_progress(name, state))
            .collect(),
        upcoming,
    }
}

/// 進行中の作業計画書nameの状態stateを、報告の形にする。
///
/// タスクの一覧は、状態ファイルに残した作業計画書の本文 (タスクを準備したときのもの) から作る。
/// 送るたびに作業計画書を読みに行かないためで、本文がまだ無ければ空にする。
fn plan_progress(name: &str, state: &PlanState) -> PlanProgress {
    let tasks = state
        .snapshot
        .as_deref()
        .and_then(|body| PlanDocument::parse(body).ok())
        .map_or_default(|doc| tasks(doc.plan(), state.task.as_deref()));
    PlanProgress {
        name: name.to_owned(),
        title: state.title.clone(),
        url: state.url.clone(),
        task: state.task.clone(),
        step: state.step.map(|step| Step {
            kind: kind(&step),
            label: step.label().to_owned(),
        }),
        wait: state.wait.as_ref().map(|wait| Wait {
            kind: kind(&wait.kind),
            label: wait.kind.label().to_owned(),
            reason: wait.reason.clone(),
        }),
        tasks,
        sessions: state
            .sessions
            .iter()
            .rev()
            .take(RECENT_SESSIONS)
            .map(session)
            .collect(),
    }
}

/// 状態の報告に載せる、作業計画書ごとの直近のセッションの数。
const RECENT_SESSIONS: usize = 5;

/// 状態ファイルのセッションの記録recordを、状態の報告のセッションにする。
fn session(record: &SessionRecord) -> Session {
    let (role, label) = match record.slot {
        Slot::Implementer => ("implementer", "作業者"),
        Slot::ReviewerOdd => ("reviewer_odd", "レビュアー"),
        Slot::ReviewerEven => ("reviewer_even", "レビュアー"),
    };
    Session {
        task: record.task.clone(),
        step: Step {
            kind: kind(&record.step),
            label: record.step.label().to_owned(),
        },
        review: record.review,
        role: Role {
            kind: role.to_owned(),
            label: label.to_owned(),
        },
        agent: record.agent.clone(),
        model: record.model.clone(),
        effort: record.effort.clone(),
        started_at: record.started_at.to_string(),
        ended_at: record.ended_at.map(|at| at.to_string()),
        result: record.result.map(|result| Outcome {
            kind: kind(&result),
            label: result_label(result, record.verdict),
        }),
    }
}

/// セッションの結果の表示名。レビューを記録したときは、その総合評価 (例: `Request Changes`) にする。
fn result_label(result: SessionResult, verdict: Option<state::Verdict>) -> String {
    match (result, verdict) {
        (SessionResult::Recorded, Some(verdict)) => verdict.as_str().to_owned(),
        (result, _) => result.label().to_owned(),
    }
}

/// 作業計画書planのタスクの一覧を、進めているタスクcurrentとチェックから進み具合を決めて返す。
fn tasks(plan: &Plan, current: Option<&str>) -> Vec<Task> {
    plan.tasks
        .iter()
        .map(|task| Task {
            number: task.id.clone(),
            title: task.title.clone(),
            status: if task.done {
                TaskStatus::Done
            } else if current == Some(task.id.as_str()) {
                TaskStatus::InProgress
            } else {
                TaskStatus::Pending
            },
        })
        .collect()
}

/// 状態ファイルに書く値 (`snake_case` の文字列) を返す。
fn kind(value: &impl Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(text)) => text,
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use korylusfactory_core::queue::{Candidate, WikiLink};
    use korylusfactory_core::state::{self, WaitKind};

    use super::*;

    /// 設定を読んだときに返す結果。
    type Answer = Result<(u16, Vec<u8>), String>;

    /// 送った本文を記録し、決めた結果を返す経路。
    struct Recorder {
        results: Mutex<Vec<Result<u16, String>>>,
        sent: Mutex<Vec<(String, StatusReport)>>,
        /// 設定を読んだときに返す結果と、読んだURL。
        settings: Mutex<(Option<Answer>, Vec<String>)>,
    }

    impl Recorder {
        fn new(results: Vec<Result<u16, String>>) -> Arc<Self> {
            Arc::new(Self {
                results: Mutex::new(results),
                sent: Mutex::new(Vec::new()),
                settings: Mutex::new((None, Vec::new())),
            })
        }

        fn answering(result: Result<(u16, &[u8]), &str>) -> Arc<Self> {
            let recorder = Self::new(Vec::new());
            recorder.settings.lock().expect("結果を書けなかった").0 = Some(
                result
                    .map(|(status, body)| (status, body.to_vec()))
                    .map_err(str::to_owned),
            );
            recorder
        }

        fn sent(&self) -> Vec<(String, StatusReport)> {
            self.sent.lock().expect("記録を読めなかった").clone()
        }
    }

    impl Transport for Recorder {
        fn put(&self, url: &str, body: &[u8]) -> Result<u16, String> {
            let report = serde_json::from_slice(body).expect("送った本文が報告ではない");
            self.sent
                .lock()
                .expect("記録に書けなかった")
                .push((url.to_owned(), report));
            let mut results = self.results.lock().expect("結果を読めなかった");
            if results.is_empty() {
                Ok(204)
            } else {
                results.remove(0)
            }
        }

        fn get(&self, url: &str) -> Result<(u16, Vec<u8>), String> {
            let mut settings = self.settings.lock().expect("結果を読めなかった");
            settings.1.push(url.to_owned());
            settings
                .0
                .clone()
                .unwrap_or_else(|| Ok((200, br#"{"paused": false}"#.to_vec())))
        }
    }

    fn link(transport: Arc<Recorder>, warnings: Arc<Mutex<Vec<String>>>) -> Link {
        Link {
            transport,
            warn: Arc::new(move |text| {
                warnings
                    .lock()
                    .expect("警告を書けなかった")
                    .push(text.to_owned());
            }),
            load_roadmap: Arc::new(|_, _| Ok(Vec::new())),
            interval: INTERVAL,
        }
    }

    fn build() -> Build {
        Build {
            version: "0.1.0".to_owned(),
            commit: "abc1234".to_owned(),
            commit_short: "abc1234".to_owned(),
            date: "2026-11-19".to_owned(),
            source_dir: std::path::PathBuf::new(),
        }
    }

    fn sample(running: bool) -> StatusReport {
        report(&build(), running, false, &[], &[])
    }

    #[test]
    fn 検証したurlのパスに報告先と設定の読み取り先を足す() {
        for (base, status, settings) in [
            (
                "https://factory.example.com",
                "https://factory.example.com/api/v1/director/status",
                "https://factory.example.com/api/v1/director/settings",
            ),
            (
                "http://[::1]:8080/factory/",
                "http://[::1]:8080/factory/api/v1/director/status",
                "http://[::1]:8080/factory/api/v1/director/settings",
            ),
        ] {
            let endpoint = Endpoint::parse(base);
            assert_eq!(
                endpoint.as_ref().map(Endpoint::as_str),
                Some(status),
                "報告の送り先が違う"
            );
            assert_eq!(
                endpoint.as_ref().map(Endpoint::settings),
                Some(settings),
                "設定の読み取り先が違う"
            );
        }
        for invalid in [
            "https://",
            "/factory",
            "https://factory.example.com:abc",
            "https://factory.example.com:99999",
            "https://factory.example.com/?view=projects",
            "https://factory.example.com/#projects",
            "https://user@factory.example.com/",
        ] {
            assert!(
                Endpoint::parse(invalid).is_none(),
                "使えないURLを受け入れた: {invalid}"
            );
        }
    }

    #[test]
    fn 稼働の設定を読み_読めなければ理由を返す() {
        let endpoint = Endpoint::parse("https://factory.example.com").expect("URLを読めなかった");
        let read = |result| {
            let recorder = Recorder::answering(result);
            let got = read_settings(
                &endpoint,
                &link(Arc::clone(&recorder), Arc::new(Mutex::new(Vec::new()))),
            );
            let urls = recorder
                .settings
                .lock()
                .expect("記録を読めなかった")
                .1
                .clone();
            assert_eq!(
                urls,
                ["https://factory.example.com/api/v1/director/settings"],
                "読んだURLが違う"
            );
            got
        };

        assert_eq!(
            read(Ok((200, br#"{"paused": true}"#))),
            Ok(Settings {
                paused: true,
                ..Settings::default()
            }),
            "停止の設定を読めなかった"
        );
        assert_eq!(
            read(Ok((404, b""))),
            Err("ウェブアプリが稼働の設定を返しませんでした (ステータス 404)".to_owned()),
            "200以外の応答の理由が違う"
        );
        assert_eq!(
            read(Ok((200, b" \n{\"future\": 1}"))),
            Ok(Settings::default()),
            "停止を省いたオブジェクトを稼働として扱わなかった"
        );
        for body in [b"<html>".as_slice(), b"[]", b"[false]", b"{"] {
            assert!(
                read(Ok((200, body))).is_err_and(|reason| reason
                    .starts_with("ウェブアプリから受け取った稼働の設定を読み取れません: ")),
                "読めない本文 {body:?} を設定として扱った"
            );
        }
        assert_eq!(
            read(Err("接続できません")),
            Err("ウェブアプリから稼働の設定を読めません: 接続できません".to_owned()),
            "届かないときの理由が違う"
        );
    }

    #[test]
    fn 変わった報告だけをすぐ送り_送った時刻を埋める() {
        let recorder = Recorder::new(Vec::new());
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let mut outbox = Outbox::new(
            "https://factory.example.com/api/v1/director/status".to_owned(),
            link(Arc::clone(&recorder), Arc::clone(&warnings)),
        );

        outbox.update(sample(true));
        outbox.update(sample(true));
        outbox.update(sample(false));

        let sent = recorder.sent();
        let running: Vec<bool> = sent.iter().map(|(_, report)| report.running).collect();
        assert_eq!(running, [true, false], "変わっていない報告も送った");
        assert!(
            sent[0].1.sent_at.parse::<Timestamp>().is_ok(),
            "送った時刻を埋めていない: {:?}",
            sent[0].1.sent_at
        );
        assert_eq!(
            sent[0].0, "https://factory.example.com/api/v1/director/status",
            "送り先が違う"
        );
    }

    #[test]
    fn 送り直しの時刻は最後に送ってから間隔が過ぎたとき() {
        let recorder = Recorder::new(Vec::new());
        let mut outbox = Outbox::new(
            "https://factory.example.com/api/v1/director/status".to_owned(),
            link(recorder, Arc::new(Mutex::new(Vec::new()))),
        );
        let now = Instant::now();

        assert_eq!(outbox.due_in(now, INTERVAL), None, "報告が無いのに送り直す");
        outbox.update(sample(true));
        let sent = outbox.sent.expect("送った時刻を記録していない");

        assert_eq!(
            outbox.due_in(sent, INTERVAL),
            Some(INTERVAL),
            "送った直後の待ち時間が間隔と違う"
        );
        assert_eq!(
            outbox.due_in(sent + INTERVAL * 2, INTERVAL),
            Some(Duration::ZERO),
            "間隔が過ぎても送り直さない"
        );
    }

    #[test]
    fn 届かないときは同じ理由の警告を届くようになるまで繰り返さない() {
        let recorder = Recorder::new(vec![
            Err("接続できません".to_owned()),
            Ok(503),
            Err("接続できません".to_owned()),
            Ok(200),
            Ok(302),
            Err("接続できません".to_owned()),
            Ok(204),
            Err("接続できません".to_owned()),
        ]);
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let mut outbox = Outbox::new(
            "https://factory.example.com/api/v1/director/status".to_owned(),
            link(recorder, Arc::clone(&warnings)),
        );

        outbox.update(sample(true));
        for _ in 0..7 {
            outbox.resend();
        }

        assert_eq!(
            *warnings.lock().expect("警告を読めなかった"),
            [
                "ウェブアプリに状態の報告を送れません: 接続できません。作業計画書は進め続けます",
                "ウェブアプリが状態の報告を受け付けませんでした (ステータス 503)。作業計画書は進め続けます",
                "ウェブアプリが状態の報告を受け付けませんでした (ステータス 200)。作業計画書は進め続けます",
                "ウェブアプリが状態の報告を受け付けませんでした (ステータス 302)。作業計画書は進め続けます",
                "ウェブアプリに状態の報告を送れません: 接続できません。作業計画書は進め続けます",
            ],
            "警告の出し方が違う"
        );
    }

    #[test]
    fn 渡した報告は捨てる前に別のスレッドから送り終える() {
        let recorder = Recorder::new(Vec::new());
        let reporter = Reporter::spawn(
            &Endpoint::parse("https://factory.example.com/").expect("URLを読めなかった"),
            &link(Arc::clone(&recorder), Arc::new(Mutex::new(Vec::new()))),
        )
        .expect("送信スレッドを起動できなかった");

        reporter.update(sample(true));
        drop(reporter);

        let sent = recorder.sent();
        assert_eq!(sent.len(), 1, "渡した報告を送らなかった");
        assert_eq!(
            sent[0].0, "https://factory.example.com/api/v1/director/status",
            "URLの末尾の / を除いて送り先を作っていない"
        );
    }

    #[test]
    fn 送信中に渡した状態は最後の1件だけ送り終えてから閉じる() {
        struct BlockFirst {
            recorder: Arc<Recorder>,
            started: mpsc::Sender<()>,
            release: Mutex<Option<mpsc::Receiver<()>>>,
        }

        impl Transport for BlockFirst {
            fn put(&self, url: &str, body: &[u8]) -> Result<u16, String> {
                if let Some(release) = self.release.lock().expect("送信を揃えられなかった").take()
                {
                    self.started
                        .send(())
                        .expect("最初の送信を知らせられなかった");
                    release
                        .recv_timeout(Duration::from_secs(5))
                        .expect("最初の送信を再開できなかった");
                }
                self.recorder.put(url, body)
            }

            fn get(&self, url: &str) -> Result<(u16, Vec<u8>), String> {
                self.recorder.get(url)
            }
        }

        let (started, first) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let recorder = Recorder::new(Vec::new());
        let mut link = link(Arc::clone(&recorder), Arc::new(Mutex::new(Vec::new())));
        link.transport = Arc::new(BlockFirst {
            recorder: Arc::clone(&recorder),
            started,
            release: Mutex::new(Some(wait)),
        });
        let endpoint = Endpoint::parse("https://factory.example.com").expect("URLを読めなかった");
        let reporter = Reporter::spawn(&endpoint, &link).expect("送信スレッドを起動できなかった");
        reporter.update(sample(true));
        first
            .recv_timeout(Duration::from_secs(5))
            .expect("最初の送信が始まらなかった");

        reporter.update(sample(false));
        let mut latest = sample(false);
        latest.korylusfactory.commit = "latest".to_owned();
        reporter.update(latest);
        release.send(()).expect("送信を再開できなかった");
        drop(reporter);

        let sent = recorder.sent();
        assert_eq!(
            sent.len(),
            2,
            "古い報告を送ったか、最後の報告を送らなかった"
        );
        assert_eq!(
            sent[1].1.korylusfactory.commit, "latest",
            "最新の報告を送らなかった"
        );
    }

    fn entry(title: &str, state: State) -> Entry {
        Entry {
            candidate: Candidate {
                section: "やってる".to_owned(),
                link: WikiLink {
                    topic: None,
                    title: title.to_owned(),
                },
            },
            state,
        }
    }

    #[test]
    fn 状態ファイルとロードマップから報告を作る() {
        let mut state = PlanState::new(
            "https://wikino.example.com/s/example/pages/20",
            "デモ 作業計画書".to_owned(),
        );
        state.task = Some("1-2".to_owned());
        state.step = Some(state::Step::Review);
        state.wait = Some(state::Wait {
            kind: WaitKind::Question,
            reason: "設計を変えてよいか".to_owned(),
            details: Vec::new(),
        });
        state.snapshot = Some(
            "**計画書名**: demo\n\n## タスクリスト\n\n- [x] **1-1**: 作る\n- [ ] **1-2**: 表示する\n- [ ] **1-3**: 送る\n"
                .to_owned(),
        );
        let entries = [
            entry(
                "デモ 作業計画書",
                State::Ready {
                    url: "https://wikino.example.com/s/example/pages/20".to_owned(),
                    name: "demo".to_owned(),
                },
            ),
            entry("次 作業計画書", State::NotFound),
        ];

        let got = report(
            &build(),
            true,
            false,
            &[("demo".to_owned(), state)],
            &entries,
        );

        let value = serde_json::to_value(&got).expect("JSONにできなかった");
        assert_eq!(
            value,
            serde_json::json!({
                "sent_at": "",
                "korylusfactory": {"version": "0.1.0", "commit": "abc1234"},
                "running": true,
                "paused": false,
                "progress": [{
                    "name": "demo",
                    "title": "デモ 作業計画書",
                    "url": "https://wikino.example.com/s/example/pages/20",
                    "task": "1-2",
                    "step": {"kind": "review", "label": "レビュー"},
                    "wait": {"kind": "question", "label": "セッションからの質問", "reason": "設計を変えてよいか"},
                    "tasks": [
                        {"number": "1-1", "title": "作る", "status": "done"},
                        {"number": "1-2", "title": "表示する", "status": "in_progress"},
                        {"number": "1-3", "title": "送る", "status": "pending"}
                    ],
                    "sessions": []
                }],
                "upcoming": [{"title": "次 作業計画書"}]
            }),
            "報告の中身が違う"
        );
    }

    #[test]
    fn 報告には直近5件のセッションを新しい順に載せ_レビューの結果は評価にする() {
        let mut state = PlanState::new(
            "https://wikino.example.com/s/example/pages/20",
            "デモ 作業計画書".to_owned(),
        );
        let at = |text: &str| text.parse::<Timestamp>().expect("日時を読めなかった");
        let record = |minute: u32| state::SessionRecord {
            task: Some("1-1".to_owned()),
            step: state::Step::Implement,
            review: None,
            slot: Slot::Implementer,
            agent: "claude".to_owned(),
            model: None,
            effort: None,
            started_at: at(&format!("2026-11-20T08:{minute:02}:00Z")),
            ended_at: Some(at(&format!("2026-11-20T08:{minute:02}:30Z"))),
            result: Some(SessionResult::Recorded),
            verdict: None,
        };
        state.sessions = (0..5).map(record).collect();
        state.sessions.push(state::SessionRecord {
            step: state::Step::Review,
            review: Some(2),
            slot: Slot::ReviewerEven,
            agent: "codex".to_owned(),
            model: Some("gpt-5.3-codex".to_owned()),
            effort: Some("high".to_owned()),
            verdict: Some(state::Verdict::RequestChanges),
            ..record(10)
        });
        state.sessions.push(state::SessionRecord {
            step: state::Step::Fix,
            review: Some(2),
            slot: Slot::ReviewerEven,
            ended_at: None,
            result: None,
            ..record(20)
        });

        let progress = plan_progress("demo", &state);

        let value = serde_json::to_value(&progress.sessions).expect("JSONにできなかった");
        assert_eq!(
            value.as_array().map(Vec::len),
            Some(5),
            "直近5件に絞らなかった"
        );
        assert_eq!(
            value[0],
            serde_json::json!({
                "task": "1-1",
                "step": {"kind": "fix", "label": "修正"},
                "review": 2,
                "role": {"kind": "reviewer_even", "label": "レビュアー"},
                "agent": "claude",
                "started_at": "2026-11-20T08:20:00Z"
            }),
            "実行中のセッションの形が違う"
        );
        assert_eq!(
            value[1],
            serde_json::json!({
                "task": "1-1",
                "step": {"kind": "review", "label": "レビュー"},
                "review": 2,
                "role": {"kind": "reviewer_even", "label": "レビュアー"},
                "agent": "codex",
                "model": "gpt-5.3-codex",
                "effort": "high",
                "started_at": "2026-11-20T08:10:00Z",
                "ended_at": "2026-11-20T08:10:30Z",
                "result": {"kind": "recorded", "label": "Request Changes"}
            }),
            "レビューのセッションの形が違う"
        );
        assert_eq!(
            value[2]["role"],
            serde_json::json!({"kind": "implementer", "label": "作業者"}),
            "作業者の担当の形が違う"
        );
        assert_eq!(
            value[2]["result"],
            serde_json::json!({"kind": "recorded", "label": "記録した"}),
            "レビュー以外の結果の表示名が違う"
        );
        assert_eq!(
            value[4]["started_at"], "2026-11-20T08:02:00Z",
            "古いセッションから省かなかった"
        );
    }
}
