//! セッションが途中で止まったか完了の条件を満たさなかったときのやり直しと、
//! セッションが人間の判断を求めて終わったときの人間待ち。

use jiff::Timestamp;

use super::review::session_review;
use super::{Error, wait};
use crate::state::{PlanState, SessionAgent, SessionRecord, SessionResult, Slot, Step, WaitKind};

/// 1つのステップを、途中で止まったか完了の条件を満たさなかったためにやり直す上限の回数。
pub const RETRY_LIMIT: u32 = 3;

/// 結果の記録前に停止していたセッションを、次の指示で説明する理由。
const INTERRUPTED: &str = "前のセッションは、結果の確認と記録が終わる前に中断されました";

/// セッションを起動することを記録する。前の起動が未確定なら、中断したことを次の指示へ残す。
///
/// セッションの記録を、エージェントagentと現在の日時nowで開く。結果を記録していない前の記録は、中断として閉じる。
/// 呼び出し側は、起動する前に状態ファイルのロックの中で保存する。
/// 中断はやり直しの回数に数えず、結果を確かめられた失敗は [`retry`] で別に数える。
pub fn begin_session(
    state: &mut PlanState,
    agent: SessionAgent,
    now: Timestamp,
) -> Result<(), Error> {
    let step = session_step(state)?;
    for session in state
        .sessions
        .iter_mut()
        .filter(|session| session.is_open())
    {
        session.result = Some(SessionResult::Interrupted);
    }
    let review = session_review(state);
    let slot = match step {
        // 回数を数えられないのは状態が壊れているときで、次の判定でエラーになるため、1回目の枠にしておく。
        Step::Review | Step::Fix => Slot::for_review(review.unwrap_or(1)),
        Step::Prepare | Step::Implement | Step::Work | Step::Report => Slot::Implementer,
    };
    state.sessions.push(SessionRecord {
        task: state.task.clone(),
        step,
        review,
        slot,
        agent: agent.agent,
        model: agent.model,
        effort: agent.effort,
        started_at: now,
        ended_at: None,
        result: None,
        verdict: None,
    });
    if state.session_started
        && !state
            .retry_reasons
            .iter()
            .any(|reason| reason == INTERRUPTED)
    {
        state.retry_reasons.push(INTERRUPTED.to_owned());
    }
    state.session_started = true;
    Ok(())
}

/// 開いているセッションの記録を、結果resultと終了時刻ended_atで閉じる。
///
/// 開いている記録が無ければ何もしない。呼び出し側は、結果を記録するのと同じ状態ファイルのロックの中で保存する。
pub fn end_session(state: &mut PlanState, result: SessionResult, ended_at: Timestamp) {
    if let Some(session) = state
        .sessions
        .last_mut()
        .filter(|session| session.is_open())
    {
        session.result = Some(result);
        session.ended_at = Some(ended_at);
    }
}

/// ステップのやり直しを記録した結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// 同じステップをやり直す。何回目のやり直しか (1始まり) を持つ。
    Again(u32),
    /// やり直しが上限に達したため、人間待ちにした。
    Limit,
}

/// 進めているステップのセッションが途中で止まったか、完了の条件を満たさなかったことを、理由reasonsとともに記録する。
///
/// やり直しの回数が上限limitに達していれば、同じステップのまま、やり直しの上限の人間待ちにする。
/// 達していなければ、やり直しの回数を数え、次のセッションに伝える理由を残す。
pub fn retry(state: &mut PlanState, reasons: Vec<String>, limit: u32) -> Result<Retry, Error> {
    let step = session_step(state)?;
    state.session_started = false;
    if state.retries >= limit {
        let mut details = reasons;
        details.push("止まった原因を取り除いてから再開してください。同じステップを新しいセッションでやり直します".to_owned());
        wait_same_step(
            state,
            step,
            WaitKind::RetryLimit,
            format!(
                "{}を{limit}回やり直しても、終えられませんでした",
                step.label()
            ),
            details,
        );
        return Ok(Retry::Limit);
    }
    state.retries = state.retries.saturating_add(1);
    state.retry_reasons = reasons;
    Ok(Retry::Again(state.retries))
}

/// 進めているステップのセッションが、人間の判断が要ると返して終わったことを、理由questionとともに記録する。
///
/// 再開したら同じステップを新しいセッションでやり直すよう、ステップを残したまま人間待ちにする。
pub fn ask(state: &mut PlanState, question: String) -> Result<(), Error> {
    let step = session_step(state)?;
    wait_same_step(
        state,
        step,
        WaitKind::Question,
        format!(
            "{}のセッションが、人間の判断を求めて終わりました",
            step.label()
        ),
        vec![
            question,
            "対応してから再開してください。同じステップを新しいセッションでやり直します".to_owned(),
        ],
    );
    Ok(())
}

/// 進めているステップのセッションが、担当ごとのエージェントの設定 (モデルかeffort) をCLIに受け付けられずに終わったことを記録する。
///
/// 同じ設定でやり直しても同じ失敗を繰り返すため、やり直しの回数に数えずに、同じステップのまま人間待ちにする。
/// detailsには、枠と使った値と、CLIが値を拒んだ文面を書く。再開すると、設定を読み直して同じステップを新しいセッションでやり直す。
pub fn reject_settings(state: &mut PlanState, details: Vec<String>) -> Result<(), Error> {
    let step = session_step(state)?;
    let mut details = details;
    details.push(
        "ウェブアプリの設定の画面で直してから再開してください。同じステップを新しいセッションでやり直します"
            .to_owned(),
    );
    wait_same_step(
        state,
        step,
        WaitKind::AgentSettings,
        format!(
            "{}のセッションで、担当ごとのエージェントの設定をCLIが受け付けませんでした",
            step.label()
        ),
        details,
    );
    Ok(())
}

/// 状態stateを、進めているステップstepのまま人間待ちにする。再開すると、同じステップを新しいセッションでやり直す。
///
/// レビューのステップをやり直すときに前の回のレビュードキュメントを受け付けないよう、
/// ステップを始めたときの最新のレビュードキュメントを残す。
fn wait_same_step(
    state: &mut PlanState,
    step: Step,
    kind: WaitKind,
    reason: String,
    details: Vec<String>,
) {
    let review_before = state.review_before.take();
    wait(state, Some(step), kind, reason, details);
    state.review_before = review_before;
}

/// 状態stateで進めている、セッションを起動するステップを返す。
///
/// 人間を待っているときと、セッションを起動しないタスクの準備を進めているときはエラーにする。
fn session_step(state: &PlanState) -> Result<Step, Error> {
    if let Some(wait) = &state.wait {
        return Err(Error::Waiting(wait.kind));
    }
    match state.step {
        Some(step) if step != Step::Prepare => Ok(step),
        step => Err(Error::NoSession(step)),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::super::tests::{implementing, new_state};
    use super::super::{Project, resume};
    use super::*;
    use crate::flow::tests::{PLAN, doc};

    fn project() -> Project {
        Project {
            root: PathBuf::from("/workspace"),
            repo: PathBuf::from("/workspace"),
        }
    }

    fn claude() -> SessionAgent {
        SessionAgent {
            agent: "claude".to_owned(),
            model: None,
            effort: None,
        }
    }

    fn at(text: &str) -> Timestamp {
        text.parse().expect("日時を読めなかった")
    }

    #[test]
    fn 開始を保存して再開したら中断を伝え_通常の失敗では開始を外す() {
        let mut state = implementing();
        begin_session(&mut state, claude(), at("2026-11-20T08:00:00Z"))
            .expect("開始を記録できなかった");
        assert!(state.session_started, "開始を残していない");
        assert!(state.retry_reasons.is_empty(), "初回に中断を伝えた");

        let saved = serde_json::to_vec(&state).expect("状態を書き出せなかった");
        let mut state: PlanState = serde_json::from_slice(&saved).expect("状態を読めなかった");
        begin_session(&mut state, claude(), at("2026-11-20T08:00:00Z"))
            .expect("再開を記録できなかった");
        begin_session(&mut state, claude(), at("2026-11-20T08:00:00Z"))
            .expect("再度の再開を記録できなかった");
        assert_eq!(
            state.retry_reasons,
            [INTERRUPTED],
            "中断を重複なく伝えていない"
        );
        assert_eq!(state.retries, 0, "中断をやり直しの回数に数えた");

        retry(&mut state, vec!["失敗".to_owned()], RETRY_LIMIT).expect("失敗を記録できなかった");
        assert!(
            !state.session_started,
            "結果を記録したのに実行中のままになった"
        );
        begin_session(&mut state, claude(), at("2026-11-20T08:00:00Z"))
            .expect("やり直しを記録できなかった");
        assert_eq!(state.retry_reasons, ["失敗"], "確定した失敗を中断扱いした");
    }

    #[test]
    fn 上限までは理由を残してやり直しの回数を数える() {
        let mut state = implementing();
        for count in 1..=RETRY_LIMIT {
            let reasons = vec![format!("{count}回目の理由")];
            assert_eq!(
                retry(&mut state, reasons.clone(), RETRY_LIMIT)
                    .expect("やり直しを記録できなかった"),
                Retry::Again(count)
            );
            assert_eq!(state.retries, count, "やり直しの回数が違う");
            assert_eq!(state.retry_reasons, reasons, "やり直しの理由を残していない");
            assert_eq!(state.step, Some(Step::Implement), "ステップが変わった");
            assert_eq!(state.wait, None, "人間待ちにした");
        }
    }

    #[test]
    fn 上限を超えたら同じステップのままやり直しの上限の人間待ちにする() {
        let mut state = implementing();
        state.retries = RETRY_LIMIT;
        state.retry_reasons = vec!["前の理由".to_owned()];

        let result = retry(
            &mut state,
            vec!["セッションが終了コード 1 で終わりました".to_owned()],
            RETRY_LIMIT,
        );

        assert_eq!(result.expect("やり直しを記録できなかった"), Retry::Limit);
        let wait = state.wait.clone().expect("人間を待っていない");
        assert_eq!(wait.kind, WaitKind::RetryLimit, "人間を待つ場面が違う");
        assert_eq!(wait.reason, "実装を3回やり直しても、終えられませんでした");
        assert_eq!(
            wait.details[0], "セッションが終了コード 1 で終わりました",
            "最後に止まった理由を人間に伝えていない"
        );
        assert_eq!(state.step, Some(Step::Implement), "ステップが変わった");
        assert_eq!(state.task.as_deref(), Some("1-1"), "タスクが変わった");
        assert_eq!(state.retries, 0, "やり直しの回数を戻していない");
        assert_eq!(state.retry_reasons, Vec::<String>::new(), "理由を残した");
    }

    #[test]
    fn 質問で人間を待ってもレビューを受け付ける境目を残す() {
        let mut state = implementing();
        state.step = Some(Step::Review);
        state.review_before = Some(PathBuf::from("docs/private/reviews/demo-1-1-001.md"));

        ask(&mut state, "設計を変えてよいか".to_owned()).expect("質問を記録できなかった");

        let wait = state.wait.clone().expect("人間を待っていない");
        assert_eq!(wait.kind, WaitKind::Question, "人間を待つ場面が違う");
        assert_eq!(
            wait.reason,
            "レビューのセッションが、人間の判断を求めて終わりました"
        );
        assert_eq!(wait.details[0], "設計を変えてよいか");
        assert_eq!(state.step, Some(Step::Review), "ステップが変わった");
        assert_eq!(
            state.review_before.as_deref(),
            Some(PathBuf::from("docs/private/reviews/demo-1-1-001.md").as_path()),
            "前の回のレビュードキュメントを受け付けてしまう"
        );
    }

    #[test]
    fn やり直しの上限と質問の人間待ちから再開すると同じステップに戻る() {
        let plan = doc(PLAN);
        for kind in [
            WaitKind::RetryLimit,
            WaitKind::Question,
            WaitKind::AgentSettings,
        ] {
            let mut state = implementing();
            match kind {
                WaitKind::RetryLimit => {
                    state.retries = RETRY_LIMIT;
                    retry(&mut state, vec!["止まった".to_owned()], RETRY_LIMIT)
                        .expect("やり直しを記録できなかった");
                }
                WaitKind::AgentSettings => {
                    reject_settings(&mut state, vec!["拒んだ".to_owned()])
                        .expect("設定の拒否を記録できなかった");
                }
                _ => ask(&mut state, "質問".to_owned()).expect("質問を記録できなかった"),
            }

            resume(&mut state, &plan, &project(), None).expect("再開できなかった");

            assert_eq!(state.wait, None, "{kind:?}: 人間待ちを解いていない");
            assert_eq!(
                state.step,
                Some(Step::Implement),
                "{kind:?}: ステップが変わった"
            );
            assert_eq!(
                state.task.as_deref(),
                Some("1-1"),
                "{kind:?}: タスクが変わった"
            );
        }
    }

    #[test]
    fn セッションを起動しないときと人間待ちのときは記録しない() {
        let mut state = new_state();
        let err = retry(&mut state, Vec::new(), RETRY_LIMIT).expect_err("記録できてしまった");
        assert!(matches!(err, Error::NoSession(None)), "エラー = {err:?}");

        state.step = Some(Step::Prepare);
        let err = ask(&mut state, "質問".to_owned()).expect_err("記録できてしまった");
        assert!(
            matches!(err, Error::NoSession(Some(Step::Prepare))),
            "エラー = {err:?}"
        );

        let mut state = implementing();
        ask(&mut state, "質問".to_owned()).expect("質問を記録できなかった");
        let err = retry(&mut state, Vec::new(), RETRY_LIMIT).expect_err("記録できてしまった");
        assert!(
            matches!(err, Error::Waiting(WaitKind::Question)),
            "エラー = {err:?}"
        );
    }

    #[test]
    fn 起動すると記録を開き_結果の記録で閉じ_閉じていない前の記録は中断にする() {
        let mut state = implementing();
        let codex = SessionAgent {
            agent: "codex".to_owned(),
            model: Some("gpt-5.3-codex".to_owned()),
            effort: Some("high".to_owned()),
        };
        begin_session(&mut state, codex, at("2026-11-20T08:00:00Z"))
            .expect("開始を記録できなかった");
        begin_session(&mut state, claude(), at("2026-11-20T09:00:00Z"))
            .expect("再開を記録できなかった");
        end_session(
            &mut state,
            SessionResult::Recorded,
            at("2026-11-20T09:30:00Z"),
        );
        end_session(&mut state, SessionResult::Stale, at("2026-11-20T10:00:00Z"));

        let first = &state.sessions[0];
        assert_eq!(
            (first.task.as_deref(), first.step, first.review, first.slot),
            (Some("1-1"), Step::Implement, None, Slot::Implementer),
            "起動したステップと枠を記録していない"
        );
        assert_eq!(
            (
                first.agent.as_str(),
                first.model.as_deref(),
                first.effort.as_deref()
            ),
            ("codex", Some("gpt-5.3-codex"), Some("high")),
            "使ったエージェントを記録していない"
        );
        assert_eq!(
            (first.started_at, first.ended_at, first.result),
            (
                at("2026-11-20T08:00:00Z"),
                None,
                Some(SessionResult::Interrupted)
            ),
            "閉じていない前の記録を中断にしなかった"
        );
        let second = &state.sessions[1];
        assert_eq!(
            (second.ended_at, second.result),
            (
                Some(at("2026-11-20T09:30:00Z")),
                Some(SessionResult::Recorded)
            ),
            "結果の記録で閉じなかった"
        );
        assert_eq!(
            state.sessions.len(),
            2,
            "閉じた記録を閉じ直したか、記録を増やした"
        );
    }

    #[test]
    fn 人間待ちのあいだはセッションの記録を開かない() {
        let mut state = implementing();
        ask(&mut state, "どちらにしますか".to_owned()).expect("質問を記録できなかった");

        let result = begin_session(&mut state, claude(), at("2026-11-20T08:00:00Z"));

        assert!(result.is_err(), "人間待ちのあいだにセッションを始めた");
        assert!(state.sessions.is_empty(), "人間待ちのあいだに記録を開いた");
    }

    #[test]
    fn 設定を受け付けられなかったらやり直しの回数に数えずに同じステップで人間を待つ() {
        let mut state = implementing();
        retry(&mut state, vec!["一時的な失敗".to_owned()], RETRY_LIMIT)
            .expect("やり直しを記録できなかった");
        begin_session(&mut state, claude(), at("2026-11-20T08:00:00Z"))
            .expect("やり直しを始められなかった");

        reject_settings(
            &mut state,
            vec![
                "作業者: Claude Code・no-such-model・既定のeffort".to_owned(),
                "[claude-code:unrecognized_model]".to_owned(),
            ],
        )
        .expect("設定の拒否を記録できなかった");

        let wait = state.wait.clone().expect("人間を待っていない");
        assert_eq!(wait.kind, WaitKind::AgentSettings, "人間を待つ場面が違う");
        assert_eq!(
            wait.reason,
            "実装のセッションで、担当ごとのエージェントの設定をCLIが受け付けませんでした",
            "待っている理由が違う"
        );
        assert_eq!(
            wait.details,
            [
                "作業者: Claude Code・no-such-model・既定のeffort",
                "[claude-code:unrecognized_model]",
                "ウェブアプリの設定の画面で直してから再開してください。同じステップを新しいセッションでやり直します",
            ],
            "待っている理由の詳細が違う"
        );
        assert_eq!(state.step, Some(Step::Implement), "ステップが変わった");
        assert_eq!(state.retries, 0, "設定の拒否をやり直しの回数に数えた");
        assert!(!state.session_started, "開始の印を外していない");
    }
}
