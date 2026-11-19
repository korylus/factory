//! プロジェクトの状態の決め方と、プロジェクトの一覧に出す中身。
//!
//! 状態は、ディレクターから最後に受け取った報告と、受け取った時刻から決める。

use std::collections::HashMap;
use std::fmt::Write as _;
use std::hash::BuildHasher;

use axum::http::Uri;
use jiff::{SignedDuration, Timestamp};
use korylusfactory_protocol::{PlanProgress, StatusReport, TaskStatus};
use rusqlite::{OptionalExtension as _, Row};

use crate::config::Director;
use crate::db::{self, Database};

/// 最後の報告からこの時間を超えて報告が届いていないプロジェクトを、不通とみなす。
pub const UNREACHABLE_AFTER: SignedDuration = SignedDuration::from_mins(5);

/// プロジェクトの状態。並べる順 (対応が要るものから) に定義する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    /// 進行中の作業計画書のどれかが、人間を待っている。
    Waiting,
    /// 報告が届いていない。
    Unreachable,
    /// セッションを実行しているか、進行中の作業計画書がある。
    Running,
    /// 進めている作業計画書が無い。
    Idle,
}

impl State {
    /// 画面に出す文言。
    pub fn label(self) -> &'static str {
        match self {
            State::Waiting => "人間待ち",
            State::Unreachable => "不通",
            State::Running => "実行中",
            State::Idle => "計画書なし",
        }
    }

    /// 状態の Badge の `data-variant`。
    pub fn variant(self) -> &'static str {
        match self {
            State::Waiting => "warning",
            State::Unreachable => "destructive",
            State::Running => "info",
            State::Idle => "secondary",
        }
    }
}

/// ディレクターから最後に受け取った報告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    /// 報告の中身。
    pub report: StatusReport,
    /// 受け取った時刻。
    pub received_at: Timestamp,
}

/// プロジェクトの一覧の1行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// プロジェクトの名前。
    pub name: String,
    /// プロジェクトの状態。
    pub state: State,
    /// 状態の Badge に添える計画書名と詳細。
    pub parts: Vec<Part>,
}

/// 状態に添える中身の1つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    /// 計画書名。長いときは画面で省略する。
    PlanTitle(String),
    /// 状態の詳細。計画書名と分け、最後まで読めるようにする。
    Text(String),
    /// 最後に報告を受け取ってからの時間。
    LastContact {
        /// 受け取った時刻 (`<time datetime>` に入れるRFC 3339のUTC)。
        datetime: String,
        /// 受け取ってからの時間 (例: `12分前`)。
        ago: String,
    },
}

/// 最後の報告から、プロジェクトの状態を決める。報告をまだ受け取っていなければ不通とする。
pub fn state(received: Option<&Received>, now: Timestamp) -> State {
    let Some(received) = received else {
        return State::Unreachable;
    };
    if now.duration_since(received.received_at) > UNREACHABLE_AFTER {
        return State::Unreachable;
    }
    let report = &received.report;
    if report.progress.iter().any(|plan| plan.wait.is_some()) {
        State::Waiting
    } else if report.running || !report.progress.is_empty() {
        State::Running
    } else {
        State::Idle
    }
}

/// 設定にあるプロジェクトを、状態の順 (同じ状態の中では設定に書いた順) に並べた一覧を作る。
///
/// 設定から外したプロジェクトの報告は出さない。
pub fn list<S: BuildHasher>(
    directors: &[Director],
    received: &HashMap<String, Received, S>,
    now: Timestamp,
) -> Vec<Item> {
    let mut items: Vec<Item> = directors
        .iter()
        .map(|director| item(&director.project, received.get(&director.project), now))
        .collect();
    items.sort_by_key(|item| item.state);
    items
}

/// プロジェクト1つの、一覧の1行を作る。
pub fn item(name: &str, received: Option<&Received>, now: Timestamp) -> Item {
    let state = state(received, now);
    let progress = received.map_or(&[][..], |received| &received.report.progress[..]);
    let parts = match state {
        State::Waiting => {
            let plan = progress.iter().find(|plan| plan.wait.is_some());
            plan.map_or_default(|plan| {
                vec![
                    Part::PlanTitle(plan.title.clone()),
                    Part::Text(waiting(plan)),
                ]
            })
        }
        State::Unreachable => match received {
            Some(received) => {
                let mut parts: Vec<Part> = progress
                    .first()
                    .map(|plan| Part::PlanTitle(plan.title.clone()))
                    .into_iter()
                    .collect();
                parts.push(Part::LastContact {
                    datetime: datetime(received.received_at),
                    ago: ago(received.received_at, now),
                });
                parts
            }
            None => vec![Part::Text("まだ連絡がありません".to_owned())],
        },
        State::Running => match progress.first() {
            Some(plan) => {
                let mut parts = vec![Part::PlanTitle(plan.title.clone())];
                parts.extend(step(plan).map(Part::Text));
                parts
            }
            None => vec![Part::Text("セッションを実行しています".to_owned())],
        },
        State::Idle => vec![Part::Text("進めている作業計画書はありません".to_owned())],
    };
    Item {
        name: name.to_owned(),
        state,
        parts,
    }
}

/// 一覧の上に出す、対応が要るプロジェクトの数のまとめ。
pub fn summary(items: &[Item]) -> String {
    let count = |state| items.iter().filter(|item| item.state == state).count();
    let counts: Vec<String> = [State::Waiting, State::Unreachable]
        .into_iter()
        .filter_map(|state| match count(state) {
            0 => None,
            n => Some(format!("{}が{n}件", state.label())),
        })
        .collect();
    if counts.is_empty() {
        "人間待ちと不通のプロジェクトはありません。".to_owned()
    } else {
        format!("{}あります。", counts.join("、"))
    }
}

/// 進めているタスクとステップ (例: `タスク1-2のレビュー`)。
fn step(plan: &PlanProgress) -> Option<String> {
    let step = plan.step.as_ref()?;
    Some(match &plan.task {
        Some(task) => format!("タスク{task}の{}", step.label),
        None => step.label.clone(),
    })
}

/// 人間待ちの種類。タスクを進めているときは、どのステップで待っているかを添える。
fn waiting(plan: &PlanProgress) -> String {
    let Some(wait) = &plan.wait else {
        return String::new();
    };
    match (&plan.task, step(plan)) {
        (Some(_), Some(step)) => format!("{step}で{}", wait.label),
        _ => wait.label.clone(),
    }
}

/// `<time datetime>` に入れる時刻。HTMLの日時の書式は秒の端数を3桁までしか許さないため、秒で切り捨てる。
fn datetime(time: Timestamp) -> String {
    Timestamp::from_second(time.as_second())
        .unwrap_or(time)
        .to_string()
}

/// fromからnowまでの時間を、相対の表現 (例: `12分前`) にする。
pub fn ago(from: Timestamp, now: Timestamp) -> String {
    let seconds = now.duration_since(from).as_secs();
    let minutes = seconds / 60;
    let hours = minutes / 60;
    if minutes < 1 {
        // 時計のずれで未来の時刻になったときも、ここに含める。
        "1分以内".to_owned()
    } else if hours < 1 {
        format!("{minutes}分前")
    } else if hours < 24 {
        format!("{hours}時間前")
    } else {
        format!("{}日前", hours / 24)
    }
}

/// DBから、プロジェクトごとに最後に受け取った報告を読む。
///
/// 読めない行 (この版のウェブアプリが読めない報告など) は、受け取っていないものとして扱い、エラーを記録する。
pub async fn load(database: &Database) -> Result<HashMap<String, Received>, db::Error> {
    let rows = database
        .call(|connection| {
            let mut statement =
                connection.prepare("SELECT project, report, received_at FROM project_statuses")?;
            let mut rows = statement.query([])?;
            let mut received = Vec::new();
            while let Some(row) = rows.next()? {
                if let Some(report) = StoredReport::read(row) {
                    received.push(report);
                }
            }
            Ok(received)
        })
        .await?;
    Ok(rows.into_iter().filter_map(StoredReport::decode).collect())
}

/// 1つのプロジェクトの最後の報告を読む。読めない行は一覧と同じく、受け取っていないものとして扱う。
pub async fn load_one(database: &Database, project: &str) -> Result<Option<Received>, db::Error> {
    let project = project.to_owned();
    let row = database
        .call(move |connection| {
            let row = connection
                .query_row(
                    "SELECT project, report, received_at FROM project_statuses WHERE project = ?1",
                    [project],
                    |row| Ok(StoredReport::read(row)),
                )
                .optional()?;
            Ok(row.flatten())
        })
        .await?;
    Ok(row.and_then(StoredReport::decode).map(|(_, report)| report))
}

/// SQLiteから読み取った1行。報告の解釈はDBの接続を解放してから行う。
struct StoredReport {
    project: String,
    json: String,
    received_at: String,
}

impl StoredReport {
    fn read(row: &Row<'_>) -> Option<Self> {
        let (Ok(project), Ok(json), Ok(received_at)) = (row.get(0), row.get(1), row.get(2)) else {
            log::error!("プロジェクトの報告をDBから文字列として読み取れません");
            return None;
        };
        Some(Self {
            project,
            json,
            received_at,
        })
    }

    fn decode(self) -> Option<(String, Received)> {
        let Self {
            project,
            json,
            received_at,
        } = self;
        let Ok(report) = serde_json::from_str::<StatusReport>(&json) else {
            log::error!("プロジェクト {project:?} の報告をDBから読み取れません");
            return None;
        };
        let Ok(received_at) = received_at.parse::<Timestamp>() else {
            log::error!("プロジェクト {project:?} の受信時刻をDBから読み取れません");
            return None;
        };
        Some((
            project,
            Received {
                report,
                received_at,
            },
        ))
    }
}

/// プロジェクトの画面のパス (`/projects/<名前>`)。名前はパスの1つの区切りとして百分率符号化する。
pub fn path(name: &str) -> String {
    // ピリオドだけの区切りは、百分率符号化してもブラウザが正規化する。
    // 元からチルダが付いた同じ形の名前にもチルダを1つ足し、一意に戻せるようにする。
    let prefix = if dot_segment(name) { "~" } else { "" };
    format!("/projects/{prefix}{}", encoded(name))
}

/// axumが百分率符号化を解いたパスの区切りから、設定のプロジェクト名に戻す。
pub(crate) fn name_from_segment(segment: &str) -> &str {
    if dot_segment(segment) {
        segment.strip_prefix('~').unwrap_or(segment)
    } else {
        segment
    }
}

fn dot_segment(name: &str) -> bool {
    matches!(name.trim_start_matches('~'), "." | "..")
}

fn encoded(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// プロジェクトの画面の中身。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detail {
    /// プロジェクトの名前。
    pub name: String,
    /// プロジェクトの状態。
    pub state: State,
    /// 最後に報告を受け取った時刻。まだ受け取っていなければ無い。
    pub last_contact: Option<Contact>,
    /// 進行中の作業計画書。
    pub plans: Vec<Plan>,
    /// これから進める作業計画書。
    pub upcoming: Vec<Link>,
}

/// 最後に報告を受け取った時刻。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    /// 受け取った時刻 (`<time datetime>` に入れるRFC 3339のUTC)。
    pub datetime: String,
    /// 受け取ってからの時間 (例: `12分前`)。
    pub ago: String,
}

/// 進行中の作業計画書1つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// 作業計画書のタイトル。
    pub title: String,
    /// 作業計画書のページへのリンク。
    pub link: Option<ReportLink>,
    /// 進めているタスクとステップ (例: `タスク1-2のレビュー`)。
    pub step: Option<String>,
    /// 人間を待っているときの、待っている理由。
    pub wait: Option<Waiting>,
    /// 済んだタスクの数。
    pub done: usize,
    /// タスクの一覧。
    pub tasks: Vec<Task>,
}

/// 人間を待っている理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    /// 人間待ちの種類の表示名 (例: `セッションからの質問`)。
    pub label: String,
    /// 待っている理由の文面。
    pub reason: String,
}

/// 作業計画書のタスク1つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// タスクの番号。
    pub number: String,
    /// タスクのタイトル。
    pub title: String,
    /// タスクの進み具合の文言。
    pub status: String,
}

/// リンク先を持つかもしれない作業計画書の名前。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// 作業計画書のタイトル。
    pub title: String,
    /// 作業計画書のページへのリンク。
    pub link: Option<ReportLink>,
}

/// 報告に含まれるページへのリンク。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportLink {
    /// ホストを持つHTTP(S)の絶対URL。
    pub url: String,
    /// 更新の前後でhtmxがキーボードフォーカスを復元するためのID。
    pub id: String,
}

/// プロジェクトの画面の中身を作る。
pub fn detail(name: &str, received: Option<&Received>, now: Timestamp) -> Detail {
    let state = state(received, now);
    let last_contact = received.map(|received| Contact {
        datetime: datetime(received.received_at),
        ago: ago(received.received_at, now),
    });
    let (plans, upcoming) = received.map_or_default(|received| {
        let report = &received.report;
        let mut link_ids = HashMap::new();
        let plans = report
            .progress
            .iter()
            .map(|entry| plan(entry, &mut link_ids))
            .collect();
        let upcoming = report
            .upcoming
            .iter()
            .map(|plan| Link {
                title: plan.title.clone(),
                link: plan
                    .url
                    .as_deref()
                    .and_then(|url| report_link(url, "upcoming-plan", &mut link_ids)),
            })
            .collect();
        (plans, upcoming)
    });
    Detail {
        name: name.to_owned(),
        state,
        last_contact,
        plans,
        upcoming,
    }
}

fn plan(plan: &PlanProgress, link_ids: &mut HashMap<String, usize>) -> Plan {
    let step = step(plan);
    let tasks = plan
        .tasks
        .iter()
        .map(|task| Task {
            number: task.number.clone(),
            title: task.title.clone(),
            status: match task.status {
                TaskStatus::Done => "済み".to_owned(),
                TaskStatus::InProgress => {
                    let doing = plan
                        .step
                        .as_ref()
                        .map_or("進めている", |step| step.label.as_str());
                    match &plan.wait {
                        Some(wait) => format!("{doing} ・ {}", wait.label),
                        None => doing.to_owned(),
                    }
                }
                TaskStatus::Pending => "未着手".to_owned(),
                TaskStatus::Unknown => "不明".to_owned(),
            },
        })
        .collect();
    Plan {
        title: plan.title.clone(),
        link: report_link(&plan.url, "active-plan", link_ids),
        step,
        wait: plan.wait.as_ref().map(|wait| Waiting {
            label: wait.label.clone(),
            reason: wait.reason.clone(),
        }),
        done: plan
            .tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Done)
            .count(),
        tasks,
    }
}

fn report_link(
    url: &str,
    prefix: &str,
    link_ids: &mut HashMap<String, usize>,
) -> Option<ReportLink> {
    let url = web_url(url)?;
    // 並びやタイトルが変わっても、同じ行き先のリンクを識別する。
    // 同じ行き先が繰り返し出る場合は、出現順でIDの重複を防ぐ。
    let base = format!("{prefix}-{}", encoded(url));
    let occurrence = link_ids.entry(base.clone()).or_default();
    *occurrence += 1;
    Some(ReportLink {
        id: format!("{base}:{occurrence}"),
        url: url.to_owned(),
    })
}

/// 管制アプリのURLを基準にせず、HTTP(S)のホストを特定できるリンクだけを返す。
fn web_url(url: &str) -> Option<&str> {
    let uri: Uri = url.parse().ok()?;
    let authority = uri.authority()?;
    let host = authority.host();
    let host_and_port = authority.as_str().rsplit('@').next()?;
    (matches!(uri.scheme_str(), Some("http" | "https"))
        && !host.is_empty()
        && (host_and_port == host || authority.port_u16().is_some()))
    .then_some(url)
}

#[cfg(test)]
mod tests {
    use korylusfactory_protocol::{Step, Task as ReportTask, UpcomingPlan, Version, Wait};

    use super::*;

    fn now() -> Timestamp {
        "2026-11-19T08:00:00Z".parse().expect("時刻を読めなかった")
    }

    fn report(running: bool, progress: Vec<PlanProgress>) -> StatusReport {
        StatusReport {
            sent_at: "2026-11-19T08:00:00Z".to_owned(),
            korylusfactory: Version {
                version: "0.1.0".to_owned(),
                commit: "abc1234".to_owned(),
            },
            running,
            progress,
            upcoming: Vec::new(),
        }
    }

    fn plan(task: Option<&str>, step: Option<&str>, wait: Option<&str>) -> PlanProgress {
        PlanProgress {
            name: "withdrawal".to_owned(),
            title: "退会機能 作業計画書".to_owned(),
            url: "https://wikino.example.com/s/example/pages/1".to_owned(),
            task: task.map(str::to_owned),
            step: step.map(|label| Step {
                kind: "review".to_owned(),
                label: label.to_owned(),
            }),
            wait: wait.map(|label| Wait {
                kind: "question".to_owned(),
                label: label.to_owned(),
                reason: "設計を変えてよいか".to_owned(),
            }),
            tasks: Vec::new(),
        }
    }

    fn task(number: &str, status: TaskStatus) -> ReportTask {
        ReportTask {
            number: number.to_owned(),
            title: format!("タスク{number}"),
            status,
        }
    }

    fn received(minutes_ago: i64, report: StatusReport) -> Received {
        Received {
            report,
            received_at: now() - SignedDuration::from_mins(minutes_ago),
        }
    }

    fn texts(item: &Item) -> Vec<String> {
        item.parts
            .iter()
            .map(|part| match part {
                Part::PlanTitle(text) | Part::Text(text) => text.clone(),
                Part::LastContact { ago, .. } => format!("最後の連絡 {ago}"),
            })
            .collect()
    }

    #[test]
    fn 報告の中身と受け取った時刻から状態を決める() {
        for (received, want, why) in [
            (None, State::Unreachable, "報告をまだ受け取っていない"),
            (
                Some(received(
                    6,
                    report(true, vec![plan(None, None, Some("質問"))]),
                )),
                State::Unreachable,
                "5分を超えて届いていなければ、人間待ちより不通にする",
            ),
            (
                Some(received(5, report(false, Vec::new()))),
                State::Idle,
                "ちょうど5分前はまだ不通にしない",
            ),
            (
                Some(received(
                    0,
                    report(
                        true,
                        vec![
                            plan(Some("1-1"), Some("実装"), None),
                            plan(None, None, Some("質問")),
                        ],
                    ),
                )),
                State::Waiting,
                "どれかの計画書が人間を待っている",
            ),
            (
                Some(received(0, report(true, Vec::new()))),
                State::Running,
                "セッションを実行している",
            ),
            (
                Some(received(0, report(false, vec![plan(None, None, None)]))),
                State::Running,
                "進行中の計画書がある",
            ),
            (
                Some(received(0, report(false, Vec::new()))),
                State::Idle,
                "進めているものが無い",
            ),
        ] {
            assert_eq!(state(received.as_ref(), now()), want, "{why}");
        }
    }

    #[test]
    fn 一覧は人間待ち_不通_実行中_計画書なしの順で同じ状態は設定の順に並べる() {
        let directors: Vec<Director> = ["Idle", "Running", "Never", "Waiting", "Late"]
            .iter()
            .map(|project| Director {
                vm: format!("{project}-factory"),
                project: (*project).to_owned(),
            })
            .collect();
        let received = HashMap::from([
            ("Idle".to_owned(), received(0, report(false, Vec::new()))),
            ("Running".to_owned(), received(0, report(true, Vec::new()))),
            (
                "Waiting".to_owned(),
                received(0, report(false, vec![plan(None, None, Some("質問"))])),
            ),
            ("Late".to_owned(), received(30, report(false, Vec::new()))),
            ("Removed".to_owned(), received(0, report(true, Vec::new()))),
        ]);

        let names: Vec<String> = list(&directors, &received, now())
            .into_iter()
            .map(|item| item.name)
            .collect();

        assert_eq!(
            names,
            ["Waiting", "Never", "Late", "Running", "Idle"],
            "並び順が違うか、設定から外したプロジェクトを出した"
        );
    }

    #[test]
    fn 状態ごとに中身を添える() {
        for (received, want) in [
            (
                Some(received(
                    0,
                    report(
                        true,
                        vec![plan(
                            Some("1-2"),
                            Some("実装"),
                            Some("セッションからの質問"),
                        )],
                    ),
                )),
                vec![
                    "退会機能 作業計画書",
                    "タスク1-2の実装でセッションからの質問",
                ],
            ),
            (
                Some(received(
                    0,
                    report(false, vec![plan(None, None, Some("作業報告書の確認"))]),
                )),
                vec!["退会機能 作業計画書", "作業報告書の確認"],
            ),
            (
                Some(received(
                    0,
                    report(true, vec![plan(Some("1-2"), Some("レビュー"), None)]),
                )),
                vec!["退会機能 作業計画書", "タスク1-2のレビュー"],
            ),
            (
                Some(received(
                    0,
                    report(true, vec![plan(None, Some("作業報告書の作成"), None)]),
                )),
                vec!["退会機能 作業計画書", "作業報告書の作成"],
            ),
            (
                Some(received(0, report(true, Vec::new()))),
                vec!["セッションを実行しています"],
            ),
            (
                Some(received(
                    12,
                    report(true, vec![plan(Some("2-1"), Some("実装"), None)]),
                )),
                vec!["退会機能 作業計画書", "最後の連絡 12分前"],
            ),
            (None, vec!["まだ連絡がありません"]),
            (
                Some(received(0, report(false, Vec::new()))),
                vec!["進めている作業計画書はありません"],
            ),
        ] {
            let item = item("Annict", received.as_ref(), now());

            assert_eq!(texts(&item), want, "{:?} の中身が違う", item.state);
        }
    }

    #[test]
    fn 最後の連絡の時刻は秒までのutcで持つ() {
        let received = Received {
            report: report(false, Vec::new()),
            received_at: "2026-11-19T07:47:59.123456789Z"
                .parse()
                .expect("時刻を読めなかった"),
        };

        let item = item("Annict", Some(&received), now());

        assert_eq!(
            item.parts,
            [Part::LastContact {
                datetime: "2026-11-19T07:47:59Z".to_owned(),
                ago: "12分前".to_owned(),
            }],
            "最後の連絡の時刻の形が違う"
        );
    }

    #[test]
    fn 経った時間を相対の表現にする() {
        for (seconds, want) in [
            (-30, "1分以内"),
            (59, "1分以内"),
            (60, "1分前"),
            (59 * 60 + 59, "59分前"),
            (60 * 60, "1時間前"),
            (24 * 60 * 60 - 1, "23時間前"),
            (24 * 60 * 60, "1日前"),
            (3 * 24 * 60 * 60, "3日前"),
        ] {
            let from = now() - SignedDuration::from_secs(seconds);

            assert_eq!(ago(from, now()), want, "{seconds}秒前の表現が違う");
        }
    }

    #[test]
    fn 対応が要るプロジェクトの数をまとめる() {
        let item = |state| Item {
            name: "A".to_owned(),
            state,
            parts: Vec::new(),
        };

        for (states, want) in [
            (
                vec![State::Waiting, State::Waiting, State::Unreachable],
                "人間待ちが2件、不通が1件あります。",
            ),
            (
                vec![State::Unreachable, State::Running],
                "不通が1件あります。",
            ),
            (
                vec![State::Running, State::Idle],
                "人間待ちと不通のプロジェクトはありません。",
            ),
            (vec![], "人間待ちと不通のプロジェクトはありません。"),
        ] {
            let items: Vec<Item> = states.into_iter().map(item).collect();

            assert_eq!(summary(&items), want, "まとめの文面が違う");
        }
    }

    #[tokio::test]
    async fn dbから報告を読み_読めない行は受け取っていないものとして扱う() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れなかった");
        let database =
            Database::open(&dir.path().join("factory.sqlite3")).expect("DBを開けなかった");
        let text = serde_json::to_string(&report(true, Vec::new())).expect("JSONにできなかった");
        database
            .call(move |connection| {
                connection.execute(
                    "INSERT INTO project_statuses VALUES
                        (?1, ?2, ?3), (?4, ?5, ?6), (?7, ?8, ?9), (?10, CAST(?11 AS TEXT), ?12)",
                    rusqlite::params![
                        "Annict",
                        text,
                        "2026-11-19T07:59:00Z",
                        "Broken",
                        "{",
                        "2026-11-19T07:59:00Z",
                        "BadTime",
                        text,
                        "昨日",
                        "BadEncoding",
                        b"\xff".as_slice(),
                        "2026-11-19T07:59:00Z",
                    ],
                )?;
                Ok(())
            })
            .await
            .expect("報告を書けなかった");

        let received = load(&database).await.expect("報告を読めなかった");

        assert_eq!(
            received.keys().collect::<Vec<_>>(),
            ["Annict"],
            "読めない行を除いていない"
        );
        assert_eq!(
            received["Annict"].received_at,
            "2026-11-19T07:59:00Z"
                .parse::<Timestamp>()
                .expect("時刻を読めなかった"),
            "受信時刻が違う"
        );
        let annict = load_one(&database, "Annict")
            .await
            .expect("Annictの報告を読めなかった");
        assert_eq!(annict.as_ref(), received.get("Annict"), "詳細の報告が違う");
        for project in ["Broken", "BadTime", "BadEncoding", "Missing"] {
            let report = load_one(&database, project)
                .await
                .expect("対象の報告の読み取りに失敗した");
            assert_eq!(report, None, "{project} を未受信として扱わなかった");
        }
    }

    #[test]
    fn プロジェクトの画面のパスは名前を百分率符号化する() {
        assert_eq!(
            path("Annict"),
            "/projects/Annict",
            "そのまま使える名前を変えた"
        );
        assert_eq!(
            path("a b/日"),
            "/projects/a%20b%2F%E6%97%A5",
            "パスに使えない文字を符号化しなかった"
        );
    }

    #[test]
    fn ホストのあるhttpとhttpsの絶対urlだけをリンクにする() {
        for url in [
            "HTTPS://wiki.example.test/s/demo/pages/1#見出し",
            "http://wiki.example.test",
            "https://[::1]:8443/pages/1",
        ] {
            assert_eq!(web_url(url), Some(url), "{url:?} をリンクにしなかった");
        }
        for url in [
            "http:future",
            "https:///future",
            "https://:443/pages/1",
            "https://wiki.example.test:bad/pages/1",
            "//wiki.example.test/pages/1",
            "javascript:alert(1)",
        ] {
            assert_eq!(web_url(url), None, "{url:?} をリンクにした");
        }
    }

    #[test]
    fn 計画書リンクのidは並び替えで変わらず_同じurlでも重複しない() {
        let first = "https://wiki.example.test/pages/1";
        let second = "https://wiki.example.test/pages/2";
        let mut old = HashMap::new();
        let first_id = report_link(first, "upcoming-plan", &mut old).expect("リンクを作れなかった");
        let second_id =
            report_link(second, "upcoming-plan", &mut old).expect("リンクを作れなかった");
        let duplicate =
            report_link(first, "upcoming-plan", &mut old).expect("リンクを作れなかった");
        assert_ne!(first_id.id, duplicate.id, "同じURLのIDが重なった");

        let mut reordered = HashMap::new();
        assert_eq!(
            report_link(second, "upcoming-plan", &mut reordered),
            Some(second_id)
        );
        assert_eq!(
            report_link(first, "upcoming-plan", &mut reordered),
            Some(first_id)
        );
        assert_eq!(
            report_link(first, "upcoming-plan", &mut reordered),
            Some(duplicate)
        );
    }

    #[test]
    fn プロジェクトの画面の中身を報告から作る() {
        let mut report = report(
            true,
            vec![PlanProgress {
                tasks: vec![
                    task("1-1", TaskStatus::Done),
                    task("1-2", TaskStatus::InProgress),
                    task("1-3", TaskStatus::Pending),
                    task("1-4", TaskStatus::Unknown),
                ],
                ..plan(Some("1-2"), Some("レビュー"), None)
            }],
        );
        report.upcoming = vec![
            UpcomingPlan {
                title: "通知 作業計画書".to_owned(),
                url: Some("HTTPS://wikino.example.com/s/example/pages/2".to_owned()),
            },
            UpcomingPlan {
                title: "危ない 作業計画書".to_owned(),
                url: Some("javascript:alert(1)".to_owned()),
            },
            UpcomingPlan {
                title: "リンクの無い 作業計画書".to_owned(),
                url: None,
            },
        ];

        let detail = detail("Annict", Some(&received(3, report)), now());

        assert_eq!(detail.state, State::Running, "状態が違う");
        assert_eq!(
            detail.last_contact.map(|contact| contact.ago),
            Some("3分前".to_owned()),
            "最後の連絡が違う"
        );
        let plan = &detail.plans[0];
        assert_eq!(plan.done, 1, "済んだタスクの数が違う");
        assert_eq!(
            plan.step.as_deref(),
            Some("タスク1-2のレビュー"),
            "進めているステップが違う"
        );
        assert_eq!(
            plan.tasks
                .iter()
                .map(|task| task.status.as_str())
                .collect::<Vec<_>>(),
            ["済み", "レビュー", "未着手", "不明"],
            "タスクの進み具合の文言が違う"
        );
        assert_eq!(
            detail
                .upcoming
                .iter()
                .map(|plan| plan.link.as_ref().map(|link| link.url.as_str()))
                .collect::<Vec<_>>(),
            [
                Some("HTTPS://wikino.example.com/s/example/pages/2"),
                None,
                None
            ],
            "http・https以外のURLを除いていない"
        );
    }

    #[test]
    fn 報告をまだ受け取っていないプロジェクトの画面は中身を持たない() {
        let detail = detail("Annict", None, now());

        assert_eq!(detail.state, State::Unreachable, "不通にしなかった");
        assert_eq!(detail.last_contact, None, "最後の連絡を作った");
        assert!(
            detail.plans.is_empty() && detail.upcoming.is_empty(),
            "受け取っていない報告から中身を作った"
        );
    }
}
