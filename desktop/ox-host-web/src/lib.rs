//! Remote access service and its same-user desktop administration interface.
pub mod access;
pub mod admin;
#[cfg(feature = "server")]
mod bridge;
#[cfg(feature = "client")]
pub mod client;
pub mod host_profile;
pub mod host_updates;
#[cfg(feature = "server")]
mod push;
pub mod serve;
#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
mod state;
#[cfg(feature = "server")]
pub use server::run;

#[cfg(all(test, feature = "server"))]
mod fixture {
    #[test]
    #[ignore = "launched by the disposable browser fixture"]
    fn browser_service() {
        let dir =
            std::env::var_os("OX_TEST_REMOTE_DIR").expect("isolated browser test state directory");
        let root = std::path::PathBuf::from(dir);
        super::run(root.clone(), root.join("mock-host.sock")).unwrap();
    }
}

#[cfg(feature = "server")]
mod cartridges;

#[cfg(any(feature = "client", feature = "server"))]
pub mod terminal_protocol;
