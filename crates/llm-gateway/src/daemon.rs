//! この端末で走らせる gateway プロセスの持ち物 (DR-0028)。
//!
//! 仕組みは汎用層 ([`gateway_core::daemon`]) が持つ。ここは置き場
//! (`$XDG_STATE_HOME/llm-gateway`) と、台への問い合わせ方を与える。
//!
//! - [`registry`] どの設定を 1 台として走らせるかの登録簿
//! - [`supervisor`] 登録された台を抱えて生かし続ける監督者
//! - [`protocol`] 監督者に頼むときの言葉

pub mod protocol {
    pub use gateway_core::daemon::protocol::*;

    use std::path::PathBuf;

    /// 監督者の待ち受け先。
    pub fn socket_path() -> PathBuf {
        gateway_core::daemon::protocol::socket_path(&crate::config::default_state_dir())
    }

    /// 子が書いたものの置き場。
    pub fn log_dir() -> PathBuf {
        gateway_core::daemon::protocol::log_dir(&crate::config::default_state_dir())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// 待ち受け先とログは、消えると困るので state の下 (cache ではない)。
        #[test]
        fn the_supervisor_lives_under_the_state_directory() {
            assert!(
                socket_path().ends_with("llm-gateway/daemon/supervisor.sock"),
                "{}",
                socket_path().display()
            );
            assert!(
                log_dir().ends_with("llm-gateway/logs"),
                "{}",
                log_dir().display()
            );
        }
    }
}

pub mod registry {
    pub use gateway_core::daemon::registry::*;

    use std::path::PathBuf;

    /// 既定の置き場。
    pub fn default_dir() -> PathBuf {
        gateway_core::daemon::registry::default_dir(&crate::config::default_state_dir())
    }

    /// 既定の置き場で開く。
    pub fn open() -> Registry {
        Registry::at(default_dir())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// 既定の置き場は state の下 (消えると再登録が要るので cache ではない)。
        #[test]
        fn the_default_directory_sits_under_the_state_directory() {
            let dir = default_dir();
            assert!(
                dir.ends_with("llm-gateway/daemon/units"),
                "{}",
                dir.display()
            );
        }
    }
}

pub mod supervisor {
    pub use gateway_core::daemon::supervisor::*;

    use std::path::Path;

    /// gateway の台への問い合わせ方。待ち受け先は台の設定の `[server] listen`。
    pub const PROBE: UnitProbe = UnitProbe {
        listen_of,
        health_path: "/llm-gateway/healthz",
        self_path: "/llm-gateway/self",
        version_path: "/llm-gateway/version",
    };

    fn listen_of(config: &Path) -> Option<String> {
        crate::Config::load(config)
            .ok()
            .map(|config| config.server.listen)
    }

    /// 既定の置き場で組み立てる。
    pub fn open() -> Supervisor {
        Supervisor::new(
            super::registry::open(),
            super::protocol::log_dir(),
            super::protocol::socket_path(),
            PROBE,
        )
    }
}
