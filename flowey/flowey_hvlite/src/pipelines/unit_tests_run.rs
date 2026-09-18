// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! See [`UnitTestsRunCli`]

use flowey::node::prelude::FlowPlatformLinuxDistro;
use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use flowey_lib_common::git_checkout::RepoSource;
use flowey_lib_hvlite::common::CommonProfile;
use flowey_lib_hvlite::run_cargo_nextest_run::NextestProfile;

/// Build and run the x64 Linux GNU unit-test and documentation-test suites.
#[derive(clap::Args)]
pub struct UnitTestsRunCli {
    #[clap(flatten)]
    local_run_args: Option<crate::pipelines_shared::cfg_common_params::LocalRunArgs>,
}

impl IntoPipeline for UnitTestsRunCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        if !matches!(backend_hint, PipelineBackendHint::Local) {
            anyhow::bail!("unit-tests-run is for local use only")
        }
        if std::env::consts::OS != "linux" || std::env::consts::ARCH != "x86_64" {
            anyhow::bail!("unit-tests-run requires an x86_64 Linux host")
        }

        let Self { local_run_args } = self;
        let mut pipeline = Pipeline::new();
        let openvmm_repo_source =
            RepoSource::ExistingClone(ReadVar::from_static(crate::repo_root()));
        let cfg_common_params = crate::pipelines_shared::cfg_common_params::get_cfg_common_params(
            &mut pipeline,
            backend_hint,
            local_run_args,
        )?;

        pipeline.inject_all_jobs_with(move |job| {
            job.dep_on(&cfg_common_params)
                .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request::Init)
                .dep_on(
                    |_| flowey_lib_hvlite::_jobs::cfg_hvlite_reposource::Params {
                        hvlite_repo_source: openvmm_repo_source.clone(),
                    },
                )
        });

        let target = target_lexicon::triple!("x86_64-unknown-linux-gnu");
        let (publish_results, _use_results) = pipeline.new_artifact("x64-linux-gnu-unit-tests");
        pipeline
            .new_job(
                FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                FlowArch::X86_64,
                "unit tests and doctests [x64-linux-gnu]",
            )
            .dep_on(
                |ctx| flowey_lib_hvlite::_jobs::build_and_run_nextest_unit_tests::Params {
                    junit_test_label: "x64-linux-gnu-unit-tests".into(),
                    target: target.clone(),
                    profile: CommonProfile::Debug,
                    nextest_profile: NextestProfile::Ci,
                    fail_job_on_test_fail: true,
                    artifact_dir: Some(ctx.publish_artifact(publish_results)),
                    done: ctx.new_done_handle(),
                },
            )
            .side_effect(
                |done| flowey_lib_hvlite::_jobs::build_and_run_doc_tests::Params {
                    target,
                    profile: CommonProfile::Debug,
                    done,
                },
            )
            .finish();

        Ok(pipeline)
    }
}
