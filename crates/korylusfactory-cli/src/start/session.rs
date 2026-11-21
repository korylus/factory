//! セッションを起動するステップ。用意した指示でセッションを起動し、結果とリポジトリの状態を確かめて記録する。
//!
//! 指示とセッションの記録のディレクトリは、開始の印を保存する前に、状態ファイルのロックの中で用意する。
//! セッションの実行中は、状態ファイルのロックを持たない。
//! 終わったらロックを取って状態を読み直し、起動したときと同じステップを進めているときだけ記録する。
//! 結果を返さずに終わったときと、完了の条件を満たさなかったときは、理由を残して同じステップをやり直す。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use jiff::Timestamp;
use korylusfactory_core::flow::{self, Fixed, Implemented, Next, RETRY_LIMIT, Reported, Retry};
use korylusfactory_core::state::{PlanDir, PlanState, SessionResult};
use korylusfactory_protocol::AgentSetting;

use super::{Director, EXCLUDED, Error, Pace, SESSION_LOCK_PATH, describe_next, minutes};
use crate::agent::{self, Outcome, Response};
use crate::instruction::{self, Agent, Context, Request};
use crate::operation::slot_label;
use crate::process::Exclusive;
use crate::{branch, describe};

/// セッションの記録を置く、作業計画書の状態の置き場の下のディレクトリの名前。
const SESSIONS: &str = "sessions";

/// 起動する前に用意した、セッションへの指示と記録のディレクトリ。
pub(super) struct Launch {
    request: Request,
    logs: PathBuf,
}

impl Launch {
    /// 状態の置き場dirの作業計画書で、状態stateの次に行うことnextのセッションへの、枠の設定settingで頼む指示と、記録のディレクトリを用意する。
    ///
    /// 記録のディレクトリの名前は、現在の日時nowから決める。セッションを起動しないステップなら `None` を返す。
    pub(super) fn new(
        dir: &PlanDir,
        state: &PlanState,
        next: &Next,
        now: Timestamp,
        setting: &AgentSetting,
    ) -> Result<Option<Self>, Error> {
        let state_path = dir.state_path();
        let context = Context {
            plan_url: &state.url,
            state_path: &state_path,
            retry: &state.retry_reasons,
            answers: &state.answers,
        };
        let Some(request) = instruction::build(next, state, &context, setting)? else {
            return Ok(None);
        };
        let logs = logs_dir(dir.path(), next, now)?;
        Ok(Some(Self { request, logs }))
    }
}

/// 計画書名nameの作業計画書で、状態stateの次に行うことnextのセッションを、用意したlaunchで起動し、結果を記録する。
///
/// 状態の置き場dirの下で状態を読み直して記録する。cancelが真になったら、セッションを終わらせて取り消しを記録する。
/// 進行の更新とセッションの記録は、同じロックの中で保存する。
pub(super) fn run(
    director: &mut Director<'_, '_>,
    dir: &PlanDir,
    name: &str,
    state: &PlanState,
    next: &Next,
    launch: Launch,
    cancel: &AtomicBool,
) -> Result<Pace, Error> {
    let Launch { request, logs } = launch;
    director.say(&format!(
        "{name}: {}を始めます ({})。記録: {}",
        describe_next(next),
        request
            .agent
            .setting_label(request.model.as_ref(), request.effort),
        logs.display()
    ))?;
    let start = director.start;
    let exclusive_path = start.root.join(SESSION_LOCK_PATH);
    let launcher = Exclusive::new(start.launcher, &exclusive_path);
    let outcome = match agent::run(
        &launcher,
        &request,
        start.root,
        &logs,
        start.timing.session,
        cancel,
    ) {
        Ok(outcome) => outcome,
        // 起動できないときも、原因を取り除くまで繰り返さないよう、途中で止まったものとしてやり直しの回数に数える。
        Err(err) => Outcome::Stopped(describe(&err)),
    };
    // 状態ファイルのロックを待つ時間を所要時間に含めないよう、ロックを取る前に終わった時刻を採る。
    let ended_at = start.clock.now();

    let lock = dir.lock()?;
    let Some(mut current) = lock.load()? else {
        return Ok(Pace::Continue);
    };
    let reasons = match outcome {
        Outcome::Cancelled => {
            // 開始の印は残し、次に起動したセッションへ中断を伝える。
            flow::end_session(&mut current, SessionResult::Cancelled, ended_at);
            lock.save(&current)?;
            return Ok(Pace::Continue);
        }
        _ if current.wait.is_some() || current.task != state.task || current.step != state.step => {
            flow::end_session(&mut current, SessionResult::Stale, ended_at);
            lock.save(&current)?;
            drop(lock);
            director.say(&format!(
                "{name}: セッションの間に状態が変わったため、{}の結果を記録しません",
                describe_next(next)
            ))?;
            return Ok(Pace::Continue);
        }
        Outcome::NeedsHuman(question) => {
            flow::ask(&mut current, question)?;
            flow::end_session(&mut current, SessionResult::Asked, ended_at);
            lock.save(&current)?;
            return Ok(Pace::Continue);
        }
        Outcome::Rejected(text) => {
            let details = rejected_details(next, &request, &text, &logs);
            flow::reject_settings(&mut current, details)?;
            flow::end_session(&mut current, SessionResult::SettingsRejected, ended_at);
            lock.save(&current)?;
            return Ok(Pace::Continue);
        }
        Outcome::Stopped(reason) => vec![reason],
        Outcome::Done(response) => {
            // 確かめる途中で失敗しても状態を書きかけにしないよう、写しに記録する。
            let mut finished = current.clone();
            match finish(director, &mut finished, next, response, request.agent) {
                Ok(()) => {
                    flow::end_session(&mut finished, SessionResult::Recorded, ended_at);
                    lock.save(&finished)?;
                    drop(lock);
                    director.say(&format!("{name}: {}を記録しました", describe_next(next)))?;
                    return Ok(Pace::Continue);
                }
                Err(Error::Flow(flow::Error::Rejected(reasons))) => reasons,
                // 結果を確かめられないときも、同じステップをやり直して確かめ直す。
                Err(err) => vec![describe(&err)],
            }
        }
    };
    let retried = flow::retry(&mut current, reasons.clone(), RETRY_LIMIT)?;
    flow::end_session(&mut current, SessionResult::Unfinished, ended_at);
    lock.save(&current)?;
    drop(lock);
    match retried {
        Retry::Again(count) => {
            let mut text = format!(
                "{name}: {}を終えられなかったため、{}分後にやり直します ({count}/{RETRY_LIMIT}回目)",
                describe_next(next),
                minutes(director.start.timing.retry)
            );
            for reason in &reasons {
                text.push_str("\n  ");
                text.push_str(&reason.replace('\n', "\n  "));
            }
            director.diagnostic(text)?;
            Ok(Pace::Sleep(director.start.timing.retry))
        }
        Retry::Limit => Ok(Pace::Continue),
    }
}

/// セッションが返した結果responseとリポジトリの状態を確かめ、次に行うことnextの完了を状態stateに記録する。
///
/// レビューは、セッションを動かしたエージェントagentのレビュアーが行ったものとして記録する。
fn finish(
    director: &Director<'_, '_>,
    state: &mut PlanState,
    next: &Next,
    response: Response,
    agent: Agent,
) -> Result<(), Error> {
    let runner = director.start.runner;
    let project = &director.project;
    match next {
        Next::Implement { task } => {
            let doc = director.read_plan(&state.url)?;
            let (repo, base) = record(state, &task.id)?;
            let implemented = Implemented {
                branch: branch::current(runner, &repo)?,
                ahead: branch::ahead(runner, &repo, &base)?,
                dirty: branch::dirty(runner, &repo, &EXCLUDED)?,
                verification: response.verification.unwrap_or_default(),
            };
            flow::finish_implement(state, &doc, project, implemented)?;
        }
        Next::Work { .. } => {
            let doc = director.read_plan(&state.url)?;
            flow::finish_work(state, &doc)?;
        }
        Next::Review { .. } | Next::ReportReview { .. } => {
            flow::finish_review(state, project, director.start.clock.now(), agent.reviewer())?;
        }
        Next::Fix { task, .. } => {
            let doc = director.read_plan(&state.url)?;
            let (repo, _) = record(state, &task.id)?;
            let fixed = Fixed {
                branch: branch::current(runner, &repo)?,
                dirty: branch::dirty(runner, &repo, &EXCLUDED)?,
            };
            flow::finish_fix(state, &doc, project, &fixed)?;
        }
        Next::Report { .. } => {
            let doc = director.read_plan(&state.url)?;
            let reported = Reported {
                location: response.location.unwrap_or_default(),
                walkthrough_skipped: response.walkthrough_skipped,
            };
            flow::finish_report(state, &doc, project, reported, director.start.clock.now())?;
        }
        Next::ReportFix { .. } => flow::finish_report_fix(state, project)?,
        Next::Prepare { .. } | Next::Wait(_) | Next::Finished => {}
    }
    Ok(())
}

/// 状態stateのタスクidの記録から、リポジトリとブランチを作った元のブランチを返す。
fn record(state: &PlanState, id: &str) -> Result<(PathBuf, String), Error> {
    let record = state
        .task_record(id)
        .ok_or_else(|| flow::Error::MissingRecord(id.to_owned()))?;
    Ok((record.repo.clone(), record.base.clone()))
}

/// 状態の置き場dirの下に、現在の日時nowと次に行うことnextから名前を決めた、セッションの記録のディレクトリを作る。
fn logs_dir(dir: &Path, next: &Next, now: Timestamp) -> Result<PathBuf, Error> {
    let parent = dir.join(SESSIONS);
    fs::create_dir_all(&parent).map_err(|source| Error::Logs {
        path: parent.clone(),
        source,
    })?;
    let stem = format!("{}-{}", now.strftime("%Y%m%dT%H%M%SZ"), slug(next));
    let mut number = 1_u32;
    loop {
        let path = if number == 1 {
            parent.join(&stem)
        } else {
            parent.join(format!("{stem}-{number}"))
        };
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            // 同じ秒に同じステップを始めたときは、番号を足して別のディレクトリにする。
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                number = number.saturating_add(1);
            }
            Err(source) => return Err(Error::Logs { path, source }),
        }
    }
}

/// 次に行うことnextを、セッションの記録のディレクトリの名前に使う形にする。
fn slug(next: &Next) -> String {
    match next {
        Next::Prepare { task, .. } => format!("{}-prepare", task.id),
        Next::Implement { task } => format!("{}-implement", task.id),
        Next::Work { task } => format!("{}-work", task.id),
        Next::Review { task, number, .. } => format!("{}-review-{number}", task.id),
        Next::Fix { task, .. } => format!("{}-fix", task.id),
        Next::Report { .. } => "report".to_owned(),
        Next::ReportReview { number, .. } => format!("report-review-{number}"),
        Next::ReportFix { .. } => "report-fix".to_owned(),
        Next::Wait(_) => "wait".to_owned(),
        Next::Finished => "finished".to_owned(),
    }
}

/// 次に行うことnextのセッションで、頼むことrequestの値をCLIが拒んだときの、人間待ちの理由の詳細を返す。
///
/// 担当の呼び名と使った値に、CLIが値を拒んだ文面text (セッションから来たデータ) と、記録のディレクトリlogsを添える。
fn rejected_details(next: &Next, request: &Request, text: &str, logs: &Path) -> Vec<String> {
    let slot = next.slot().map_or("セッション", |slot| {
        slot_label(instruction::protocol_slot(slot))
    });
    vec![
        format!(
            "{slot}: {}",
            request
                .agent
                .setting_label(request.model.as_ref(), request.effort)
        ),
        format!("CLIの出力: {text}"),
        format!("記録: {}", logs.display()),
    ]
}
