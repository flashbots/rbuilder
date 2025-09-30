// The file has been placed there by the build script.

mod internal {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

use internal::{
    BUILT_TIME_UTC, CI_PLATFORM, FEATURES, GIT_COMMIT_HASH_SHORT, GIT_HEAD_REF, PROFILE,
    RUSTC_VERSION,
};
use rbuilder::utils::build_info::Version;

pub fn print_version_info() {
    println!(
        "commit:         {}",
        GIT_COMMIT_HASH_SHORT.unwrap_or_default()
    );
    println!("branch:         {}", GIT_HEAD_REF.unwrap_or_default());
    println!("build_platform: {:?}", CI_PLATFORM.unwrap_or_default());
    println!("build_time:     {BUILT_TIME_UTC}");
    println!("features:       {FEATURES:?}");
    println!("profile:        {PROFILE}");
    println!("rustc:          {RUSTC_VERSION}");
}

pub fn rbuilder_version() -> Version {
    let git_commit = {
        let mut commit = String::new();
        if let Some(hash) = GIT_COMMIT_HASH_SHORT {
            commit.push_str(hash);
        }
        if commit.is_empty() {
            commit.push_str("unknown");
        }
        commit
    };

    let git_ref = GIT_HEAD_REF.unwrap_or("unknown").to_string();

    Version {
        git_commit,
        git_ref,
        build_time_utc: BUILT_TIME_UTC.to_string(),
    }
}
