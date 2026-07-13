use gpui::{App, Hsla, SharedString};
use ui::prelude::*;

use crate::runner::{ProcessOutput, SampleResult};

#[derive(Clone)]
pub struct CommandOutputState {
    pub items: Vec<CommandOutputItem>,
}

#[derive(Clone)]
pub struct CommandOutputItem {
    pub label: SharedString,
    pub status: CommandOutputItemStatus,
    pub detail: Option<CommandOutputDetail>,
}

#[derive(Clone)]
pub struct CommandOutputDetail {
    pub heading: SharedString,
    pub diff: Option<OutputDiff>,
    pub sections: Vec<CommandOutputDetailSection>,
}

#[derive(Clone)]
pub struct OutputDiff {
    pub expected: SharedString,
    pub actual: SharedString,
}

#[derive(Clone)]
pub struct CommandOutputDetailSection {
    pub title: SharedString,
    pub body: SharedString,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandOutputItemStatus {
    Pending,
    Passed,
    Warning,
    Failed,
}

#[derive(Clone)]
pub struct CaseResult {
    pub status: CommandOutputItemStatus,
    pub heading: SharedString,
    pub actual: Option<String>,
    pub stderr: Option<String>,
}

pub struct SampleTestSummary {
    pub build_item: CommandOutputItem,
    pub build_succeeded: bool,
    pub total_cases: usize,
    pub passed_cases: usize,
    pub case_results: Vec<(usize, CaseResult)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutputSelection {
    Step(usize),
    Case(usize),
}

impl CommandOutputState {
    pub fn empty() -> Self {
        Self { items: Vec::new() }
    }

    pub fn running(label: &'static str) -> Self {
        Self {
            items: vec![CommandOutputItem {
                label: label.into(),
                status: CommandOutputItemStatus::Pending,
                detail: Some(CommandOutputDetail::new("Waiting for command output...")),
            }],
        }
    }
}

impl CommandOutputDetail {
    pub fn new(heading: impl Into<SharedString>) -> Self {
        Self {
            heading: heading.into(),
            diff: None,
            sections: Vec::new(),
        }
    }

    pub fn with_sections(
        heading: impl Into<SharedString>,
        sections: Vec<CommandOutputDetailSection>,
    ) -> Self {
        Self {
            heading: heading.into(),
            diff: None,
            sections,
        }
    }
}

pub fn single_command_output_state(
    label: impl Into<SharedString>,
    status: CommandOutputItemStatus,
    detail: impl Into<SharedString>,
) -> CommandOutputState {
    CommandOutputState {
        items: vec![CommandOutputItem {
            label: label.into(),
            status,
            detail: Some(CommandOutputDetail::new(detail)),
        }],
    }
}

pub fn sample_case_results(results: &[(usize, SampleResult)]) -> Vec<(usize, CaseResult)> {
    results
        .iter()
        .map(|(index, result)| {
            let case = match result {
                SampleResult::Ac { actual, stderr } => CaseResult {
                    status: CommandOutputItemStatus::Passed,
                    heading: "Accepted".into(),
                    actual: Some(actual.clone()),
                    stderr: (!stderr.trim().is_empty()).then(|| stderr.clone()),
                },
                SampleResult::Wa { actual, stderr, .. } => CaseResult {
                    status: CommandOutputItemStatus::Failed,
                    heading: "Wrong Answer".into(),
                    actual: Some(actual.clone()),
                    stderr: (!stderr.trim().is_empty()).then(|| stderr.clone()),
                },
                SampleResult::Re { stderr } => CaseResult {
                    status: CommandOutputItemStatus::Failed,
                    heading: "Runtime Error".into(),
                    actual: None,
                    stderr: Some(stderr.clone()),
                },
                SampleResult::Tle => CaseResult {
                    status: CommandOutputItemStatus::Failed,
                    heading: "Time Limit Exceeded".into(),
                    actual: None,
                    stderr: None,
                },
            };
            (*index, case)
        })
        .collect()
}

pub fn process_output_item(
    step: &ProcessOutput,
    heading: impl Into<SharedString>,
) -> CommandOutputItem {
    let mut sections = Vec::new();
    if !step.stdout.trim().is_empty() {
        sections.push(CommandOutputDetailSection {
            title: "stdout".into(),
            body: step.stdout.trim_end().to_string().into(),
        });
    }
    if !step.stderr.trim().is_empty() {
        sections.push(CommandOutputDetailSection {
            title: "stderr".into(),
            body: step.stderr.trim_end().to_string().into(),
        });
    }
    CommandOutputItem {
        label: step.label.into(),
        status: if step.status.success() {
            CommandOutputItemStatus::Passed
        } else {
            CommandOutputItemStatus::Failed
        },
        detail: Some(CommandOutputDetail::with_sections(heading, sections)),
    }
}

pub fn sample_test_summary(
    report: crate::runner::SampleTestReport,
    build_heading: impl Into<SharedString>,
) -> SampleTestSummary {
    let build_succeeded = report.build.status.success();
    let mut build_item = process_output_item(&report.build, build_heading);
    if build_succeeded && report.build.stderr.contains("warning") {
        build_item.status = CommandOutputItemStatus::Warning;
    }

    let total_cases = report.cases.len();
    let passed_cases = report
        .cases
        .iter()
        .filter(|(_, result)| matches!(result, SampleResult::Ac { .. }))
        .count();
    let case_results = build_succeeded
        .then(|| sample_case_results(&report.cases))
        .unwrap_or_default();

    SampleTestSummary {
        build_item,
        build_succeeded,
        total_cases,
        passed_cases,
        case_results,
    }
}

pub fn status_dot_color(status: CommandOutputItemStatus, cx: &App) -> Hsla {
    match status {
        CommandOutputItemStatus::Pending => cx.theme().colors().border_variant,
        CommandOutputItemStatus::Passed => cx.theme().status().success,
        CommandOutputItemStatus::Warning => cx.theme().status().warning,
        CommandOutputItemStatus::Failed => cx.theme().status().error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_sample_results_to_case_results() {
        let cases = sample_case_results(&[
            (
                0,
                SampleResult::Ac {
                    actual: "ok".into(),
                    stderr: "diagnostic".into(),
                },
            ),
            (
                1,
                SampleResult::Wa {
                    actual: "actual".into(),
                    expected: "expected".into(),
                    stderr: String::new(),
                },
            ),
            (
                2,
                SampleResult::Re {
                    stderr: "panic".into(),
                },
            ),
            (3, SampleResult::Tle),
        ]);

        assert_eq!(cases.len(), 4);
        assert_eq!(cases[0].1.status, CommandOutputItemStatus::Passed);
        assert_eq!(cases[0].1.heading.as_ref(), "Accepted");
        assert_eq!(cases[0].1.stderr.as_deref(), Some("diagnostic"));
        assert_eq!(cases[1].1.status, CommandOutputItemStatus::Failed);
        assert_eq!(cases[1].1.actual.as_deref(), Some("actual"));
        assert_eq!(cases[1].1.stderr, None);
        assert_eq!(cases[2].1.stderr.as_deref(), Some("panic"));
        assert_eq!(cases[3].1.heading.as_ref(), "Time Limit Exceeded");
    }

    #[test]
    fn summarizes_build_and_case_results() {
        let report = crate::runner::SampleTestReport {
            build: ProcessOutput {
                label: "cargo build",
                executable: "cargo".into(),
                args: vec!["build".into()],
                status: success_exit_status(),
                stdout: String::new(),
                stderr: "warning: unused variable".into(),
            },
            cases: vec![
                (
                    0,
                    SampleResult::Ac {
                        actual: "ok".into(),
                        stderr: String::new(),
                    },
                ),
                (
                    1,
                    SampleResult::Wa {
                        actual: "actual".into(),
                        expected: "expected".into(),
                        stderr: String::new(),
                    },
                ),
            ],
        };

        let summary = sample_test_summary(report, "cargo build succeeded");

        assert!(summary.build_succeeded);
        assert_eq!(summary.build_item.status, CommandOutputItemStatus::Warning);
        assert_eq!(summary.total_cases, 2);
        assert_eq!(summary.passed_cases, 1);
        assert_eq!(summary.case_results.len(), 2);
    }

    #[cfg(unix)]
    fn success_exit_status() -> std::process::ExitStatus {
        std::os::unix::process::ExitStatusExt::from_raw(0)
    }

    #[cfg(windows)]
    fn success_exit_status() -> std::process::ExitStatus {
        std::os::windows::process::ExitStatusExt::from_raw(0)
    }
}
