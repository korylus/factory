//! 監督のループが作業を始める前に、ウェブアプリから稼働と担当ごとのエージェントの設定を読む。
//!
//! 読めた設定は、前と変わったときに最後に読めた設定のファイルへ書く。
//! 届かないときは、最後に読めた設定 (ファイルも無ければ稼働と既定のエージェント) に従い、読み直す間隔が過ぎるまで読み直さない。
//! 担当ごとのエージェントは枠ごとに検証し、検証できない枠は警告を出して、最後に読めたその枠の値 (無ければ既定) を使う。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use jiff::{SignedDuration, Timestamp};
use korylusfactory_protocol::{AgentEntryResult, AgentSetting, Slot};

use super::Clock;
use crate::describe;
use crate::operation::{self, Saved, slot_label};
use crate::web::{self, Endpoint, Link};

pub(super) struct Operation {
    endpoint: Endpoint,
    link: Link,
    root: PathBuf,
    /// 最後に読めた設定。まだ読めておらず、ファイルも無ければ `None`。
    known: Option<Saved>,
    /// 最後にファイルに書けた設定。書き込みに失敗している間は known と食い違う。
    persisted: Option<Saved>,
    /// 書き込みが失敗してから出した最後の警告。
    write_warning: Option<String>,
    /// 最後に読めなかった時刻。読めたら `None` に戻す。
    failed_at: Option<Timestamp>,
    /// 最後に読めてから出した警告。
    warned: HashSet<String>,
    /// 枠ごとに、検証できなかった値について最後に出した警告。検証できる値か既定に戻ったら外す。
    agent_warned: HashMap<Slot, String>,
    /// 読めなかったあと、読み直すまで空ける間隔。
    interval: SignedDuration,
}

impl Operation {
    /// endpointから設定を読む。最後に読めた設定は、プロジェクトのルートrootのファイルから読んでおく。
    pub(super) fn new(
        endpoint: &Endpoint,
        link: &Link,
        root: PathBuf,
        interval: SignedDuration,
    ) -> Self {
        let known = match operation::load(&root) {
            Ok(known) => known,
            Err(error) => {
                (link.warn)(&format!("{}。稼働として進めます", describe(&error)));
                None
            }
        };
        Self {
            endpoint: endpoint.clone(),
            link: link.clone(),
            root,
            persisted: known.clone(),
            known,
            write_warning: None,
            failed_at: None,
            warned: HashSet::new(),
            agent_warned: HashMap::new(),
            interval,
        }
    }

    /// プロジェクトを止めているかを返す。前に読めなかったときは、間隔が過ぎてから読み直す。
    pub(super) fn paused(&mut self, clock: &dyn Clock) -> bool {
        if self
            .failed_at
            .is_none_or(|failed| clock.now().duration_since(failed) >= self.interval)
        {
            self.read(clock);
            self.persist();
        }
        self.known.as_ref().is_some_and(|known| known.paused)
    }

    /// 枠slotのエージェントの設定を返す。最後に読めた設定に無ければ既定を返す。
    ///
    /// 稼働の設定と同じ読み取り ([`Self::paused`]) で読んだ値を返し、ここでは読み直さない。
    pub(super) fn agent(&self, slot: Slot) -> AgentSetting {
        self.known
            .as_ref()
            .map_or_else(|| slot.default_setting(), |known| known.agent(slot))
    }

    fn read(&mut self, clock: &dyn Clock) {
        let result = web::read_settings(&self.endpoint, &self.link);
        let finished_at = clock.now();
        match result {
            Ok(settings) => {
                self.failed_at = None;
                self.warned.clear();
                let agents = self.agents(&settings.agents);
                let changed = self
                    .known
                    .as_ref()
                    .is_none_or(|known| known.paused != settings.paused || known.agents != agents);
                if changed {
                    self.known = Some(Saved {
                        paused: settings.paused,
                        agents,
                        read_at: finished_at,
                    });
                }
            }
            Err(reason) => {
                self.failed_at = Some(finished_at);
                let fallback = match &self.known {
                    Some(known) if known.paused => "最後に読めた停止の設定に従います",
                    Some(_) => "最後に読めた稼働の設定に従います",
                    None => "稼働として進めます",
                };
                self.warn(&format!("{reason}。{fallback}"));
            }
        }
    }

    /// 応答の担当ごとのエージェントreceivedを枠ごとに検証し、使う設定を返す。
    ///
    /// 載っていない枠は既定にする。検証できない枠は、枠と理由が前と違うときだけ警告し、最後に読めたその枠の値を使う。
    fn agents(
        &mut self,
        received: &BTreeMap<String, AgentEntryResult>,
    ) -> BTreeMap<Slot, AgentSetting> {
        let mut agents = BTreeMap::new();
        for slot in Slot::ALL {
            let Some(entry) = received.get(slot.value()) else {
                self.agent_warned.remove(&slot);
                continue;
            };
            match entry.validate() {
                Ok(setting) => {
                    self.agent_warned.remove(&slot);
                    agents.insert(slot, setting);
                }
                Err(error) => {
                    let previous = self
                        .known
                        .as_ref()
                        .and_then(|known| known.agents.get(&slot));
                    let fallback = if previous.is_some() {
                        "最後に読めたこの枠の設定に従います"
                    } else {
                        "既定のエージェントで動かします"
                    };
                    let warning = format!(
                        "ウェブアプリの担当ごとのエージェントの設定 ({}) を使えません: {error}。{fallback}",
                        slot_label(slot)
                    );
                    if self.agent_warned.get(&slot) != Some(&warning) {
                        (self.link.warn)(&warning);
                        self.agent_warned.insert(slot, warning);
                    }
                    if let Some(previous) = previous {
                        agents.insert(slot, previous.clone());
                    }
                }
            }
        }
        agents
    }

    /// 未保存の設定は、同じ値を読み取った回や、通信に失敗した回にも保存を試みる。
    fn persist(&mut self) {
        let Some(known) = &self.known else {
            return;
        };
        if self.persisted.as_ref() == Some(known) {
            return;
        }
        match operation::save(&self.root, known) {
            Ok(()) => {
                self.persisted = Some(known.clone());
                self.write_warning = None;
            }
            Err(error) => {
                let warning = format!("{}。読んだ設定に従います", describe(&error));
                if self.write_warning.as_ref() != Some(&warning) {
                    (self.link.warn)(&warning);
                    self.write_warning = Some(warning);
                }
            }
        }
    }

    /// 最後に読めてから同じ警告を出していなければ、textを警告として出す。
    fn warn(&mut self, text: &str) {
        if self.warned.insert(text.to_owned()) {
            (self.link.warn)(text);
        }
    }
}
