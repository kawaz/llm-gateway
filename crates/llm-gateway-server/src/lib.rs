//! HTTP 層。Anthropic Messages API を話す口を生やす。
//!
//! 実運用のログから、クライアントが叩くのは 3 つと分かっている:
//! `POST /v1/messages` / `POST /v1/messages/count_tokens` / `GET /v1/models`。
//! (`HEAD /api/hello` も大量に来るが、404 のままで支障が出ていない)

// TODO: 実装 (task #7)
