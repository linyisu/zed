use std::{
    path::{Path, PathBuf},
    process::ExitStatus,
    time::Duration,
};

use anyhow::Context as _;
use smol::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[derive(Clone, Debug)]
pub struct RustSolution {
    pub root_path: PathBuf,
    pub target_path: PathBuf,
    pub bin_name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SampleCase {
    pub input: String,
    pub expected: String,
}

#[derive(Debug)]
pub struct ProcessOutput {
    pub label: &'static str,
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug)]
pub enum SampleResult {
    Ac {
        actual: String,
        stderr: String,
    },
    Wa {
        actual: String,
        expected: String,
        stderr: String,
    },
    Re {
        stderr: String,
    },
    Tle,
}

#[derive(Debug)]
pub struct SampleTestReport {
    pub build: ProcessOutput,
    pub cases: Vec<(usize, SampleResult)>,
}

enum SampleProcessWait {
    Finished(std::io::Result<SampleProcessOutput>),
    TimedOut,
}

struct SampleProcessOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

pub async fn run_rust_sample_tests(
    solution: RustSolution,
    cases: Vec<SampleCase>,
    timeout: Duration,
) -> anyhow::Result<SampleTestReport> {
    let cargo = find_executable("cargo").ok_or_else(|| {
        anyhow::anyhow!(
            "could not find `cargo` in PATH or common user bin directories. PATH={}",
            std::env::var("PATH").unwrap_or_default()
        )
    })?;
    let build_args = cargo_build_args(&solution);
    let build = run_process(
        "cargo build",
        &solution.root_path,
        &cargo,
        &build_args,
        &solution.target_path,
    )
    .await?;
    if !build.status.success() {
        return Ok(SampleTestReport {
            build,
            cases: Vec::new(),
        });
    }

    let mut results = Vec::with_capacity(cases.len());
    for (index, case) in cases.into_iter().enumerate() {
        let result = run_sample_case(&solution, &cargo, case, timeout).await;
        results.push((index, result));
    }
    Ok(SampleTestReport {
        build,
        cases: results,
    })
}

fn cargo_build_args(solution: &RustSolution) -> Vec<String> {
    let mut args = vec!["build".to_string(), "--release".to_string()];
    if let Some(bin_name) = solution.bin_name.as_deref() {
        args.push("--bin".to_string());
        args.push(bin_name.to_string());
    }
    args
}

fn cargo_run_args(solution: &RustSolution) -> Vec<String> {
    let mut args = vec!["run".to_string(), "--release".to_string(), "-q".to_string()];
    if let Some(bin_name) = solution.bin_name.as_deref() {
        args.push("--bin".to_string());
        args.push(bin_name.to_string());
    }
    args
}

async fn run_process(
    label: &'static str,
    current_dir: &Path,
    executable: &Path,
    args: &[String],
    target_path: &Path,
) -> anyhow::Result<ProcessOutput> {
    let output = smol::process::Command::new(executable)
        .args(args)
        .current_dir(current_dir)
        .env("CARGO_TARGET_DIR", target_path)
        .output()
        .await
        .with_context(|| format!("running {}", executable.display()))?;

    Ok(ProcessOutput {
        label,
        executable: executable.to_path_buf(),
        args: args.to_vec(),
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

async fn run_sample_case(
    solution: &RustSolution,
    cargo: &Path,
    case: SampleCase,
    timeout: Duration,
) -> SampleResult {
    let args = cargo_run_args(solution);
    let mut child = match smol::process::Command::new(cargo)
        .args(&args)
        .current_dir(&solution.root_path)
        .env("CARGO_TARGET_DIR", &solution.target_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return SampleResult::Re {
                stderr: error.to_string(),
            };
        }
    };

    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let input = case.input.into_bytes();
    let write_input = async move {
        if let Some(stdin) = stdin.as_mut()
            && let Err(error) = stdin.write_all(&input).await
            && error.kind() != std::io::ErrorKind::BrokenPipe
        {
            log::debug!("failed to write sample input to stdin: {error}");
        }
        drop(stdin);
    };
    let read_stdout = async move {
        let mut text = String::new();
        if let Some(stdout) = stdout.as_mut() {
            stdout.read_to_string(&mut text).await?;
        }
        std::io::Result::Ok(text)
    };
    let read_stderr = async move {
        let mut text = String::new();
        if let Some(stderr) = stderr.as_mut() {
            stderr.read_to_string(&mut text).await?;
        }
        std::io::Result::Ok(text)
    };

    let output = smol::future::race(
        async {
            let (status, (_, (stdout, stderr))) = smol::future::zip(
                child.status(),
                smol::future::zip(write_input, smol::future::zip(read_stdout, read_stderr)),
            )
            .await;
            SampleProcessWait::Finished(status.and_then(|status| {
                Ok(SampleProcessOutput {
                    status,
                    stdout: stdout?,
                    stderr: stderr?,
                })
            }))
        },
        async {
            smol::Timer::after(timeout).await;
            SampleProcessWait::TimedOut
        },
    )
    .await;

    let output = match output {
        SampleProcessWait::Finished(output) => match output {
            Ok(output) => output,
            Err(error) => {
                return SampleResult::Re {
                    stderr: error.to_string(),
                };
            }
        },
        SampleProcessWait::TimedOut => {
            if let Err(error) = child.kill() {
                log::debug!("failed to kill timed out sample process: {error}");
            }
            return SampleResult::Tle;
        }
    };

    if !output.status.success() {
        return SampleResult::Re {
            stderr: output.stderr,
        };
    }
    if output.stdout.trim_end() == case.expected.trim_end() {
        SampleResult::Ac {
            actual: output.stdout,
            stderr: output.stderr,
        }
    } else {
        SampleResult::Wa {
            actual: output.stdout,
            expected: case.expected,
            stderr: output.stderr,
        }
    }
}

fn find_executable(program: &str) -> Option<PathBuf> {
    let program_path = Path::new(program);
    if program_path.components().count() > 1 && program_path.is_file() {
        return Some(program_path.to_path_buf());
    }

    let mut paths = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let home = PathBuf::from(home);
        paths.push(home.join(".cargo").join("bin"));
        paths.push(home.join(".local").join("bin"));
    }
    paths.push(PathBuf::from("/usr/local/bin"));
    paths.push(PathBuf::from("/usr/bin"));

    paths
        .into_iter()
        .map(|path| path.join(program))
        .find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_arguments_include_selected_binary() {
        let solution = RustSolution {
            root_path: PathBuf::from("."),
            target_path: PathBuf::from("target"),
            bin_name: Some("a".to_string()),
        };
        assert_eq!(
            cargo_build_args(&solution),
            ["build", "--release", "--bin", "a"]
        );
        assert_eq!(
            cargo_run_args(&solution),
            ["run", "--release", "-q", "--bin", "a"]
        );
    }

    #[test]
    fn cargo_arguments_omit_binary_for_default_target() {
        let solution = RustSolution {
            root_path: PathBuf::from("."),
            target_path: PathBuf::from("target"),
            bin_name: None,
        };
        assert_eq!(cargo_build_args(&solution), ["build", "--release"]);
        assert_eq!(cargo_run_args(&solution), ["run", "--release", "-q"]);
    }
}
