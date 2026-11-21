//! 状態の報告を受け取ったことを、開いている画面へ知らせるSSE (`GET /events`)。
//!
//! 知らせるのは、どのプロジェクトの報告を受け取ったかだけにする。
//! プロジェクト名が空白だけのときは、すべてのプロジェクトを取り直す。
//! 画面は知らせを受けたら、表示している部分を取り直す。

use std::convert::Infallible;

use axum::extract::State;
use axum::http::{HeaderName, HeaderValue, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse as _, Response};
use futures_util::stream::{self, Stream};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, watch};

use crate::app::AppState;

/// SSEのパス。
pub const PATH: &str = "/events";

/// 報告を受け取ったときに送るイベントの名前。画面の `hx-trigger` で待ち受ける。
pub const STATUS_CHANGED: &str = "status-changed";

// 空白だけのプロジェクト名は設定で拒否される。空文字列を渡すとaxumはdata行を省く。
const ALL_PROJECTS: &str = " ";

/// 読み手が追い付けないときに溜めておく知らせの数。
const CAPACITY: usize = 64;

/// SSEの応答に付けるヘッダー。
const HEADERS: [(HeaderName, &str); 2] = [
    (header::CACHE_CONTROL, "no-store"),
    (HeaderName::from_static("x-robots-tag"), "noindex"),
];

/// 開いている画面への知らせの送り口。
#[derive(Debug, Clone)]
pub struct Events {
    sender: broadcast::Sender<String>,
    closed: watch::Sender<bool>,
}

impl Default for Events {
    fn default() -> Self {
        Self::new()
    }
}

impl Events {
    /// 知らせの送り口を作る。
    pub fn new() -> Self {
        Self {
            sender: broadcast::Sender::new(CAPACITY),
            closed: watch::Sender::new(false),
        }
    }

    /// projectの報告を受け取ったことを、開いているすべての画面へ知らせる。
    pub fn publish(&self, project: &str) {
        // 開いている画面が無いときは送り先が無いだけなので、失敗を無視する。
        let _ = self.sender.send(project.to_owned());
    }

    /// 開いているSSEをすべて終わらせ、これから開くSSEもすぐに終わらせる。
    ///
    /// SSEは終わらない応答のため、止めるときに閉じないと、処理中の要求を待って止まるサーバーが止まらない。
    pub fn close(&self) {
        self.closed.send_replace(true);
    }

    /// 知らせを、SSEのイベントの並びとして受け取る。
    fn subscribe(&self) -> impl Stream<Item = Result<Event, Infallible>> + use<> {
        let closed = self.closed.subscribe();
        let receiver = self.sender.subscribe();
        stream::unfold(
            (receiver, closed),
            |(mut receiver, mut closed)| async move {
                if *closed.borrow_and_update() {
                    return None;
                }
                let project = tokio::select! {
                    message = receiver.recv() => match message {
                        Ok(project) => project,
                        // 取りこぼしたときは、どれかのプロジェクトが変わったことだけを知らせる。
                        Err(RecvError::Lagged(_)) => ALL_PROJECTS.to_owned(),
                        Err(RecvError::Closed) => return None,
                    },
                    _ = closed.wait_for(|closed| *closed) => return None,
                };
                let event = Event::default().event(STATUS_CHANGED).data(project);
                Some((Ok(event), (receiver, closed)))
            },
        )
    }
}

/// `GET /events`: 報告を受け取るたびに、どのプロジェクトが変わったかを送り続ける。
pub async fn stream(State(state): State<AppState>) -> Response {
    let mut response = Sse::new(state.events.subscribe())
        .keep_alive(KeepAlive::default())
        .into_response();
    for (name, value) in HEADERS {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    response
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::BodyDataStream;
    use futures_util::StreamExt as _;

    use super::*;

    /// 知らせの並びを、SSEの応答の本文として読む。
    fn body(events: &Events) -> BodyDataStream {
        Sse::new(events.subscribe())
            .into_response()
            .into_body()
            .into_data_stream()
    }

    async fn next_text(body: &mut BodyDataStream) -> Option<String> {
        let chunk = tokio::time::timeout(Duration::from_secs(5), body.next())
            .await
            .expect("SSEのイベントか終了が届かなかった")?
            .expect("本文を読めなかった");
        Some(String::from_utf8(chunk.to_vec()).expect("本文がUTF-8ではない"))
    }

    #[tokio::test]
    async fn 受け取ったプロジェクトを開いている画面へ知らせる() {
        let events = Events::new();
        let mut first = body(&events);
        let mut second = body(&events);

        events.publish("Annict");

        for body in [&mut first, &mut second] {
            assert_eq!(
                next_text(body).await.as_deref(),
                Some("event: status-changed\ndata: Annict\n\n"),
                "知らせの中身が違う"
            );
        }
    }

    #[tokio::test]
    async fn 閉じると開いているsseとこれから開くsseを終わらせる() {
        let events = Events::new();
        let mut open = body(&events);

        events.close();
        let mut later = body(&events);

        assert_eq!(
            next_text(&mut open).await,
            None,
            "開いていたSSEが終わらなかった"
        );
        assert_eq!(
            next_text(&mut later).await,
            None,
            "閉じたあとに開いたSSEが終わらなかった"
        );
    }

    #[tokio::test]
    async fn 取りこぼしたときもどれかが変わったことを知らせる() {
        let events = Events::new();
        let mut body = body(&events);

        for _ in 0..=CAPACITY {
            events.publish("Annict");
        }

        assert_eq!(
            next_text(&mut body).await.as_deref(),
            Some("event: status-changed\ndata:  \n\n"),
            "取りこぼしたあとの知らせが違う"
        );
    }
}
