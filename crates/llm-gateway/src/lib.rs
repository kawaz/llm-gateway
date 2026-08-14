//! llm-gateway core.
//!
//! クライアントは Messages 形式の API を話す。gateway はモデル名から
//! upstream と認証情報を選び、最小限の加工をして転送する。
//!
//! 構成は DR-0002 / DR-0014 を参照:
//! - [`router`] モデル名 + session キー → 経路の優先順位。経路が使えるかは経路に聞く
//! - [`provider`] provider preset の契約 (Auth / Wire / Metering / 任意 capability) と経路の状態
//! - [`preset`] その契約の provider ごとの実装。方言と認証を組み合わせて束ねる
//! - [`egress`] upstream へ出るときの provider-neutral な HTTP の形と、出口の手順
//! - [`session`] クライアントの言い分から会話を見分ける (入口)
//! - [`credential`] token の取得とリフレッシュ。永続化はプラガブル
//! - [`denial`] 1 経路の状態機構 — 締め出しの印・枠・様子見 (DR-0009)
//! - [`exchange`] 本文を流し終えた (途切れた) ところの記録
//! - [`events`] 転送のたびに起きたことを見ている人へ流す (DR-0012)
//! - [`webhook`] その知らせを受け口へ送る (DR-0012)
//! - [`quota`] 枠の観測スナップショットと、その置き場 (DR-0007)
//! - [`metering`] トークン集計の正規形と、単価を引き当てる契約
//! - [`stats`] 応答の usage を日ごとに積む (DR-0011)

pub mod config;
pub mod credential;
pub mod denial;
pub mod discovery;
pub mod egress;
pub mod events;
pub mod exchange;
pub mod gateway;
pub mod metering;
pub mod pattern;
pub mod preset;
pub mod provider;
pub mod quota;
pub mod router;
pub mod session;
pub mod stats;
pub mod tap;
pub mod webhook;

pub use config::Config;
pub use gateway::Gateway;

mod error;
mod persist;

pub use error::{Error, Result};

/// core が provider の名前を 1 つも知らないことを、実ファイルを読んで確かめる。
///
/// DR-0014 §3 は「この設計が達成できたか」の判定基準を 1 つに定めている —
/// **core のコードに provider の名前が現れないこと**。現れたなら、方言の知識が
/// core へ漏れた印であり、3 つ目の provider を足すときに同じ場所を再び触る
/// ことになる。
///
/// 目視では守れないので試験にする。読むのは各 module の `#[cfg(test)]` より
/// 前 (= 実際に動く側) だけ。試験の中は upstream の実物を模した値を置く場所で、
/// そこに実名が出るのは漏れではない。
#[cfg(test)]
mod provider_neutrality {
    /// provider の顔ぶれを知らない module。
    ///
    /// 入口側 ([`crate::session`]) と設定 ([`crate::config`]) は対象外。
    /// クライアント方言と設定の語彙は、どの upstream を選ぶかとは別の軸で
    /// 実名を持つ (DR-0004 の 2 軸)。
    const GENERIC: &[&str] = &[
        "egress.rs",
        "provider.rs",
        "metering.rs",
        "quota.rs",
        "denial.rs",
        "router.rs",
        "gateway.rs",
        "stats.rs",
        "exchange.rs",
        "events.rs",
    ];

    /// 現れてはいけない名前 (小文字にしてから照合)。
    const NAMES: &[&str] = &["anthropic", "claude", "openai", "bedrock"];

    /// この module の src ディレクトリ。
    fn src() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
    }

    #[test]
    fn generic_core_never_names_a_provider() {
        let src = src();
        let mut leaks = Vec::new();

        for name in GENERIC {
            let path = src.join(name);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{} を読めません: {e}", path.display()));

            // 試験の手前までが「動く側」。試験を持たない module は全部が対象。
            let production = match text.find("#[cfg(test)]") {
                Some(at) => &text[..at],
                None => &text[..],
            };

            for (number, line) in production.lines().enumerate() {
                let lowered = line.to_lowercase();
                for word in NAMES {
                    if lowered.contains(word) {
                        leaks.push(format!("{name}:{}: {word} — {}", number + 1, line.trim()));
                    }
                }
            }
        }

        assert!(
            leaks.is_empty(),
            "core が provider の名前を知っています (DR-0014 §3):\n{}",
            leaks.join("\n")
        );
    }

    /// 検査する相手が実在することも確かめる。
    ///
    /// module を改名したときに、名前が合わないまま「漏れ 0 件」で通り続けると
    /// 判定そのものが黙って効かなくなる。
    #[test]
    fn every_listed_module_exists() {
        let src = src();
        for name in GENERIC {
            assert!(src.join(name).is_file(), "{name} が見つかりません");
        }
    }
}
