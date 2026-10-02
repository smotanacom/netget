//! CPU-only checks for drift between decoder targets and the on-demand fuzz job.

use std::collections::BTreeSet;

#[test]
fn every_fuzz_target_is_scheduled_by_the_fuzz_workflow() {
    let manifest: toml::Value = toml::from_str(include_str!("../fuzz/Cargo.toml")).unwrap();
    let targets: BTreeSet<_> = manifest["bin"]
        .as_array()
        .unwrap()
        .iter()
        .map(|target| target["name"].as_str().unwrap().to_owned())
        .collect();
    let workflow: serde_yaml::Value =
        serde_yaml::from_str(include_str!("../.github/workflows/fuzz.yml")).unwrap();
    let matrix = workflow["jobs"]["fuzz"]["strategy"]["matrix"]["target"]
        .as_sequence()
        .unwrap();
    let scheduled: BTreeSet<_> = matrix
        .iter()
        .map(|target| target.as_str().unwrap().to_owned())
        .collect();
    assert!(!targets.is_empty());
    assert_eq!(
        matrix.len(),
        scheduled.len(),
        "duplicate target wastes a matrix job"
    );
    assert_eq!(
        targets, scheduled,
        "each declared decoder must be searched when fuzzing is dispatched"
    );
}

#[test]
fn manual_workflow_inputs_are_passed_as_environment_data() {
    for source in [
        include_str!("../.github/workflows/nightly-eval.yml"),
        include_str!("../.github/workflows/nightly-soak.yml"),
        include_str!("../.github/workflows/fuzz.yml"),
    ] {
        let workflow: serde_yaml::Value = serde_yaml::from_str(source).unwrap();
        for job in workflow["jobs"].as_mapping().unwrap().values() {
            for step in job["steps"].as_sequence().unwrap() {
                if let Some(script) = step["run"].as_str() {
                    assert!(
                        !script.contains("${{ github.event.inputs"),
                        "dispatch data must not become shell source: {script}"
                    );
                    assert!(
                        !script.contains("${{ env.SECONDS_PER_TARGET"),
                        "indirect dispatch data must remain an environment variable: {script}"
                    );
                }
            }
        }
    }
}
