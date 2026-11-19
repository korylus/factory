//! 監督のループで状態を採取し、待たずに送信スレッドへ渡す。

use std::io;
use std::sync::{Arc, mpsc};
use std::thread;

use jiff::{SignedDuration, Timestamp};
use korylusfactory_core::state::{self, PlanState, Store};

use super::Start;
use crate::config::Config;
use crate::describe;
use crate::roadmap::{self, Entry};
use crate::version::Build;
use crate::web::{self, Endpoint, Link, Reporter};

pub(super) struct Reporting {
    reporter: Reporter,
    link: Link,
    config: Config,
    store: Store,
    build: Build,
    progress: Vec<(String, PlanState)>,
    last_state_warning: Option<String>,
    entries: Vec<Entry>,
    checked_at: Option<Timestamp>,
    interval: SignedDuration,
    reading: Option<Reading>,
}

struct Reading {
    result: mpsc::Receiver<Result<Vec<Entry>, roadmap::Error>>,
    superseded: bool,
}

impl Reporting {
    pub(super) fn new(endpoint: &Endpoint, start: &Start<'_>, config: &Config) -> io::Result<Self> {
        Ok(Self {
            reporter: Reporter::spawn(endpoint, start.web)?,
            link: start.web.clone(),
            config: config.clone(),
            store: Store::new(start.root),
            build: start.build.clone(),
            progress: Vec::new(),
            last_state_warning: None,
            entries: Vec::new(),
            checked_at: None,
            interval: SignedDuration::try_from(start.timing.roadmap).unwrap_or(SignedDuration::MAX),
            reading: None,
        })
    }

    pub(super) fn use_roadmap(&mut self, entries: Vec<Entry>, now: Timestamp) {
        self.entries = entries;
        self.checked_at = Some(now);
        if let Some(reading) = &mut self.reading {
            // 監督が読み直した並びを、先に始めた報告用の取得で上書きしない。
            reading.superseded = true;
        }
    }

    pub(super) fn update(&mut self, running: bool, now: Timestamp) {
        match self.read_progress() {
            Ok(progress) => {
                self.progress = progress;
                self.last_state_warning = None;
            }
            Err(error) => {
                let text = format!("ウェブアプリに送る状態を読めません: {}", describe(&error));
                if self.last_state_warning.as_ref() != Some(&text) {
                    (self.link.warn)(&text);
                    self.last_state_warning = Some(text);
                }
            }
        }
        self.refresh_roadmap(now);
        self.reporter.update(web::report(
            &self.build,
            running,
            &self.progress,
            &self.entries,
        ));
    }

    fn read_progress(&self) -> Result<Vec<(String, PlanState)>, state::Error> {
        let mut progress = Vec::new();
        for name in self.store.plan_names()? {
            let dir = self.store.plan(&name)?;
            if !dir.state_path().is_file() {
                continue;
            }
            let state = match dir.try_lock()? {
                Some(lock) => lock.load()?,
                // 競合している間は、以前にロックの中で読んだ状態を使う。
                None => self
                    .progress
                    .iter()
                    .find(|(saved, _)| saved == &name)
                    .map(|(_, state)| state.clone()),
            };
            if let Some(state) = state
                && !state.is_completed()
            {
                progress.push((name, state));
            }
        }
        Ok(progress)
    }

    fn refresh_roadmap(&mut self, now: Timestamp) {
        if let Some(reading) = self.reading.take() {
            match reading.result.try_recv() {
                Err(mpsc::TryRecvError::Empty) => {
                    self.reading = Some(reading);
                    return;
                }
                _ if reading.superseded => {}
                Ok(Ok(entries)) => self.entries = entries,
                Ok(Err(error)) => (self.link.warn)(&format!(
                    "ウェブアプリに送るロードマップの並びを読めません: {}",
                    describe(&error)
                )),
                Err(mpsc::TryRecvError::Disconnected) => {
                    (self.link.warn)("ウェブアプリに送るロードマップの取得が中断されました");
                }
            }
        }
        if self
            .checked_at
            .is_some_and(|at| now.duration_since(at) < self.interval)
        {
            return;
        }

        self.checked_at = Some(now);
        let loader = Arc::clone(&self.link.load_roadmap);
        let config = self.config.clone();
        let store = self.store.clone();
        let (sender, result) = mpsc::channel();
        // 報告用の取得は同時に1つだけにする。終了時に取得を待たず、最後に読めた並びで送る。
        match thread::Builder::new()
            .name("factory-report-roadmap".to_owned())
            .spawn(move || {
                // 監督が先に終了した場合は結果を使うものがいない。
                let _ = sender.send(loader(&config, &store));
            }) {
            Ok(_) => {
                self.reading = Some(Reading {
                    result,
                    superseded: false,
                });
            }
            Err(error) => (self.link.warn)(&format!(
                "ウェブアプリに送るロードマップの取得を始められません: {error}"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::{Mutex, atomic::AtomicBool};
    use std::time::Duration;

    use jiff::tz::TimeZone;
    use korylusfactory_protocol::StatusReport;

    use super::*;
    use crate::config;
    use crate::process::System;
    use crate::start::{SystemClock, Timing};
    use crate::web::Transport;

    struct Sent(mpsc::Sender<StatusReport>);

    impl Transport for Sent {
        fn put(&self, _: &str, body: &[u8]) -> Result<u16, String> {
            let report = serde_json::from_slice(body).expect("状態の報告を読めなかった");
            self.0.send(report).expect("状態の報告を記録できなかった");
            Ok(204)
        }
    }

    fn next_report(received: &mpsc::Receiver<StatusReport>, context: &str) -> StatusReport {
        received
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|err| panic!("{context}: {err}"))
    }

    #[test]
    fn ロックやロードマップの更新を待たずに前の状態を報告する() {
        let timeout = Duration::from_secs(5);
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れなかった");
        let root = dir.path();
        fs::create_dir_all(root.join("docs/private/factory")).expect("設定の置き場を作れなかった");
        fs::write(
            root.join(config::CONFIG_PATH),
            r#"{"roadmap":"https://wikino.app/s/example/pages/1","web":{"url":"https://factory.example.com"}}"#,
        )
        .expect("設定を書けなかった");
        let config = config::load(root).expect("設定を読めなかった");
        let endpoint = config.web.as_ref().expect("送り先が無かった");
        let state = PlanState::new(
            "https://wikino.app/s/example/pages/2",
            "報告する計画書".to_owned(),
        );
        let plan = Store::new(root)
            .plan("sample")
            .expect("状態の置き場を決められなかった");
        plan.lock()
            .expect("ロックを取れなかった")
            .save(&state)
            .expect("状態を保存できなかった");

        let (sent, received) = mpsc::channel();
        let (reading, started) = mpsc::channel();
        let (release, pending) = mpsc::channel();
        let pending = Mutex::new(pending);
        let link = Link {
            transport: Arc::new(Sent(sent)),
            warn: Arc::new(|warning| panic!("想定しない警告: {warning}")),
            load_roadmap: Arc::new(move |_, _| {
                reading
                    .send(())
                    .expect("ロードマップの更新を知らせられなかった");
                pending
                    .lock()
                    .expect("更新を揃えられなかった")
                    .recv_timeout(timeout)
                    .expect("更新を再開できなかった");
                Ok(Vec::new())
            }),
            interval: web::INTERVAL,
        };
        let cancel = AtomicBool::new(false);
        let build = Build::current();
        let start = Start {
            runner: &System,
            launcher: &System,
            clock: &SystemClock::new(&cancel),
            queues: &System,
            notifier: &System,
            terminal: false,
            time_zone: &TimeZone::UTC,
            build: &build,
            web: &link,
            root,
            timing: Timing::default(),
        };
        let now: Timestamp = "2026-11-19T00:00:00Z".parse().expect("日時を読めなかった");
        let later = now
            .checked_add(SignedDuration::from_mins(6))
            .expect("日時を進められなかった");
        let mut reporting =
            Reporting::new(endpoint, &start, &config).expect("報告を始められなかった");
        reporting.use_roadmap(Vec::new(), now);
        reporting.update(false, now);
        let first = next_report(&received, "初回の報告が届かなかった");
        assert_eq!(first.progress.len(), 1, "初回の状態が欠けている");
        let (locked, confirmed) = mpsc::channel();
        let (unlock, release_lock) = mpsc::channel();
        let held = thread::spawn(move || {
            let _held = plan.lock().expect("競合するロックを取れなかった");
            locked.send(()).expect("ロック取得を知らせられなかった");
            release_lock
                .recv_timeout(timeout)
                .expect("報告がロックの解放を待った");
        });
        confirmed
            .recv_timeout(timeout)
            .expect("ロックを取れなかった");

        reporting.update(true, later);
        started
            .recv_timeout(timeout)
            .expect("ロードマップの更新が始まらなかった");
        let during = next_report(&received, "ロック中の報告が届かなかった");
        assert!(during.running, "状態の変更を反映していない");
        assert_eq!(
            during.progress, first.progress,
            "前に読んだ状態を保持していない"
        );
        reporting.update(false, later);
        let after = next_report(&received, "ロードマップを待つ間に次の報告が届かなかった");
        assert!(!after.running, "次の状態の変更を反映していない");
        unlock.send(()).expect("ロックの解放を知らせられなかった");
        held.join().expect("ロックの確認に失敗した");
        release
            .send(())
            .expect("ロードマップの更新を再開できなかった");
    }
}
