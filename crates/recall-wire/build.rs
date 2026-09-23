//! Decides this build's channel, which `discovery::channel` reports.
//!
//! `RECALL_BUILD_CHANNEL` wins when set: the release workflow sets
//! `release`, the server's Dockerfile sets `dev`. Otherwise the source
//! decides. A git checkout is a development build; a crate unpacked from
//! crates.io, which is what `cargo install recall` compiles, has no `.git`
//! and is exactly the release it was published as.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-env-changed=RECALL_BUILD_CHANNEL");
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let checkout = Path::new(&manifest_dir).join("../../.git").exists();
    let channel = match std::env::var("RECALL_BUILD_CHANNEL").as_deref() {
        Ok("release") => "release",
        Ok(_) => "dev",
        Err(_) if checkout => "dev",
        Err(_) => "release",
    };
    println!("cargo:rustc-env=RECALL_RESOLVED_CHANNEL={channel}");
}
