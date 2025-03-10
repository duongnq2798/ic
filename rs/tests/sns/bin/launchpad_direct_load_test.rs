use anyhow::Result;
use std::time::Duration;

use ic_tests::driver::new::group::SystemTestGroup;
use ic_tests::driver::test_env::TestEnv;
use ic_tests::nns_tests::sns_aggregator::{benchmark_config, workload_direct};
use ic_tests::nns_tests::sns_deployment::initiate_token_swap;
use ic_tests::systest;

const WORKLOAD_DURATION: Duration = Duration::from_secs(2 * 60);

fn workload_direct_rps300(env: TestEnv) {
    let rps = 300;
    let duration = WORKLOAD_DURATION;
    workload_direct(env, rps, duration);
}

fn workload_direct_rps600(env: TestEnv) {
    let rps = 600;
    let duration = WORKLOAD_DURATION;
    workload_direct(env, rps, duration);
}

fn workload_direct_rps1200(env: TestEnv) {
    let rps = 1200;
    let duration = WORKLOAD_DURATION;
    workload_direct(env, rps, duration);
}

fn workload_direct_rps2400(env: TestEnv) {
    let rps = 2400;
    let duration = WORKLOAD_DURATION;
    workload_direct(env, rps, duration);
}

fn workload_direct_rps4800(env: TestEnv) {
    let rps = 4800;
    let duration = WORKLOAD_DURATION;
    workload_direct(env, rps, duration);
}

/// This is a non-interactive load test. We model the behavior of (multiple) web-browsers
/// that, for some reason, cannot (or do not want to) use the aggregator canister,
/// so they resort to interacting with the SNS directly.
///
/// See https://github.com/dfinity/nns-dapp/blob/6b85f56b6f5261bf0d1e4a1848752828ff0f4238/frontend/src/lib/services/%24public/sns.services.ts#L82
///
/// 1. Install NNS and SNS
/// 2. Initiate the token sale
/// 3. Generate workload (mimicking nns-dapp frontent) at various RPSs
fn main() -> Result<()> {
    SystemTestGroup::new()
        .with_overall_timeout(Duration::from_secs(60 * 60))
        .with_timeout_per_test(Duration::from_secs(60 * 60))
        .with_setup(benchmark_config)
        .add_test(systest!(initiate_token_swap))
        .add_test(systest!(workload_direct_rps300))
        .add_test(systest!(workload_direct_rps600))
        .add_test(systest!(workload_direct_rps1200))
        .add_test(systest!(workload_direct_rps2400))
        .add_test(systest!(workload_direct_rps4800))
        .execute_from_args()?;
    Ok(())
}
