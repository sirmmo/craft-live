//! `nextcloud-live`: the add-in. `nextcloud-live --manifest` prints its `plugin.json`; otherwise
//! it answers the app on standard input and output.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable, clippy::indexing_slicing)]
#![forbid(unsafe_code)]

fn main() {
    if std::env::args().any(|a| a == "--manifest") {
        println!("{:#}", nextcloud_live::manifest::manifest());
        return;
    }
    nextcloud_live::runtime::serve(nextcloud_live::commands::Live::new(), std::io::stdin(), std::io::stdout());
}
