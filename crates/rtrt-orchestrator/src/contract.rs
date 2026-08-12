use std::{error::Error, fmt, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::protocol::TaskId;

pub const DEFAULT_MAX_SUMMARY_LINES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    Passed,
    Failed,
    NotRun,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestResult {
    pub command: String,
    pub status: TestStatus,
}

impl TestResult {
    pub fn new(command: impl Into<String>, status: TestStatus) -> Self {
        Self {
            command: command.into(),
            status,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerReturn {
    pub files: Vec<PathBuf>,
    pub summary: String,
    pub tests: Vec<TestResult>,
    pub detail_ref: String,
}

impl WorkerReturn {
    pub fn new(
        files: Vec<PathBuf>,
        summary: impl Into<String>,
        tests: Vec<TestResult>,
        detail_ref: impl Into<String>,
    ) -> Result<Self, ContractError> {
        Self::new_with_summary_limit(files, summary, tests, detail_ref, DEFAULT_MAX_SUMMARY_LINES)
    }

    pub fn new_with_summary_limit(
        files: Vec<PathBuf>,
        summary: impl Into<String>,
        tests: Vec<TestResult>,
        detail_ref: impl Into<String>,
        max_summary_lines: usize,
    ) -> Result<Self, ContractError> {
        let worker_return = Self {
            files,
            summary: summary.into(),
            tests,
            detail_ref: detail_ref.into(),
        };
        worker_return.validate_with_summary_limit(max_summary_lines)?;
        Ok(worker_return)
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        self.validate_with_summary_limit(DEFAULT_MAX_SUMMARY_LINES)
    }

    pub fn validate_with_summary_limit(
        &self,
        max_summary_lines: usize,
    ) -> Result<(), ContractError> {
        if self.files.is_empty() {
            return Err(ContractError::MissingFiles);
        }
        if let Some(index) = self
            .files
            .iter()
            .position(|path| path.as_os_str().is_empty())
        {
            return Err(ContractError::EmptyFile { index });
        }

        let actual_lines = self.summary.lines().count();
        if actual_lines > max_summary_lines {
            return Err(ContractError::SummaryTooLong {
                max_lines: max_summary_lines,
                actual_lines,
            });
        }

        if self.tests.is_empty() {
            return Err(ContractError::MissingTests);
        }
        if let Some(index) = self
            .tests
            .iter()
            .position(|test| test.command.trim().is_empty())
        {
            return Err(ContractError::EmptyTestCommand { index });
        }
        if self.detail_ref.trim().is_empty() {
            return Err(ContractError::MissingDetailRef);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FallbackProvenance {
    pub from_lane: String,
    pub reason: String,
    pub redo_from_scratch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultProvenance {
    pub assigned_lane: String,
    pub actual_lane: String,
    pub fallback: Option<FallbackProvenance>,
}

impl ResultProvenance {
    pub fn assigned(lane: impl Into<String>) -> Self {
        let lane = lane.into();
        Self {
            assigned_lane: lane.clone(),
            actual_lane: lane,
            fallback: None,
        }
    }

    pub fn fallback(
        assigned_lane: impl Into<String>,
        actual_lane: impl Into<String>,
        from_lane: impl Into<String>,
        reason: impl Into<String>,
        redo_from_scratch: bool,
    ) -> Self {
        Self {
            assigned_lane: assigned_lane.into(),
            actual_lane: actual_lane.into(),
            fallback: Some(FallbackProvenance {
                from_lane: from_lane.into(),
                reason: reason.into(),
                redo_from_scratch,
            }),
        }
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        if self.assigned_lane.trim().is_empty() {
            return Err(ContractError::MissingLane {
                field: "assigned_lane",
            });
        }
        if self.actual_lane.trim().is_empty() {
            return Err(ContractError::MissingLane {
                field: "actual_lane",
            });
        }

        match (&self.fallback, self.assigned_lane == self.actual_lane) {
            (None, false) => return Err(ContractError::MissingFallbackProvenance),
            (Some(_), true) => return Err(ContractError::UnexpectedFallbackProvenance),
            (Some(fallback), false) => {
                if fallback.from_lane.trim().is_empty() {
                    return Err(ContractError::MissingFallbackLane);
                }
                if fallback.reason.trim().is_empty() {
                    return Err(ContractError::MissingFallbackReason);
                }
            }
            (None, true) => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerResult {
    pub output: WorkerReturn,
    pub provenance: ResultProvenance,
}

impl WorkerResult {
    pub fn validate(&self) -> Result<(), ContractError> {
        self.validate_with_summary_limit(DEFAULT_MAX_SUMMARY_LINES)
    }

    pub fn validate_with_summary_limit(
        &self,
        max_summary_lines: usize,
    ) -> Result<(), ContractError> {
        self.output.validate_with_summary_limit(max_summary_lines)?;
        self.provenance.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskWriteSet {
    pub task_id: TaskId,
    pub writes: Vec<PathBuf>,
}

impl TaskWriteSet {
    pub fn new(task_id: TaskId, writes: Vec<PathBuf>) -> Self {
        Self { task_id, writes }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskIsolation {
    Shared,
    Worktree,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedTask {
    pub task: TaskWriteSet,
    pub isolation: TaskIsolation,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionWave {
    pub tasks: Vec<PlannedTask>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IsolationPlan {
    pub waves: Vec<ExecutionWave>,
}

impl IsolationPlan {
    pub fn new(tasks: Vec<TaskWriteSet>) -> Self {
        plan_isolation(tasks)
    }

    pub fn tasks(&self) -> impl Iterator<Item = &PlannedTask> {
        self.waves.iter().flat_map(|wave| &wave.tasks)
    }
}

/// Builds stable, contiguous waves. Tasks in one wave never have overlapping
/// write sets; any task participating in an overlap gets a dedicated worktree.
pub fn plan_isolation(tasks: Vec<TaskWriteSet>) -> IsolationPlan {
    let needs_worktree = tasks
        .iter()
        .enumerate()
        .map(|(index, task)| {
            tasks.iter().enumerate().any(|(other_index, other)| {
                index != other_index && write_sets_overlap(&task.writes, &other.writes)
            })
        })
        .collect::<Vec<_>>();

    let mut waves: Vec<ExecutionWave> = Vec::new();
    for (task, needs_worktree) in tasks.into_iter().zip(needs_worktree) {
        let planned = PlannedTask {
            task,
            isolation: if needs_worktree {
                TaskIsolation::Worktree
            } else {
                TaskIsolation::Shared
            },
        };

        match waves.last_mut() {
            Some(wave)
                if wave.tasks.iter().all(|current| {
                    !write_sets_overlap(&current.task.writes, &planned.task.writes)
                }) =>
            {
                wave.tasks.push(planned);
            }
            _ => waves.push(ExecutionWave {
                tasks: vec![planned],
            }),
        }
    }
    IsolationPlan { waves }
}

pub fn write_sets_overlap(left: &[PathBuf], right: &[PathBuf]) -> bool {
    left.iter().any(|left_path| {
        right.iter().any(|right_path| {
            left_path == right_path
                || left_path.starts_with(right_path)
                || right_path.starts_with(left_path)
        })
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractError {
    MissingFiles,
    EmptyFile {
        index: usize,
    },
    SummaryTooLong {
        max_lines: usize,
        actual_lines: usize,
    },
    MissingTests,
    EmptyTestCommand {
        index: usize,
    },
    MissingDetailRef,
    MissingLane {
        field: &'static str,
    },
    MissingFallbackProvenance,
    UnexpectedFallbackProvenance,
    MissingFallbackLane,
    MissingFallbackReason,
}

impl fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingFiles => formatter.write_str("modified-file list is required"),
            Self::EmptyFile { index } => {
                write!(formatter, "modified file at index {index} is empty")
            }
            Self::SummaryTooLong {
                max_lines,
                actual_lines,
            } => write!(
                formatter,
                "summary has {actual_lines} lines; maximum is {max_lines}"
            ),
            Self::MissingTests => formatter.write_str("test status is required"),
            Self::EmptyTestCommand { index } => {
                write!(formatter, "test command at index {index} is empty")
            }
            Self::MissingDetailRef => formatter.write_str("detail reference is required"),
            Self::MissingLane { field } => write!(formatter, "{field} is required"),
            Self::MissingFallbackProvenance => {
                formatter.write_str("a lane change requires fallback provenance")
            }
            Self::UnexpectedFallbackProvenance => {
                formatter.write_str("an unchanged lane must not have fallback provenance")
            }
            Self::MissingFallbackLane => formatter.write_str("fallback source lane is required"),
            Self::MissingFallbackReason => formatter.write_str("fallback reason is required"),
        }
    }
}

impl Error for ContractError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, writes: &[&str]) -> TaskWriteSet {
        TaskWriteSet::new(
            TaskId::new(id).unwrap(),
            writes.iter().map(PathBuf::from).collect(),
        )
    }

    fn valid_return() -> WorkerReturn {
        WorkerReturn::new(
            vec![PathBuf::from("src/lib.rs")],
            "implemented contract",
            vec![TestResult::new("cargo test", TestStatus::Passed)],
            "memory://run/detail",
        )
        .unwrap()
    }

    fn task_ids(wave: &ExecutionWave) -> Vec<&str> {
        wave.tasks
            .iter()
            .map(|planned| planned.task.task_id.as_str())
            .collect()
    }

    #[test]
    fn non_overlapping_tasks_share_one_parallel_wave() {
        let plan = plan_isolation(vec![
            task("first", &["src/a.rs"]),
            task("second", &["src/b.rs"]),
        ]);

        assert_eq!(plan.waves.len(), 1);
        assert_eq!(task_ids(&plan.waves[0]), vec!["first", "second"]);
        assert!(
            plan.tasks()
                .all(|task| task.isolation == TaskIsolation::Shared)
        );
    }

    #[test]
    fn overlapping_tasks_use_separate_waves_and_worktrees() {
        let plan = plan_isolation(vec![
            task("first", &["src/lib.rs"]),
            task("second", &["src/lib.rs"]),
        ]);

        assert_eq!(plan.waves.len(), 2);
        assert_eq!(task_ids(&plan.waves[0]), vec!["first"]);
        assert_eq!(task_ids(&plan.waves[1]), vec!["second"]);
        assert!(
            plan.tasks()
                .all(|task| task.isolation == TaskIsolation::Worktree)
        );
    }

    #[test]
    fn parent_and_child_write_paths_overlap() {
        assert!(write_sets_overlap(
            &[PathBuf::from("src")],
            &[PathBuf::from("src/lib.rs")]
        ));
        assert!(!write_sets_overlap(
            &[PathBuf::from("src")],
            &[PathBuf::from("tests/lib.rs")]
        ));
    }

    #[test]
    fn empty_write_sets_are_parallel_and_need_no_worktree() {
        let plan = plan_isolation(vec![task("first", &[]), task("second", &[])]);

        assert_eq!(plan.waves.len(), 1);
        assert_eq!(task_ids(&plan.waves[0]), vec!["first", "second"]);
        assert!(
            plan.tasks()
                .all(|task| task.isolation == TaskIsolation::Shared)
        );
    }

    #[test]
    fn waves_are_deterministic_and_preserve_input_order() {
        let input = vec![
            task("first", &["src/a.rs"]),
            task("second", &["src/b.rs"]),
            task("third", &["src/a.rs"]),
            task("fourth", &["src/b.rs"]),
        ];
        let first = plan_isolation(input.clone());
        let second = plan_isolation(input);

        assert_eq!(first, second);
        assert_eq!(task_ids(&first.waves[0]), vec!["first", "second"]);
        assert_eq!(task_ids(&first.waves[1]), vec!["third", "fourth"]);
        assert_eq!(
            first
                .tasks()
                .map(|planned| planned.task.task_id.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second", "third", "fourth"]
        );
    }

    #[test]
    fn default_and_caller_supplied_summary_limits_are_enforced() {
        let mut returned = valid_return();
        returned.summary = "one\ntwo\nthree".into();
        assert!(returned.validate().is_ok());

        returned.summary.push_str("\nfour");
        assert_eq!(
            returned.validate(),
            Err(ContractError::SummaryTooLong {
                max_lines: 3,
                actual_lines: 4,
            })
        );
        assert_eq!(
            returned.validate_with_summary_limit(2),
            Err(ContractError::SummaryTooLong {
                max_lines: 2,
                actual_lines: 4,
            })
        );
    }

    #[test]
    fn modified_files_tests_and_detail_reference_are_required() {
        let mut returned = valid_return();
        returned.files.clear();
        assert_eq!(returned.validate(), Err(ContractError::MissingFiles));

        let mut returned = valid_return();
        returned.tests.clear();
        assert_eq!(returned.validate(), Err(ContractError::MissingTests));

        let mut returned = valid_return();
        returned.detail_ref = " \t".into();
        assert_eq!(returned.validate(), Err(ContractError::MissingDetailRef));
    }

    #[test]
    fn fallback_provenance_records_assignment_actual_lane_and_redo() {
        let provenance = ResultProvenance::fallback(
            "assigned",
            "actual",
            "assigned",
            "assigned lane exhausted",
            true,
        );
        let result = WorkerResult {
            output: valid_return(),
            provenance: provenance.clone(),
        };

        assert!(result.validate().is_ok());
        assert_eq!(provenance.assigned_lane, "assigned");
        assert_eq!(provenance.actual_lane, "actual");
        let fallback = provenance.fallback.unwrap();
        assert_eq!(fallback.from_lane, "assigned");
        assert_eq!(fallback.reason, "assigned lane exhausted");
        assert!(fallback.redo_from_scratch);
    }

    #[test]
    fn lane_changes_cannot_drop_fallback_provenance() {
        let provenance = ResultProvenance {
            assigned_lane: "assigned".into(),
            actual_lane: "actual".into(),
            fallback: None,
        };

        assert_eq!(
            provenance.validate(),
            Err(ContractError::MissingFallbackProvenance)
        );
        assert!(ResultProvenance::assigned("assigned").validate().is_ok());
    }
}
