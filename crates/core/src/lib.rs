pub mod config;
pub mod feedback;
pub mod generations;
pub mod i18n;
pub mod storage;
pub mod types;

/// The addon's directory name under the game's `addons` folder, as Nexus
/// resolves it for the running DLL (`<addons>/gw2_build_optimizer/`). The
/// developer settings in `dev.cfg` derive the cache path from the same name.
pub const ADDON_DIR_NAME: &str = "gw2_build_optimizer";

/// `Display`/`to_string` of `reqwest::Error` is only the top wrapper
/// (`error sending request for url (...)`). Walk `source()` so the UI and
/// logs still show the TLS/IO cause (Proton/Wine schannel revocation, rustls
/// handshake, and so on).
pub fn format_error_chain(err: &dyn std::error::Error) -> String {
    let mut msg = err.to_string();
    let mut cause = err.source();
    while let Some(e) = cause {
        msg.push_str(": ");
        msg.push_str(&e.to_string());
        cause = e.source();
    }
    msg
}

#[cfg(test)]
mod format_error_chain_tests {
    use super::format_error_chain;
    use std::fmt;

    #[derive(Debug)]
    struct Cause(&'static str);
    impl fmt::Display for Cause {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }
    impl std::error::Error for Cause {}

    #[derive(Debug)]
    struct Wrapper {
        msg: &'static str,
        source: Cause,
    }
    impl fmt::Display for Wrapper {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.msg)
        }
    }
    impl std::error::Error for Wrapper {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.source)
        }
    }

    #[test]
    fn includes_chained_source_in_the_formatted_string() {
        let err = Wrapper {
            msg: "error sending request for url (https://api.guildwars2.com/v2)",
            source: Cause("invalid peer certificate: CERT_E_REVOCATION_FAILURE"),
        };
        let before = err.to_string();
        let after = format_error_chain(&err);
        assert_eq!(
            before,
            "error sending request for url (https://api.guildwars2.com/v2)"
        );
        assert_eq!(
            after,
            "error sending request for url (https://api.guildwars2.com/v2): invalid peer certificate: CERT_E_REVOCATION_FAILURE"
        );
    }
}
