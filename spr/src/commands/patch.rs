/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use crate::error::Result;

#[derive(Debug, clap::Parser)]
pub struct PatchOptions {
    /// Pull Request number
    pull_request: u64,

    /// Name of the branch to be created. Defaults to `PR-<number>`
    #[clap(long)]
    branch_name: Option<String>,

    /// If given, create new branch but do not check out
    #[clap(long)]
    no_checkout: bool,
}

pub async fn patch(
    _opts: PatchOptions,
    _jj: &crate::jj::Jujutsu,
    _gh: &crate::github::GitHub,
    _config: &crate::config::Config,
) -> Result<()> {
    // TODO: Implement Jujutsu-native patch functionality
    // This command needs to be completely rewritten for Jujutsu workflow
    // The current implementation uses complex Git operations that need
    // to be translated to Jujutsu equivalents

    use crate::error::Error;
    Err(Error::new(
        "The patch command is not yet implemented for Jujutsu workflow. \
         Please use the GitHub web interface to create branches from pull requests for now."
            .to_string(),
    ))
}
