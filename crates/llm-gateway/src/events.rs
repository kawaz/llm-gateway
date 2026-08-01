//! 転送のたびに起きたことを、見ている人へ流す (DR-0012)。
//!
//! gateway は全部のリクエストを仲介しているので、**upstream が応答を返した
//! 瞬間**を知っている唯一の場所になる。prompt cache の 5 分は upstream が
//! 前処理を始めた時点から走るので、外から見える最良の近似がこの瞬間になる。
//!
//! 流すのは起きたことだけで、状態は持たない。誰も見ていなければ何もしない。
//! 見ている人が遅れたら、その人の分は落ちる — 5 分の残りを数える相手に、
//! 遅れて届いた開始時刻を渡しても使い道がない。

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::credential::time::format_rfc3339;

/// 会話の id を載せてくるリクエストヘッダ。
///
/// Claude Code が付ける。付けない相手 (curl 等) もいるので、無ければ
/// 会話を特定しないイベントとして流す。
pub const SESSION_HEADER: &str = "x-claude-code-session-id";

/// 溜めておける数。
///
/// 見ている人が遅れた分はここを溢れて落ちる。大きくしても「古い開始時刻が
/// まとめて届く」だけで、数え直す相手の役には立たない。
const BACKLOG: usize = 256;

/// upstream が応答を返した、という知らせ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// 応答のヘッダを受け取った時刻 (Unix 秒)。
    pub ts: i64,
    /// 同じ時刻の ISO 8601 表記。人が読む側で変換し直さずに済む。
    pub ts_iso: String,
    /// どの会話か。ヘッダを付けてこないクライアントでは `null`。
    pub session_id: Option<String>,
    /// どの namespace 宛か。
    pub ns: String,
    /// 解決後の実モデル名 (`opus` のような短い名前はここでは解決済み)。
    pub model: String,
    /// 答えた経路の名前 (= 設定に書いた credential の名前)。
    pub credential: String,
    pub status: u16,
}

impl Event {
    pub fn new(
        ts: i64,
        session_id: Option<String>,
        ns: &str,
        model: &str,
        credential: &str,
        status: u16,
    ) -> Self {
        Self {
            ts,
            ts_iso: format_rfc3339(ts),
            session_id,
            ns: ns.to_owned(),
            model: model.to_owned(),
            credential: credential.to_owned(),
            status,
        }
    }
}

/// 見ている人へ配る口。
pub struct Events {
    tx: broadcast::Sender<Event>,
}

impl Default for Events {
    fn default() -> Self {
        Self::new()
    }
}

impl Events {
    pub fn new() -> Self {
        Self {
            tx: broadcast::Sender::new(BACKLOG),
        }
    }

    /// 1 件流す。
    ///
    /// 誰も見ていなければ何もしない。**転送の邪魔をしないこと**が第一で、
    /// 配れなかったことを転送側へ持ち帰らない (待たない・失敗にしない)。
    pub fn publish(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    /// 見る側に回る。届くのは**これ以降**の分だけ。
    ///
    /// 過去に遡らないのは、この知らせが「今から 5 分」を数えるためのもの
    /// だから。接続した時点で既に過ぎている分を配っても数え直せない。
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// 今この口を見ている人の数。
    pub fn watchers(&self) -> usize {
        self.tx.receiver_count()
    }
}

/// リクエストヘッダから会話の id を拾う。無ければ `None`。
pub fn session_id(headers: &[(String, String)]) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(SESSION_HEADER))
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// 大文字小文字は問わない。相手の書き方に合わせない。
    #[test]
    fn the_session_header_is_read_in_any_case() {
        for name in [
            "x-claude-code-session-id",
            "X-Claude-Code-Session-Id",
            "X-CLAUDE-CODE-SESSION-ID",
        ] {
            assert_eq!(
                session_id(&headers(&[(name, "s-1")])),
                Some("s-1".to_owned()),
                "{name}"
            );
        }
    }

    /// 付けてこないクライアント (curl 等) もいる。会話を特定しないだけ。
    #[test]
    fn a_request_without_the_header_has_no_session() {
        assert_eq!(session_id(&[]), None);
        assert_eq!(session_id(&headers(&[("content-type", "json")])), None);
        assert_eq!(session_id(&headers(&[(SESSION_HEADER, "  ")])), None);
    }

    #[test]
    fn surrounding_space_is_trimmed() {
        assert_eq!(
            session_id(&headers(&[(SESSION_HEADER, " s-1 ")])),
            Some("s-1".to_owned())
        );
    }

    /// 誰も見ていなくても、流す側は何も気にしない。
    #[test]
    fn publishing_to_nobody_is_fine() {
        let events = Events::new();
        assert_eq!(events.watchers(), 0);
        events.publish(Event::new(NOW, None, "personal", "m", "a", 200));
    }

    #[tokio::test]
    async fn a_watcher_gets_what_happens_next() {
        let events = Events::new();
        let mut watching = events.subscribe();

        events.publish(Event::new(
            NOW,
            Some("s-1".to_owned()),
            "personal",
            "claude-fable-5",
            "claude-kawazzz",
            200,
        ));

        let got = watching.recv().await.unwrap();
        assert_eq!(got.session_id.as_deref(), Some("s-1"));
        assert_eq!(got.model, "claude-fable-5");
        assert_eq!(got.credential, "claude-kawazzz");
        assert_eq!(got.status, 200);
        assert_eq!(got.ts, NOW);
        assert_eq!(got.ts_iso, format_rfc3339(NOW));
    }

    /// 見始める前に起きたことは届かない。今から 5 分を数えるための知らせなので、
    /// 過ぎた分を配っても使えない。
    #[tokio::test]
    async fn nothing_is_replayed() {
        let events = Events::new();
        events.publish(Event::new(NOW, None, "personal", "m", "a", 200));

        let mut watching = events.subscribe();
        events.publish(Event::new(NOW + 1, None, "personal", "m", "b", 200));

        let got = watching.recv().await.unwrap();
        assert_eq!(got.ts, NOW + 1, "見始めた後の分だけ");
    }

    /// 断られた応答も流す。上限に当たったことも、見ている側には知らせ。
    #[tokio::test]
    async fn a_denial_is_an_event_too() {
        let events = Events::new();
        let mut watching = events.subscribe();
        events.publish(Event::new(NOW, None, "personal", "m", "a", 429));
        assert_eq!(watching.recv().await.unwrap().status, 429);
    }

    /// JSON の形。見る側 (ccmsg 等) が読む契約なので、欄の名前を固定する。
    #[test]
    fn the_json_shape_is_fixed() {
        let event = Event::new(NOW, Some("s-1".to_owned()), "personal", "m", "a", 200);
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["ts"], NOW);
        assert_eq!(json["ts_iso"], format_rfc3339(NOW));
        assert_eq!(json["session_id"], "s-1");
        assert_eq!(json["ns"], "personal");
        assert_eq!(json["model"], "m");
        assert_eq!(json["credential"], "a");
        assert_eq!(json["status"], 200);

        // 会話が分からない場合も欄は残す (欠けると、読む側が形を 2 通り扱う)。
        let nameless = Event::new(NOW, None, "personal", "m", "a", 200);
        assert!(serde_json::to_value(&nameless).unwrap()["session_id"].is_null());
    }
}
